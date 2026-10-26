//! Minecraft Java Edition: server list, waking on join, holding the player on a loading screen, and moving
//! them to the real server with the Transfer packet (1.20.5+).

use crate::door::{Door, PacketIds};
use crate::proto::{frame, json_text, mc_string, read_packet, varint, Nbt};
use crate::relay::relay;
use crate::wake::{Wake, WakeState};
use anyhow::Result;
use serde_json::json;
use std::{
    sync::Arc,
    time::{Duration, Instant, SystemTime},
};
use tokio::{
    io::AsyncWriteExt,
    net::{tcp::OwnedWriteHalf, TcpStream},
    time::timeout,
};

const LOGIN_DISCONNECT: i32 = 0x00;
const LOGIN_SUCCESS: i32 = 0x02;
const LOGIN_START: i32 = 0x00;
const LOGIN_ACKNOWLEDGED: i32 = 0x03;

fn valid_name(n: &str) -> bool {
    (1..=16).contains(&n.len()) && n.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

fn elapsed(since: SystemTime) -> String {
    let s = since.elapsed().unwrap_or_default().as_secs();
    format!("{}:{:02}", s / 60, s % 60)
}

fn sleeping_status(door: &Door) -> String {
    let text = match door.waker.starting_since() {
        Some(since) => {
            format!("\u{a7}e\u{23f3} Starting up\u{a7}r \u{a7}7({})\n\u{a7}7Join now and you'll get in when it's ready", elapsed(since))
        }
        None => "\u{a7}6\u{263e} Sleeping\u{a7}r - join to start it\n\u{a7}7Starting takes about 2 minutes".into(),
    };
    let seen = door.backend.last_seen();
    let mut status = json!({
        "version": { "name": door.backend.version_name(), "protocol": door.backend.protocol() },
        "players": { "max": door.backend.max_players(), "online": 0 },
        "description": { "text": text },
    });
    if let Some(icon) = seen["favicon"].as_str() {
        status["favicon"] = json!(icon);
    }
    status.to_string()
}

pub async fn handle(door: Arc<Door>, s: TcpStream) {
    let ip = s.peer_addr().map(|a| a.ip().to_string()).unwrap_or_else(|_| "?".into());
    if let Err(e) = serve(&door, s, &ip).await {
        tracing::debug!("java connection from {ip} ended: {e}");
    }
}

async fn serve(door: &Arc<Door>, mut s: TcpStream, ip: &str) -> Result<()> {
    let _ = s.set_nodelay(true);
    let mut first = [0u8; 1];
    timeout(Duration::from_secs(10), s.peek(&mut first)).await??;
    if first[0] == 0xFE {
        return Ok(()); // pre-1.7 server list ping
    }
    let mut hs = timeout(Duration::from_secs(10), read_packet(&mut s, 1024, None)).await??;
    if hs.id != 0 {
        return Ok(());
    }
    let protocol = hs.data.varint()?;
    hs.data.string(255)?;
    hs.data.ushort()?;
    match hs.data.varint()? {
        1 => status(door, &mut s).await,
        2 | 3 => login(door, s, ip, protocol, hs.raw).await,
        _ => Ok(()),
    }
}

async fn status(door: &Arc<Door>, s: &mut TcpStream) -> Result<()> {
    let p = timeout(Duration::from_secs(10), read_packet(s, 64, None)).await??;
    if p.id != 0 {
        return Ok(());
    }
    let reply = match door.backend.status_for_list().await {
        Some(up) => up.raw,
        None => sleeping_status(door),
    };
    s.write_all(&frame(0, &mc_string(&reply))).await?;
    let mut ping = timeout(Duration::from_secs(10), read_packet(s, 64, None)).await??;
    if ping.id == 1 {
        let payload = ping.data.take(8)?.to_vec();
        s.write_all(&frame(1, &payload)).await?;
    }
    Ok(())
}

async fn kick_login(s: &mut TcpStream, message: &str) -> Result<()> {
    s.write_all(&frame(LOGIN_DISCONNECT, &json_text(message))).await?;
    Ok(())
}

async fn login(door: &Arc<Door>, mut s: TcpStream, ip: &str, protocol: i32, handshake: Vec<u8>) -> Result<()> {
    let mut start = timeout(Duration::from_secs(10), read_packet(&mut s, 256, None)).await??;
    if start.id != LOGIN_START {
        return Ok(());
    }
    let name = start.data.string(16)?;
    let player_uuid: [u8; 16] = start.data.take(16)?.try_into()?;
    if !valid_name(&name) {
        return kick_login(&mut s, "That player name isn't valid.").await;
    }
    let ids = door.protocols.get(&protocol).copied();

    if let Some(up) = door.backend.status_for_list().await {
        let server_protocol = up.json["version"]["protocol"].as_i64();
        match ids {
            Some(ids) if server_protocol == Some(protocol as i64) => {
                enter_configuration(&mut s, ids, &name, &player_uuid).await?;
                s.write_all(&transfer(door, ids)).await?;
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            _ => {
                // The real server answers clients this program can't hand over (e.g. other versions).
                let mut first = handshake;
                first.extend(start.raw);
                relay(&mut s, &door.cfg.backend_host, door.cfg.backend_port, &first).await;
            }
        }
        return Ok(());
    }

    if !door.allowed.contains_name(&name).await {
        tracing::info!("{name} ({ip}) tried to wake the server but isn't on the whitelist");
        return kick_login(&mut s, "You are not white-listed on this server!").await;
    }
    if protocol != door.backend.protocol() {
        let msg = format!("This server runs {}. Switch to that version in the launcher and join again.", door.backend.version_name());
        return kick_login(&mut s, &msg).await;
    }
    let wake = door.waker.request(&format!("{name} ({ip})"));
    let Some(ids) = ids else {
        return kick_login(&mut s, "The server is starting up.\n\nJoin again in about 2 minutes.").await;
    };
    hold(door, s, ids, &name, &player_uuid, &wake).await
}

async fn enter_configuration(s: &mut TcpStream, ids: PacketIds, name: &str, player_uuid: &[u8; 16]) -> Result<()> {
    // Offline-style login: the real server does the actual authentication after the transfer.
    let mut payload = player_uuid.to_vec();
    payload.extend(mc_string(name));
    payload.extend(varint(0));
    if ids.login_success_session_id {
        payload.extend(uuid::Uuid::new_v4().as_bytes());
    }
    s.write_all(&frame(LOGIN_SUCCESS, &payload)).await?;
    loop {
        let p = timeout(Duration::from_secs(15), read_packet(s, 4096, None)).await??;
        if p.id == LOGIN_ACKNOWLEDGED {
            return Ok(());
        }
    }
}

fn transfer(door: &Door, ids: PacketIds) -> Vec<u8> {
    let mut payload = mc_string(&door.cfg.transfer_host);
    payload.extend(varint(door.cfg.transfer_port as i32));
    frame(ids.config_transfer, &payload)
}

fn dialog(ids: PacketIds, wake: &Wake) -> Vec<u8> {
    let body = format!(
        "The server was asleep and is starting up for you.\n\nThis usually takes about 2 minutes. Stay on this screen: \
         you'll join automatically when it's ready.\n\nWaiting: {}",
        elapsed(wake.started)
    );
    let nbt = Nbt::Compound(vec![
        ("type".into(), Nbt::Str("minecraft:notice".into())),
        ("title".into(), Nbt::Str("Starting the server".into())),
        (
            "body".into(),
            Nbt::List(vec![Nbt::Compound(vec![
                ("type".into(), Nbt::Str("minecraft:plain_message".into())),
                ("contents".into(), Nbt::Str(body)),
                ("width".into(), Nbt::Int(300)),
            ])]),
        ),
        ("can_close_with_escape".into(), Nbt::Bool(true)),
        ("pause".into(), Nbt::Bool(false)),
        ("after_action".into(), Nbt::Str("close".into())),
    ]);
    frame(ids.config_show_dialog, &nbt.encode())
}

async fn hold(door: &Arc<Door>, mut s: TcpStream, ids: PacketIds, name: &str, player_uuid: &[u8; 16], wake: &Arc<Wake>) -> Result<()> {
    enter_configuration(&mut s, ids, name, player_uuid).await?;
    let (reader, writer) = s.into_split();
    hold_on(door, reader, writer, ids, name, wake).await
}

async fn hold_on(
    door: &Door,
    mut reader: tokio::net::tcp::OwnedReadHalf,
    mut writer: OwnedWriteHalf,
    ids: PacketIds,
    name: &str,
    wake: &Arc<Wake>,
) -> Result<()> {
    // Read (and ignore) whatever the client sends; a read error means it left.
    let gone = tokio::spawn(async move { while read_packet(&mut reader, 64 * 1024, None).await.is_ok() {} });
    let mut state = wake.subscribe();
    let (mut last_keep_alive, mut last_dialog) = (None::<Instant>, None::<Instant>);
    let result = loop {
        if gone.is_finished() {
            tracing::info!("{name} left while waiting");
            break Ok(());
        }
        let current = state.borrow().clone();
        match current {
            WakeState::Ready => {
                writer.write_all(&transfer(door, ids)).await?;
                tracing::info!("transferred {name}");
                tokio::time::sleep(Duration::from_secs(1)).await;
                break Ok(());
            }
            WakeState::Failed(reason) => {
                writer.write_all(&frame(ids.config_disconnect, &Nbt::Str(reason).encode())).await?;
                break Ok(());
            }
            WakeState::Pending => {}
        }
        if last_keep_alive.map_or(true, |t| t.elapsed() >= Duration::from_secs(10)) {
            writer.write_all(&frame(ids.config_keep_alive, &rand::random::<i64>().abs().to_be_bytes())).await?;
            last_keep_alive = Some(Instant::now());
        }
        if last_dialog.map_or(true, |t| t.elapsed() >= Duration::from_secs(15)) {
            writer.write_all(&dialog(ids, wake)).await?;
            last_dialog = Some(Instant::now());
        }
        let _ = timeout(Duration::from_secs(1), state.changed()).await;
    };
    gone.abort();
    result
}
