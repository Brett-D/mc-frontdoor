//! Plain TCP relays to the server (e.g. a web map), with a "map is asleep" page on the map port while it's off.

use crate::door::Door;
use std::{sync::Arc, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    time::timeout,
};

pub async fn relay(client: &mut TcpStream, host: &str, port: u16, first: &[u8]) -> bool {
    let Ok(Ok(mut up)) = timeout(Duration::from_secs(5), TcpStream::connect((host, port))).await else {
        return false;
    };
    let _ = up.set_nodelay(true);
    if !first.is_empty() && up.write_all(first).await.is_err() {
        return true;
    }
    let _ = tokio::io::copy_bidirectional(client, &mut up).await;
    true
}

pub async fn handle(door: Arc<Door>, mut client: TcpStream, listen_port: u16, server_port: u16) {
    if relay(&mut client, &door.cfg.backend_host, server_port, &[]).await || listen_port != door.cfg.map_port {
        return;
    }
    let mut buf = [0u8; 8192];
    let _ = timeout(Duration::from_secs(2), client.read(&mut buf)).await;
    let page = "<!doctype html><meta charset=utf-8><meta name=viewport content='width=device-width'>\
        <title>Map offline</title><body style='font-family:system-ui;background:#10161c;color:#e8eef3;\
        display:flex;align-items:center;justify-content:center;height:90vh;text-align:center'>\
        <div><h1>The map is asleep</h1><p>The Minecraft server is switched off while nobody plays.<br>\
        Join the server to start it, then reload this page in a few minutes.</p></div>";
    let reply = format!(
        "HTTP/1.1 503 Service Unavailable\r\nContent-Type: text/html; charset=utf-8\r\nCache-Control: no-store\r\n\
         Retry-After: 120\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{page}",
        page.len()
    );
    let _ = client.write_all(reply.as_bytes()).await;
}
