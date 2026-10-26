//! Verifies the Minecraft auth service's tokens, which Bedrock clients send when they join (NetherNet), with
//! the service's published signing keys.

use crate::config::Config;
use anyhow::{bail, Context, Result};
use jsonwebtoken::{decode, decode_header, Algorithm, DecodingKey, Validation};
use serde_json::Value;
use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

const AUDIENCE: &str = "api://auth-minecraft-services/multiplayer";

struct Keys {
    keys: HashMap<String, (String, String)>,
    issuer: Option<String>,
    loaded: Option<Instant>,
}

pub struct Auth {
    cfg: Arc<Config>,
    http: reqwest::Client,
    state: tokio::sync::Mutex<Keys>,
}

impl Auth {
    pub fn new(cfg: Arc<Config>, http: reqwest::Client) -> Self {
        Auth { cfg, http, state: tokio::sync::Mutex::new(Keys { keys: HashMap::new(), issuer: None, loaded: None }) }
    }

    async fn get_json(&self, url: &str) -> Result<Value> {
        Ok(self.http.get(url).send().await?.error_for_status()?.json().await?)
    }

    async fn load(&self) -> Result<(HashMap<String, (String, String)>, Option<String>)> {
        let (jwks, issuer) = if let Some(file) = &self.cfg.auth_jwks_file {
            (serde_json::from_slice::<Value>(&tokio::fs::read(file).await?)?, self.cfg.auth_issuer.clone())
        } else {
            let discovery = self.get_json(&self.cfg.auth_discovery).await?;
            let service = discovery["result"]["serviceEnvironments"]["auth"]["prod"]["serviceUri"]
                .as_str()
                .context("auth service missing from discovery")?
                .trim_end_matches('/')
                .to_string();
            let openid = self.get_json(&format!("{service}/.well-known/openid-configuration")).await?;
            let issuer = openid["issuer"].as_str().map(str::to_string).unwrap_or(service);
            let jwks = self.get_json(openid["jwks_uri"].as_str().context("no jwks_uri")?).await?;
            (jwks, Some(issuer))
        };
        let keys: HashMap<_, _> = jwks["keys"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|k| k["kty"] == "RSA")
            .filter_map(|k| Some((k["kid"].as_str()?.to_string(), (k["n"].as_str()?.to_string(), k["e"].as_str()?.to_string()))))
            .collect();
        if keys.is_empty() {
            bail!("no RSA keys published");
        }
        Ok((keys, issuer))
    }

    async fn key(&self, kid: &str) -> Option<((String, String), Option<String>)> {
        let mut st = self.state.lock().await;
        let age = st.loaded.map(|t| t.elapsed());
        let stale = age.map_or(true, |a| a > Duration::from_secs(6 * 3600));
        // An unknown key id usually means the keys were rotated; look again, at most every 5 minutes.
        let unknown = !st.keys.contains_key(kid) && age.map_or(true, |a| a > Duration::from_secs(300));
        if stale || unknown {
            match self.load().await {
                Ok((keys, issuer)) => {
                    st.keys = keys;
                    st.issuer = issuer;
                    st.loaded = Some(Instant::now());
                }
                Err(e) => {
                    tracing::warn!("could not load the Minecraft auth signing keys: {e}");
                    st.loaded = Some(Instant::now() - Duration::from_secs(6 * 3600 - 60));
                }
            }
        }
        st.keys.get(kid).cloned().map(|k| (k, st.issuer.clone()))
    }

    /// Returns the token's claims, or an error explaining why it was refused.
    pub async fn verify(&self, token: &str) -> Result<Value> {
        let header = decode_header(token).context("malformed token")?;
        if !matches!(header.alg, Algorithm::RS256 | Algorithm::RS384 | Algorithm::RS512) {
            bail!("unsupported token algorithm {:?}", header.alg);
        }
        let kid = header.kid.context("token has no key id")?;
        let ((n, e), issuer) = self.key(&kid).await.context("token signed with an unknown key")?;
        let key = DecodingKey::from_rsa_components(&n, &e)?;
        let mut v = Validation::new(header.alg);
        v.leeway = 60;
        v.validate_nbf = true;
        v.set_audience(&[AUDIENCE]);
        if let Some(iss) = issuer {
            let bare = iss.trim_end_matches('/').to_string();
            v.set_issuer(&[bare.clone(), format!("{bare}/")]);
        }
        let data = decode::<Value>(token, &key, &v).map_err(|e| anyhow::anyhow!("token rejected: {e}"))?;
        Ok(data.claims)
    }
}

/// The player's auth token from the a=identity line of a NetherNet SDP offer.
pub fn identity_token(sdp: &str) -> Option<String> {
    use base64::Engine;
    let line = sdp.lines().find_map(|l| l.strip_prefix("a=identity:"))?;
    let envelope: Value = serde_json::from_slice(&base64::engine::general_purpose::STANDARD.decode(line.trim()).ok()?).ok()?;
    let assertion = &envelope["assertion"];
    let assertion: Value = match assertion.as_str() {
        Some(s) => serde_json::from_str(s).ok()?,
        None => assertion.clone(),
    };
    assertion["token"].as_str().map(str::to_string)
}

/// The UUID Floodgate gives a Bedrock player: zeros followed by their XUID in hex.
pub fn floodgate_uuid(xuid: u64) -> String {
    let h = format!("{xuid:016x}");
    format!("00000000-0000-0000-{}-{}", &h[..4], &h[4..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn floodgate_uuid_matches_known_player() {
        assert_eq!(floodgate_uuid(0x0009012345ABCDEF), "00000000-0000-0000-0009-012345abcdef");
    }

    #[test]
    fn finds_token_in_offer() {
        use base64::Engine;
        let assertion = serde_json::json!({"token": "a.b.c", "fingerprints": "x..y"}).to_string();
        let env = serde_json::json!({"idp": {"domain": "d", "protocol": "default"}, "assertion": assertion}).to_string();
        let sdp = format!("v=0\r\na=identity:{}\r\nm=application", base64::engine::general_purpose::STANDARD.encode(env));
        assert_eq!(identity_token(&sdp).as_deref(), Some("a.b.c"));
        assert_eq!(identity_token("v=0\r\n"), None);
    }
}
