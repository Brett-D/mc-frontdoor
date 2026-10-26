//! Minecraft Bedrock via Geyser's NetherNet signaling (HTTP or HTTPS on TCP, HTTP/1.1 or HTTP/2).
//!
//! While the server is up, requests are forwarded to Geyser (with X-Forwarded-For), which tells the player's
//! device to connect straight to the game server for gameplay. While it's asleep, the front door answers the
//! server list itself, and a whitelisted player who tries to join wakes the server. Bedrock can't be kept
//! waiting, so their join fails and they join again once it's up.

use crate::auth::{floodgate_uuid, identity_token};
use crate::door::Door;
use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper::{body::Incoming, header, Method, Request, Response, StatusCode};
use hyper_util::{
    client::legacy::{connect::HttpConnector, Client},
    rt::{TokioExecutor, TokioIo},
    server::conn::auto,
};
use serde_json::{json, Value};
use std::{convert::Infallible, sync::Arc, time::Duration};
use tokio::{io::AsyncWriteExt, net::TcpStream, time::timeout};

pub type Forwarder = Client<HttpConnector, Full<Bytes>>;

pub fn forwarder() -> Forwarder {
    Client::builder(TokioExecutor::new()).pool_idle_timeout(Duration::from_secs(30)).build_http()
}

const HOP_BY_HOP: [&str; 8] = ["connection", "keep-alive", "proxy-connection", "transfer-encoding", "upgrade", "te", "trailer", "host"];

pub async fn handle(door: Arc<Door>, mut tcp: TcpStream, server_port: u16, fwd: Forwarder) {
    let ip = tcp.peer_addr().map(|a| a.ip().to_string()).unwrap_or_else(|_| "?".into());
    let mut first = [0u8; 1];
    if !matches!(timeout(Duration::from_secs(10), tcp.peek(&mut first)).await, Ok(Ok(1))) {
        return;
    }
    if first[0] == 0x16 {
        let Some(acceptor) = door.tls.clone() else {
            if door.backend.status_for_list().await.is_some() {
                crate::relay::relay(&mut tcp, &door.cfg.backend_host, server_port, &[]).await;
            } else {
                // HTTPS without a certificate: clients reaching the server by name try HTTPS first and fall back
                // to plain HTTP when it fails. Fail it at once (handshake_failure alert).
                tracing::info!("bedrock HTTPS attempt from {ip} turned away (no certificate) so it falls back to HTTP");
                let _ = tcp.write_all(&[0x15, 0x03, 0x01, 0x00, 0x02, 0x02, 0x28]).await;
            }
            return;
        };
        match timeout(Duration::from_secs(10), acceptor.accept(tcp)).await {
            Ok(Ok(tls)) => {
                let alpn = tls.get_ref().1.alpn_protocol().map(|p| String::from_utf8_lossy(p).into_owned());
                tracing::debug!("bedrock HTTPS from {ip} ({})", alpn.as_deref().unwrap_or("no ALPN"));
                serve(door, TokioIo::new(tls), ip, true, server_port, fwd).await;
            }
            Ok(Err(e)) => tracing::info!("bedrock HTTPS handshake with {ip} failed: {e}"),
            Err(_) => tracing::info!("bedrock HTTPS handshake with {ip} timed out"),
        }
    } else {
        serve(door, TokioIo::new(tcp), ip, false, server_port, fwd).await;
    }
}

async fn serve<I>(door: Arc<Door>, io: TokioIo<I>, ip: String, secure: bool, server_port: u16, fwd: Forwarder)
where
    I: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let svc = hyper::service::service_fn(move |req: Request<Incoming>| {
        let (door, ip, fwd) = (door.clone(), ip.clone(), fwd.clone());
        async move { Ok::<_, Infallible>(respond(door, &ip, secure, server_port, fwd, req).await) }
    });
    let _ = auto::Builder::new(TokioExecutor::new()).serve_connection(io, svc).await;
}

fn reply(status: StatusCode, content_type: &str, body: impl Into<Bytes>) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CACHE_CONTROL, "no-store")
        .body(Full::new(body.into()))
        .unwrap()
}

async fn respond(
    door: Arc<Door>,
    ip: &str,
    secure: bool,
    server_port: u16,
    fwd: Forwarder,
    req: Request<Incoming>,
) -> Response<Full<Bytes>> {
    let scheme = if secure { "HTTPS" } else { "HTTP" };
    tracing::debug!("bedrock {} {} over {scheme} ({:?}) from {ip}", req.method(), req.uri().path(), req.version());
    if door.backend.status_for_list().await.is_some() {
        return forward(&door, ip, secure, server_port, &fwd, req).await;
    }
    asleep(&door, ip, req).await
}

