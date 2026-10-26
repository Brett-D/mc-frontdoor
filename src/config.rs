//! Settings, read from environment variables so the same binary works under systemd (EnvironmentFile=),
//! in Docker, or from a shell. See README.md for the full list.

use anyhow::{bail, Context, Result};
use std::{env, path::PathBuf, time::Duration};

#[derive(Clone, Debug)]
pub enum WakeMethod {
    /// The game-server portal from the companion template: POST /api/start, GET /api/status.
    Portal { url: String, key: String, game: String },
    /// Any URL (webhook); the front door then waits for the server to answer.
    Http { url: String, method: String, headers: Vec<(String, String)>, body: Option<String> },
    /// Start an Azure VM with a managed identity; the game server must start with the VM.
    AzureVm { vm_id: String, client_id: Option<String> },
}

#[derive(Clone, Debug)]
pub struct Config {
    pub listen_host: String,
    pub listen_port: u16,
    pub backend_host: String,
    pub backend_port: u16,
    pub transfer_host: String,
    pub transfer_port: u16,
    pub relay_ports: Vec<(u16, u16)>,
    pub map_port: u16,
    pub bedrock_ports: Vec<(u16, u16)>,
    pub auth_discovery: String,
    pub auth_jwks_file: Option<PathBuf>,
    pub auth_issuer: Option<String>,
    pub tls_cert: Option<PathBuf>,
    pub tls_key: Option<PathBuf>,
    pub wake: WakeMethod,
    pub allow_files: Vec<PathBuf>,
    pub allow_url: Option<String>,
    pub allow_url_headers: Vec<(String, String)>,
    pub allow_anyone: bool,
    pub wake_timeout: Duration,
    pub state_dir: PathBuf,
    pub default_version: String,
    pub default_protocol: i32,
    pub protocols_file: Option<PathBuf>,
}

fn opt(name: &str) -> Option<String> {
    env::var(name).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

fn req(name: &str) -> Result<String> {
    opt(name).with_context(|| format!("{name} is not set"))
}

fn num<T: std::str::FromStr>(name: &str, default: T) -> Result<T> {
    match opt(name) {
        None => Ok(default),
        Some(v) => v.parse().map_err(|_| anyhow::anyhow!("{name} must be a number, got {v:?}")),
    }
}

/// "8100 19132" or "18100:8100" (listen:server) pairs.
pub fn port_pairs(spec: &str) -> Result<Vec<(u16, u16)>> {
    spec.split_whitespace()
        .map(|p| {
            let mut parts = p.split(':');
            let listen: u16 = parts.next().unwrap_or_default().parse().with_context(|| format!("bad port {p:?}"))?;
            let server = match parts.next() {
                Some(s) => s.parse().with_context(|| format!("bad port {p:?}"))?,
                None => listen,
            };
            Ok((listen, server))
        })
        .collect()
}

/// "Name: value; Other: value" headers.
pub fn headers(spec: &str) -> Result<Vec<(String, String)>> {
    spec.split(';')
        .map(str::trim)
        .filter(|h| !h.is_empty())
        .map(|h| {
            let (k, v) = h.split_once(':').with_context(|| format!("header {h:?} must look like 'Name: value'"))?;
            Ok((k.trim().to_string(), v.trim().to_string()))
        })
        .collect()
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let credentials = opt("CREDENTIALS_DIRECTORY").map(PathBuf::from);
        let tls_cert = opt("TLS_CERT_FILE").map(PathBuf::from).or_else(|| credentials.as_ref().map(|d| d.join("tls.crt")));
        let tls_key = opt("TLS_KEY_FILE").map(PathBuf::from).or_else(|| credentials.as_ref().map(|d| d.join("tls.key")));

        let method = opt("WAKE_METHOD").unwrap_or_else(|| if opt("PORTAL_URL").is_some() { "portal".into() } else { String::new() });
        let wake = match method.as_str() {
            "portal" => WakeMethod::Portal {
                url: req("PORTAL_URL")?.trim_end_matches('/').to_string(),
                key: req("PORTAL_KEY")?,
                game: opt("GAME").unwrap_or_else(|| "minecraft".into()),
            },
            "http" => WakeMethod::Http {
                url: req("WAKE_URL")?,
                method: opt("WAKE_HTTP_METHOD").unwrap_or_else(|| "POST".into()).to_uppercase(),
                headers: headers(&opt("WAKE_HEADERS").unwrap_or_default())?,
                body: opt("WAKE_BODY"),
            },
            "azure_vm" => WakeMethod::AzureVm { vm_id: req("AZURE_VM_ID")?, client_id: opt("AZURE_CLIENT_ID") },
            "" => bail!("set WAKE_METHOD to portal, http or azure_vm"),
            other => bail!("unknown WAKE_METHOD {other:?} (use portal, http or azure_vm)"),
        };

        let default_state = if cfg!(windows) { "state" } else { "/var/lib/mc-frontdoor" };
        if opt("AUTH_JWKS_FILE").is_some() && opt("AUTH_ISSUER").is_none() {
            bail!("AUTH_JWKS_FILE needs AUTH_ISSUER too, so tokens' issuer is still checked");
        }
        Ok(Config {
            listen_host: opt("LISTEN_HOST").unwrap_or_else(|| "0.0.0.0".into()),
            listen_port: num("LISTEN_PORT", 25565)?,
            backend_host: req("BACKEND_HOST")?,
            backend_port: num("BACKEND_PORT", 25565)?,
            transfer_host: req("TRANSFER_HOST")?,
            transfer_port: num("TRANSFER_PORT", 25565)?,
            relay_ports: port_pairs(&opt("RELAY_PORTS").unwrap_or_default())?,
            map_port: num("MAP_PORT", 8100)?,
            bedrock_ports: port_pairs(&opt("BEDROCK_PORTS").unwrap_or_default())?,
            auth_discovery: opt("AUTH_DISCOVERY")
                .unwrap_or_else(|| "https://client.discovery.minecraft-services.net/api/v1.0/discovery/MinecraftPE/builds/1.0.0.0".into()),
            auth_jwks_file: opt("AUTH_JWKS_FILE").map(PathBuf::from),
            auth_issuer: opt("AUTH_ISSUER"),
            tls_cert,
            tls_key,
            wake,
            allow_files: opt("ALLOW_FILE").unwrap_or_default().split_whitespace().map(PathBuf::from).collect(),
            allow_url: opt("ALLOW_URL"),
            allow_url_headers: headers(&opt("ALLOW_URL_HEADERS").unwrap_or_default())?,
            allow_anyone: matches!(opt("ALLOW_ANYONE").as_deref(), Some("true" | "1" | "yes")),
            wake_timeout: Duration::from_secs(num("WAKE_TIMEOUT", 600)?),
            state_dir: PathBuf::from(opt("STATE_DIRECTORY").unwrap_or_else(|| default_state.into())),
            default_version: opt("DEFAULT_VERSION").unwrap_or_else(|| "Paper 26.3".into()),
            default_protocol: num("DEFAULT_PROTOCOL", 777)?,
            protocols_file: opt("PROTOCOLS_FILE").map(PathBuf::from),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_port_pairs_and_headers() {
        assert_eq!(port_pairs("8100 18100:8101").unwrap(), vec![(8100, 8100), (18100, 8101)]);
        assert!(port_pairs("x").is_err());
        assert_eq!(
            headers("x-portal-key: abc; Authorization: Bearer t:1").unwrap(),
            vec![("x-portal-key".into(), "abc".into()), ("Authorization".into(), "Bearer t:1".into())]
        );
    }
}
