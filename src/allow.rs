//! Who may wake the server: player names and UUIDs from JSON files (such as the server's own whitelist.json
//! and ops.json) and/or a URL. Java players are matched by name, Bedrock players by their Floodgate UUID.

use crate::config::Config;
use serde_json::Value;
use std::{
    collections::HashSet,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

pub struct Allowed {
    cfg: Arc<Config>,
    http: reqwest::Client,
    keys: Mutex<HashSet<String>>,
    loaded: Mutex<Option<Instant>>,
    refresh: tokio::sync::Mutex<()>,
}

fn valid_name(n: &str) -> bool {
    let n = n.strip_prefix('.').unwrap_or(n);
    (1..=16).contains(&n.len()) && n.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

fn add(keys: &mut HashSet<String>, entry: &Value) {
    let name = entry.as_str().or_else(|| entry["name"].as_str());
    if let Some(n) = name.filter(|n| valid_name(n)) {
        keys.insert(format!("name:{}", n.to_lowercase()));
    }
    if let Some(u) = entry["uuid"].as_str().filter(|u| u.len() == 36 && u.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')) {
        keys.insert(format!("uuid:{}", u.to_lowercase()));
    }
}

fn add_list(keys: &mut HashSet<String>, data: &Value) {
    let list = data.as_array().or_else(|| data["players"].as_array());
    for entry in list.into_iter().flatten() {
        add(keys, entry);
    }
}

impl Allowed {
    pub fn new(cfg: Arc<Config>, http: reqwest::Client) -> Self {
        let cached = std::fs::read(cfg.state_dir.join("allowed.json"))
            .ok()
            .and_then(|b| serde_json::from_slice::<Vec<String>>(&b).ok())
            .map(|v| v.into_iter().filter(|k| k.starts_with("name:") || k.starts_with("uuid:")).collect())
            .unwrap_or_default();
        Allowed { cfg, http, keys: Mutex::new(cached), loaded: Mutex::new(None), refresh: tokio::sync::Mutex::new(()) }
    }

    pub fn has_sources(&self) -> bool {
        self.cfg.allow_anyone || !self.cfg.allow_files.is_empty() || self.cfg.allow_url.is_some()
    }

    async fn fetch(&self) -> anyhow::Result<HashSet<String>> {
        let mut keys = HashSet::new();
        for file in &self.cfg.allow_files {
            let data: Value = serde_json::from_slice(&tokio::fs::read(file).await?)?;
            add_list(&mut keys, &data);
        }
        if let Some(url) = &self.cfg.allow_url {
            let mut req = self.http.get(url);
            for (k, v) in &self.cfg.allow_url_headers {
                req = req.header(k, v);
            }
            let data: Value = req.send().await?.error_for_status()?.json().await?;
            add_list(&mut keys, &data);
        }
        Ok(keys)
    }

    async fn refresh(&self) {
        let _guard = self.refresh.lock().await;
        if self.loaded.lock().unwrap().is_some_and(|t| t.elapsed() < Duration::from_secs(60)) {
            return;
        }
        match self.fetch().await {
            Ok(fresh) => {
                let changed = *self.keys.lock().unwrap() != fresh;
                if changed {
                    let mut list: Vec<_> = fresh.iter().cloned().collect();
                    list.sort();
                    let _ = std::fs::create_dir_all(&self.cfg.state_dir);
                    let _ = std::fs::write(self.cfg.state_dir.join("allowed.json"), serde_json::to_string(&list).unwrap());
                    *self.keys.lock().unwrap() = fresh;
                }
                *self.loaded.lock().unwrap() = Some(Instant::now());
            }
            Err(e) => {
                tracing::warn!("could not refresh the whitelist (keeping {} entries): {e}", self.keys.lock().unwrap().len());
                // Try again in 15 seconds rather than a minute.
                *self.loaded.lock().unwrap() = Some(Instant::now() - Duration::from_secs(45));
            }
        }
    }

    pub async fn contains_name(&self, name: &str) -> bool {
        if self.cfg.allow_anyone {
            return true;
        }
        self.refresh().await;
        self.keys.lock().unwrap().contains(&format!("name:{}", name.to_lowercase()))
    }

    pub async fn contains_uuid(&self, uuid: &str) -> bool {
        if self.cfg.allow_anyone {
            return true;
        }
        self.refresh().await;
        self.keys.lock().unwrap().contains(&format!("uuid:{}", uuid.to_lowercase()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_whitelist_formats() {
        let mut keys = HashSet::new();
        add_list(
            &mut keys,
            &serde_json::json!(["Alex_Builder", {"name": ".BedrockBea", "uuid": "00000000-0000-0000-0009-012345ABCDEF"}, "bad name!"]),
        );
        add_list(&mut keys, &serde_json::json!({"players": [{"name": "steve42"}]}));
        assert!(keys.contains("name:alex_builder"));
        assert!(keys.contains("name:.bedrockbea"));
        assert!(keys.contains("uuid:00000000-0000-0000-0009-012345abcdef"));
        assert!(keys.contains("name:steve42"));
        assert_eq!(keys.len(), 4);
    }
}
