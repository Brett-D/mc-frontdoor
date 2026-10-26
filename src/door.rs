//! State shared by all connections, and per-address connection limits.

use crate::{allow::Allowed, auth::Auth, backend::Backend, config::Config, wake::Waker};
use anyhow::{Context, Result};
use serde::Deserialize;
use std::{
    collections::HashMap,
    net::IpAddr,
    sync::{Arc, Mutex},
};
use tokio_rustls::{rustls, TlsAcceptor};

const MAX_CONNECTIONS: usize = 200;
const MAX_PER_IP: usize = 8;

/// Packet ids the front door needs in the configuration state. They move between Minecraft versions; see
/// https://minecraft.wiki/w/Java_Edition_protocol/Packets (a packet's id is its position in its list there).
#[derive(Clone, Copy, Debug, Deserialize)]
pub struct PacketIds {
    pub config_disconnect: i32,
    pub config_keep_alive: i32,
    pub config_transfer: i32,
    pub config_show_dialog: i32,
    /// Login Success carries a session id after the game profile.
    #[serde(default = "yes")]
    pub login_success_session_id: bool,
}

fn yes() -> bool {
    true
}

fn builtin_protocols() -> HashMap<i32, PacketIds> {
    HashMap::from([(
        777, // Minecraft 26.3
        PacketIds {
            config_disconnect: 0x02,
            config_keep_alive: 0x04,
            config_transfer: 0x0C,
            config_show_dialog: 0x13,
            login_success_session_id: true,
        },
    )])
}

pub struct Door {
    pub cfg: Arc<Config>,
    pub backend: Arc<Backend>,
    pub allowed: Allowed,
    pub waker: Arc<Waker>,
    pub auth: Auth,
    pub tls: Option<TlsAcceptor>,
    pub protocols: HashMap<i32, PacketIds>,
    limits: Mutex<(usize, HashMap<IpAddr, usize>)>,
}

/// Holds a connection slot; released when dropped.
pub struct Slot {
    door: Arc<Door>,
    ip: IpAddr,
}

impl Drop for Slot {
    fn drop(&mut self) {
        let mut l = self.door.limits.lock().unwrap();
        l.0 -= 1;
        if let Some(n) = l.1.get_mut(&self.ip) {
            *n -= 1;
            if *n == 0 {
                l.1.remove(&self.ip);
            }
        }
    }
}

fn load_tls(cfg: &Config) -> Result<Option<TlsAcceptor>> {
    let (Some(cert), Some(key)) = (&cfg.tls_cert, &cfg.tls_key) else { return Ok(None) };
    if !cert.is_file() || !key.is_file() {
        return Ok(None);
    }
    let certs = rustls_pemfile::certs(&mut std::io::BufReader::new(std::fs::File::open(cert)?)).collect::<Result<Vec<_>, _>>()?;
    let key = rustls_pemfile::private_key(&mut std::io::BufReader::new(std::fs::File::open(key)?))?.context("no private key found")?;
    let mut server = rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_single_cert(certs, key)?;
    // Bedrock clients offer HTTP/2 and HTTP/1.1; both are served.
    server.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(Some(TlsAcceptor::from(Arc::new(server))))
}

impl Door {
    pub fn new(cfg: Config) -> Result<Arc<Self>> {
        let cfg = Arc::new(cfg);
        let http = reqwest::Client::builder().user_agent(concat!("mc-frontdoor/", env!("CARGO_PKG_VERSION"))).build()?;
        let backend = Backend::new(cfg.clone());
        let mut protocols = builtin_protocols();
        if let Some(file) = &cfg.protocols_file {
            let extra: HashMap<String, PacketIds> = serde_json::from_slice(&std::fs::read(file)?).context("reading PROTOCOLS_FILE")?;
            for (k, v) in extra {
                protocols.insert(k.parse().context("PROTOCOLS_FILE keys must be protocol numbers")?, v);
            }
        }
        Ok(Arc::new(Door {
            tls: load_tls(&cfg).context("loading the TLS certificate")?,
            allowed: Allowed::new(cfg.clone(), http.clone()),
            waker: Waker::new(cfg.clone(), http.clone(), backend.clone()),
            auth: Auth::new(cfg.clone(), http),
            backend,
            protocols,
            limits: Mutex::new((0, HashMap::new())),
            cfg,
        }))
    }

    pub fn admit(self: &Arc<Self>, ip: IpAddr) -> Option<Slot> {
        let mut l = self.limits.lock().unwrap();
        let per = l.1.get(&ip).copied().unwrap_or(0);
        if l.0 >= MAX_CONNECTIONS || per >= MAX_PER_IP {
            return None;
        }
        l.0 += 1;
        l.1.insert(ip, per + 1);
        Some(Slot { door: self.clone(), ip })
    }
}
