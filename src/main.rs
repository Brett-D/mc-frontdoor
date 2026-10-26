//! mc-frontdoor: answers for a Minecraft server while it sleeps, wakes it when a whitelisted player joins, and
//! sends players on to it. See README.md.

mod allow;
mod auth;
mod backend;
mod bedrock;
mod config;
mod door;
mod java;
mod proto;
mod raknet;
mod relay;
mod wake;

use anyhow::{Context, Result};
use std::{future::Future, sync::Arc};
use tokio::net::{TcpListener, UdpSocket};

fn accept_loop<F, Fut>(door: Arc<door::Door>, listener: TcpListener, handler: F)
where
    F: Fn(Arc<door::Door>, tokio::net::TcpStream) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    tokio::spawn(async move {
        loop {
            let Ok((stream, addr)) = listener.accept().await else { continue };
            let Some(slot) = door.admit(addr.ip()) else { continue };
            let fut = handler(door.clone(), stream);
            tokio::spawn(async move {
                fut.await;
                drop(slot);
            });
        }
    });
}

#[tokio::main]
async fn main() -> Result<()> {
    let level = std::env::var("LOG_LEVEL").unwrap_or_else(|_| "info".into()).to_lowercase();
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new(format!("warn,mc_frontdoor={level}")))
        .with_target(false)
        .with_ansi(false)
        .without_time()
        .init();

    let cfg = config::Config::from_env()?;
    let door = door::Door::new(cfg)?;
    let c = door.cfg.clone();
    if !door.allowed.has_sources() {
        tracing::warn!("no ALLOW_FILE or ALLOW_URL set, and ALLOW_ANYONE is off: nobody can wake the server");
    }

    let bind = |port: u16| format!("{}:{port}", c.listen_host);
    accept_loop(door.clone(), TcpListener::bind(bind(c.listen_port)).await.context("binding the Java port")?, java::handle);
    for &(listen, server) in &c.relay_ports {
        let l = TcpListener::bind(bind(listen)).await.with_context(|| format!("binding relay port {listen}"))?;
        accept_loop(door.clone(), l, move |d, s| relay::handle(d, s, listen, server));
    }
    let fwd = bedrock::forwarder();
    for &(listen, server) in &c.bedrock_ports {
        let l = TcpListener::bind(bind(listen)).await.with_context(|| format!("binding Bedrock port {listen}"))?;
        let f = fwd.clone();
        accept_loop(door.clone(), l, move |d, s| bedrock::handle(d, s, server, f.clone()));
        let udp = UdpSocket::bind(bind(listen)).await.with_context(|| format!("binding Bedrock UDP port {listen}"))?;
        tokio::spawn(raknet::run(door.clone(), udp, listen));
    }

    let pairs = |p: &[(u16, u16)]| p.iter().map(|(a, b)| format!("{a}->{b}")).collect::<Vec<_>>().join(", ");
    tracing::info!(
        "mc-frontdoor {} listening on {} (relaying {}; bedrock {}{}) for {}:{}",
        env!("CARGO_PKG_VERSION"),
        c.listen_port,
        if c.relay_ports.is_empty() { "nothing".into() } else { pairs(&c.relay_ports) },
        if c.bedrock_ports.is_empty() { "off".into() } else { pairs(&c.bedrock_ports) },
        if door.tls.is_some() { " with HTTPS" } else { "" },
        c.backend_host,
        c.backend_port
    );
    shutdown().await;
    Ok(())
}

/// Ctrl-C, or SIGTERM from systemd/Docker (as PID 1 in a container, unhandled signals are ignored).
async fn shutdown() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = term.recv() => {}
                }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
