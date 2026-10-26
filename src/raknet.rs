//! Bedrock's RakNet server-list ping (UDP), which clients still send for servers added by IP. Geyser in NetherNet
//! mode doesn't answer it, so the front door does, asleep or awake. Other packets are ignored.

use crate::door::Door;
use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::net::UdpSocket;

const MAGIC: [u8; 16] = [0x00, 0xff, 0xff, 0x00, 0xfe, 0xfe, 0xfe, 0xfe, 0xfd, 0xfd, 0xfd, 0xfd, 0x12, 0x34, 0x56, 0x78];

fn plain(text: &str) -> String {
    let mut out = String::new();
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        match c {
            '\u{a7}' => {
                chars.next();
            }
            ';' | '\r' | '\n' => out.push(' '),
            c => out.push(c),
        }
    }
    let out: String = out.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(60).collect();
    if out.is_empty() {
        "Minecraft Server".into()
    } else {
        out
    }
}

fn motd(door: &Door, guid: u64, port: u16) -> String {
    let seen = door.backend.last_seen();
    let bedrock = &seen["bedrock"];
    let (line1, line2, online, max) = match door.backend.cached() {
        Some(up) => {
            let d = &up.json["description"];
            let text = d.as_str().map(str::to_string).unwrap_or_else(|| {
                let mut t = d["text"].as_str().unwrap_or_default().to_string();
                for e in d["extra"].as_array().into_iter().flatten() {
                    t.push_str(e["text"].as_str().unwrap_or_default());
                }
                t
            });
            (
                plain(&text),
                "Online".to_string(),
                up.json["players"]["online"].as_i64().unwrap_or(0),
                up.json["players"]["max"].as_i64().unwrap_or(20),
            )
        }
        None => match door.waker.starting_since() {
            Some(since) => {
                let s = since.elapsed().unwrap_or_default().as_secs();
                (format!("Starting up ({}:{:02}) - join again soon", s / 60, s % 60), "Starting".into(), 0, door.backend.max_players())
            }
            None => ("Sleeping - join to wake it (about 2 min)".into(), "Sleeping".into(), 0, door.backend.max_players()),
        },
    };
    let mode = match bedrock["gameType"].as_i64() {
        Some(1) => "Creative",
        Some(2) => "Adventure",
        _ => "Survival",
    };
    format!(
        "MCPE;{line1};{};{};{online};{max};{guid};{line2};{mode};1;{port};{port};",
        bedrock["protocol"].as_i64().unwrap_or(0),
        bedrock["version"].as_str().unwrap_or_default()
    )
}

pub async fn run(door: Arc<Door>, socket: UdpSocket, port: u16) {
    let guid: u64 = rand::random();
    let mut buf = [0u8; 2048];
    // A small allowance per address (clients ping once per server entry every few seconds), so the port can't
    // be used to flood someone with answers.
    let mut tokens: HashMap<IpAddr, (f64, Instant)> = HashMap::new();
    loop {
        let Ok((n, addr)) = socket.recv_from(&mut buf).await else { continue };
        let data = &buf[..n];
        if n < 33 || !matches!(data[0], 0x01 | 0x02) || data[9..25] != MAGIC {
            continue;
        }
        let now = Instant::now();
        let (t, last) = tokens.get(&addr.ip()).copied().unwrap_or((10.0, now));
        let t = (t + now.duration_since(last).as_secs_f64() * 5.0).min(10.0);
        if t < 1.0 {
            continue;
        }
        tokens.insert(addr.ip(), (t - 1.0, now));
        if tokens.len() > 4096 {
            tokens.retain(|_, (_, at)| now.duration_since(*at) < Duration::from_secs(5));
        }
        if door.backend.stale_for(Duration::from_secs(15)) {
            let b = door.backend.clone();
            tokio::spawn(async move { b.status(Duration::from_secs(3)).await });
        }
        let text = motd(&door, guid, port);
        let mut out = vec![0x1c];
        out.extend_from_slice(&data[1..9]);
        out.extend(guid.to_be_bytes());
        out.extend(MAGIC);
        out.extend((text.len() as u16).to_be_bytes());
        out.extend(text.as_bytes());
        let _ = socket.send_to(&out, SocketAddr::new(addr.ip(), addr.port())).await;
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn plain_strips_codes_and_separators() {
        assert_eq!(super::plain("\u{a7}6Hello;\u{a7}r world\nok"), "Hello world ok");
        assert_eq!(super::plain(""), "Minecraft Server");
    }
}