async fn forward(door: &Door, ip: &str, secure: bool, server_port: u16, fwd: &Forwarder, req: Request<Incoming>) -> Response<Full<Bytes>> {
    let (parts, body) = req.into_parts();
    let Ok(body) = Limited::new(body, 256 * 1024).collect().await.map(|b| b.to_bytes()) else {
        return reply(StatusCode::PAYLOAD_TOO_LARGE, "text/plain", "");
    };
    let host = parts
        .uri
        .authority()
        .map(|a| a.to_string())
        .or_else(|| parts.headers.get(header::HOST).and_then(|h| h.to_str().ok()).map(str::to_string));
    let path = parts.uri.path_and_query().map_or("/", |p| p.as_str());
    let mut b = Request::builder().method(parts.method.clone()).uri(format!("http://{}:{server_port}{path}", door.cfg.backend_host));
    for (k, v) in &parts.headers {
        if !HOP_BY_HOP.contains(&k.as_str()) {
            b = b.header(k, v);
        }
    }
    if let Some(h) = host {
        b = b.header(header::HOST, h);
    }
    b = b.header("x-forwarded-for", ip).header("x-forwarded-proto", if secure { "https" } else { "http" });
    let Ok(up_req) = b.body(Full::new(body)) else { return reply(StatusCode::BAD_REQUEST, "text/plain", "") };
    match timeout(Duration::from_secs(20), fwd.request(up_req)).await {
        Ok(Ok(resp)) => {
            let (parts, body) = resp.into_parts();
            let body = Limited::new(body, 1024 * 1024).collect().await.map(|b| b.to_bytes()).unwrap_or_default();
            let mut out = Response::builder().status(parts.status);
            for (k, v) in &parts.headers {
                if !HOP_BY_HOP.contains(&k.as_str()) && k != header::CONTENT_LENGTH {
                    out = out.header(k, v);
                }
            }
            out.body(Full::new(body)).unwrap_or_else(|_| reply(StatusCode::BAD_GATEWAY, "text/plain", ""))
        }
        _ => reply(StatusCode::BAD_GATEWAY, "text/plain", "The server didn't answer."),
    }
}

/// The server-list answer while asleep: Geyser's last answer with the name and live fields replaced.
fn info(door: &Door) -> Value {
    let name = match door.waker.starting_since() {
        Some(since) => {
            let s = since.elapsed().unwrap_or_default().as_secs();
            format!("\u{a7}eStarting up ({}:{:02}) - join again soon", s / 60, s % 60)
        }
        None => "\u{a7}6Sleeping\u{a7}r - join to wake it (about 2 min)".into(),
    };
    let mut info = json!({ "protocol": 0, "version": "", "level": "world", "gameType": 0, "maxPlayers": door.backend.max_players() });
    if let Some(seen) = door.backend.last_seen()["bedrock"].as_object() {
        for (k, v) in seen {
            info[k] = v.clone();
        }
    }
    info["name"] = json!(name);
    info["players"] = json!(0);
    info["nonce"] = json!(format!("{:016x}", rand::random::<u64>()));
    info
}

async fn asleep(door: &Door, ip: &str, req: Request<Incoming>) -> Response<Full<Bytes>> {
    let path = req.uri().path().to_string();
    if path == "/v1/join" {
        if req.method() != Method::GET {
            return reply(StatusCode::METHOD_NOT_ALLOWED, "text/plain", "");
        }
        return reply(StatusCode::OK, "application/json", info(door).to_string());
    }
    if !path.starts_with("/v1/join/") || req.method() != Method::POST {
        return reply(StatusCode::NOT_FOUND, "text/plain", "");
    }
    tracing::info!("bedrock join attempt from {ip}");
    let Ok(body) = Limited::new(req.into_body(), 256 * 1024).collect().await.map(|b| b.to_bytes()) else {
        return reply(StatusCode::PAYLOAD_TOO_LARGE, "text/plain", "");
    };
    // The offer carries the player's auth token (the Xbox account they're signed in with).
    let checked = async {
        let token = identity_token(&String::from_utf8_lossy(&body)).ok_or_else(|| anyhow::anyhow!("no identity in the join request"))?;
        let claims = door.auth.verify(&token).await?;
        claims["xid"].as_str().and_then(|x| x.parse::<u64>().ok()).ok_or_else(|| anyhow::anyhow!("no Xbox id in the token"))
    };
    let xuid = match checked.await {
        Ok(x) => x,
        Err(e) => {
            tracing::info!("bedrock join from {ip} rejected: {e}");
            return reply(StatusCode::FORBIDDEN, "text/plain", "Could not verify your account.");
        }
    };
    let player = floodgate_uuid(xuid);
    if !door.allowed.contains_uuid(&player).await {
        tracing::info!("bedrock player {player} ({ip}) tried to wake the server but isn't on the whitelist");
        return reply(StatusCode::FORBIDDEN, "text/plain", "You are not white-listed on this server.");
    }
    door.waker.request(&format!("bedrock player {player} ({ip})"));
    reply(StatusCode::SERVICE_UNAVAILABLE, "text/plain", "The server is starting up. Join again in about 2 minutes.")
}
