//! The real server's status (Minecraft's server-list ping), and what it last reported, kept on disk so the
//! front door can describe the server while it sleeps.

use crate::config::Config;
use crate::proto::{frame, mc_string, read_packet, varint};
use serde_json::{json, Map, Value};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    time::timeout,
};

#[derive(Clone)]
pub struct Status {
    pub raw: String,
    pub json: Value,
}

pub struct Backend {
    cfg: Arc<Config>,
    cache: Mutex<(Option<Instant>, Option<Status>)>,
    refresh: tokio::sync::Mutex<()>,
    last_seen: Mutex<Value>,
    last_seen_path: PathBuf,
    bedrock_checked: Mutex<Option<Instant>>,
}

impl Backend {
    pub fn new(cfg: Arc<Config>) -> Arc<Self> {
        let path = cfg.state_dir.join("last-status.json");
        let last_seen = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
            .filter(Value::is_object)
            .unwrap_or_else(|| json!({}));
        Arc::new(Backend {
            cfg,
            cache: Mutex::new((None, None)),
            refresh: tokio::sync::Mutex::new(()),
            last_seen: Mutex::new(last_seen),
            last_seen_path: path,
            bedrock_checked: Mutex::new(None),
        })
    }

    pub fn last_seen(&self) -> Value {
        self.last_seen.lock().unwrap().clone()
    }

    pub fn protocol(&self) -> i32 {
        self.last_seen()["version"]["protocol"].as_i64().map_or(self.cfg.default_protocol, |p| p as i32)
    }

    pub fn version_name(&self) -> String {
        self.last_seen()["version"]["name"].as_str().map_or_else(|| self.cfg.default_version.clone(), str::to_string)
    }

    pub fn max_players(&self) -> i64 {
        self.last_seen()["maxPlayers"].as_i64().unwrap_or(20)
    }

    pub fn cached(&self) -> Option<Status> {
        self.cache.lock().unwrap().1.clone()
    }

    fn age(&self) -> Option<Duration> {
        self.cache.lock().unwrap().0.map(|t| t.elapsed())
    }

    fn write_last_seen(&self, data: Value) {
        let mut current = self.last_seen.lock().unwrap();
        if *current == data {
            return;
        }
        *current = data.clone();
        drop(current);
        let tmp = self.last_seen_path.with_extension("json.tmp");
        let result = std::fs::create_dir_all(&self.cfg.state_dir)
            .and_then(|_| std::fs::write(&tmp, data.to_string()))
            .and_then(|_| std::fs::rename(&tmp, &self.last_seen_path));
        if let Err(e) = result {
            tracing::warn!("could not save the last status: {e}");
        }
    }

    fn remember_java(&self, status: &Value) {
        let mut keep = Map::new();
        for k in ["version", "favicon"] {
            if let Some(v) = status.get(k) {
                keep.insert(k.into(), v.clone());
            }
        }
        keep.insert("maxPlayers".into(), status["players"]["max"].clone());
        if let Some(b) = self.last_seen().get("bedrock") {
            keep.insert("bedrock".into(), b.clone());
        }
        self.write_last_seen(Value::Object(keep));
    }

    async fn ping(&self) -> anyhow::Result<Status> {
        let host = &self.cfg.backend_host;
        let port = self.cfg.backend_port;
        // Same network: a running server answers in milliseconds, a stopped one never does.
        let mut s = timeout(Duration::from_secs(1), TcpStream::connect((host.as_str(), port))).await??;
        let mut hs = varint(self.protocol());
        hs.extend(mc_string(host));
        hs.extend(port.to_be_bytes());
        hs.extend(varint(1));
        let mut out = frame(0, &hs);
        out.extend(frame(0, &[]));
        s.write_all(&out).await?;
        let mut p = timeout(Duration::from_secs(4), read_packet(&mut s, 256 * 1024, None)).await??;
        if p.id != 0 {
            anyhow::bail!("unexpected status reply");
        }
        let raw = p.data.string(32767)?;
        let json: Value = serde_json::from_str(&raw)?;
        Ok(Status { raw, json })
    }

    /// The status if the real server answers, else None. Results younger than `max_age` are reused.
    pub async fn status(self: &Arc<Self>, max_age: Duration) -> Option<Status> {
        let _guard = self.refresh.lock().await;
        if self.age().is_some_and(|a| a < max_age) {
            return self.cached();
        }
        let result = self.ping().await.ok();
        if let Some(s) = &result {
            self.remember_java(&s.json);
            self.maybe_remember_bedrock();
        }
        *self.cache.lock().unwrap() = (Some(Instant::now()), result.clone());
        result
    }

    /// For server-list pings: recent results straight away (refreshed in the background); after a quiet spell
    /// it checks first, which takes at most a second while the server is off.
    pub async fn status_for_list(self: &Arc<Self>) -> Option<Status> {
        match self.age() {
            Some(a) if a <= Duration::from_secs(15) => {
                if a >= Duration::from_secs(3) {
                    let me = self.clone();
                    tokio::spawn(async move { me.status(Duration::from_secs(3)).await });
                }
                self.cached()
            }
            _ => self.status(Duration::from_secs(3)).await,
        }
    }

    pub fn stale_for(&self, d: Duration) -> bool {
        self.age().map_or(true, |a| a > d)
    }

    fn maybe_remember_bedrock(self: &Arc<Self>) {
        let Some(&(_, port)) = self.cfg.bedrock_ports.first() else { return };
        {
            let mut checked = self.bedrock_checked.lock().unwrap();
            if checked.is_some_and(|t| t.elapsed() < Duration::from_secs(300)) {
                return;
            }
            *checked = Some(Instant::now());
        }
        let me = self.clone();
        tokio::spawn(async move {
            if let Err(e) = me.remember_bedrock(port).await {
                tracing::debug!("could not read Geyser's server info: {e}");
            }
        });
    }

    /// Saves what Geyser tells Bedrock clients (version, protocol, world, ...) to repeat while asleep.
    async fn remember_bedrock(&self, port: u16) -> anyhow::Result<()> {
        let mut s = timeout(Duration::from_secs(2), TcpStream::connect((self.cfg.backend_host.as_str(), port))).await??;
        s.write_all(b"GET /v1/join HTTP/1.0\r\nHost: frontdoor\r\nConnection: close\r\n\r\n").await?;
        let mut raw = Vec::new();
        timeout(Duration::from_secs(5), (&mut s).take(64 * 1024).read_to_end(&mut raw)).await??;
        let text = String::from_utf8_lossy(&raw);
        let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
        if !head.lines().next().unwrap_or_default().contains(" 200 ") {
            return Ok(());
        }
        let info: Value = serde_json::from_str(body)?;
        let Some(obj) = info.as_object().filter(|o| o.get("protocol").is_some_and(Value::is_i64)) else { return Ok(()) };
        let keep: Map<String, Value> = obj
            .iter()
            .filter(|(k, v)| {
                k.as_str() != "players"
                    && k.as_str() != "nonce"
                    && (v.is_string() || v.is_number() || v.is_boolean())
                    && v.to_string().len() <= 200
            })
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let mut seen = self.last_seen();
        seen["bedrock"] = Value::Object(keep);
        self.write_last_seen(seen);
        Ok(())
    }
}
