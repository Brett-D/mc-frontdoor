//! Starting the server. One wake at a time, shared by everyone waiting for it.

use crate::backend::Backend;
use crate::config::{Config, WakeMethod};
use serde_json::Value;
use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::sync::watch;

#[derive(Clone, Debug, PartialEq)]
pub enum WakeState {
    Pending,
    Ready,
    Failed(String),
}

pub struct Wake {
    pub started: SystemTime,
    state: watch::Sender<WakeState>,
}

impl Wake {
    pub fn subscribe(&self) -> watch::Receiver<WakeState> {
        self.state.subscribe()
    }
    fn done(&self) -> bool {
        *self.state.borrow() != WakeState::Pending
    }
}

pub struct Waker {
    cfg: Arc<Config>,
    http: reqwest::Client,
    backend: Arc<Backend>,
    current: Mutex<Option<Arc<Wake>>>,
}

enum Call {
    Ok(Value),
    Status(u16, Value),
    Unreachable,
}

impl Waker {
    pub fn new(cfg: Arc<Config>, http: reqwest::Client, backend: Arc<Backend>) -> Arc<Self> {
        Arc::new(Waker { cfg, http, backend, current: Mutex::new(None) })
    }

    pub fn request(self: &Arc<Self>, who: &str) -> Arc<Wake> {
        let mut current = self.current.lock().unwrap();
        if let Some(w) = current.as_ref().filter(|w| !w.done()) {
            return w.clone();
        }
        let (tx, _) = watch::channel(WakeState::Pending);
        let wake = Arc::new(Wake { started: SystemTime::now(), state: tx });
        *current = Some(wake.clone());
        tracing::info!("waking the server for {who}");
        let me = self.clone();
        let w = wake.clone();
        tokio::spawn(async move {
            let result = me.run(&w).await;
            match &result {
                WakeState::Ready => tracing::info!("server is up after {}s", w.started.elapsed().unwrap_or_default().as_secs()),
                WakeState::Failed(reason) => tracing::warn!("wake failed: {reason}"),
                WakeState::Pending => {}
            }
            w.state.send_replace(result);
        });
        wake
    }

    pub fn starting_since(&self) -> Option<SystemTime> {
        self.current.lock().unwrap().as_ref().filter(|w| !w.done()).map(|w| w.started)
    }

    async fn call(&self, method: reqwest::Method, url: &str, headers: &[(String, String)], body: Option<String>) -> Call {
        for attempt in 0..4 {
            let mut req = self.http.request(method.clone(), url).timeout(Duration::from_secs(60));
            for (k, v) in headers {
                req = req.header(k, v);
            }
            if let Some(b) = &body {
                req = req.header("content-type", "application/json").body(b.clone());
            }
            match req.send().await {
                Ok(r) => {
                    let code = r.status().as_u16();
                    // The portal scales to zero; the first request after a quiet period can get a 503.
                    if matches!(code, 502..=504) && attempt < 3 {
                        tokio::time::sleep(Duration::from_secs(5)).await;
                        continue;
                    }
                    let json = r.json::<Value>().await.unwrap_or(Value::Null);
                    return if (200..300).contains(&code) { Call::Ok(json) } else { Call::Status(code, json) };
                }
                Err(_) if attempt < 3 => tokio::time::sleep(Duration::from_secs(5)).await,
                Err(_) => return Call::Unreachable,
            }
        }
        Call::Unreachable
    }

    async fn azure_token(&self, client_id: Option<&str>) -> anyhow::Result<String> {
        let mut url = "http://169.254.169.254/metadata/identity/oauth2/token?api-version=2018-02-01&resource=https://management.azure.com/"
            .to_string();
        if let Some(id) = client_id {
            url.push_str(&format!("&client_id={id}"));
        }
        let v: Value =
            self.http.get(url).header("Metadata", "true").timeout(Duration::from_secs(10)).send().await?.error_for_status()?.json().await?;
        v["access_token"].as_str().map(str::to_string).ok_or_else(|| anyhow::anyhow!("no access token"))
    }

    /// Asks for the start. Ok(true) once accepted; Ok(false) to try again shortly; Err to give up.
    async fn start(&self) -> Result<bool, String> {
        match &self.cfg.wake {
            WakeMethod::Portal { url, key, game } => {
                let body = serde_json::json!({ "game": game }).to_string();
                match self
                    .call(reqwest::Method::POST, &format!("{url}/api/start"), &[("x-portal-key".into(), key.clone())], Some(body))
                    .await
                {
                    Call::Ok(_) => Ok(true),
                    Call::Status(401 | 403, _) => {
                        Err("The front door's portal password is wrong. Use the portal to start the server.".into())
                    }
                    // 409 while the VM is shutting down: try again shortly.
                    Call::Status(code, json) => {
                        tracing::info!("start request returned {code}: {}", json["error"].as_str().unwrap_or(""));
                        Ok(false)
                    }
                    Call::Unreachable => Ok(false),
                }
            }
            WakeMethod::Http { url, method, headers, body } => {
                let m = reqwest::Method::from_bytes(method.as_bytes()).unwrap_or(reqwest::Method::POST);
                match self.call(m, url, headers, body.clone()).await {
                    Call::Ok(_) => Ok(true),
                    Call::Status(code, _) => {
                        tracing::info!("wake URL returned {code}");
                        Ok(false)
                    }
                    Call::Unreachable => Ok(false),
                }
            }
            WakeMethod::AzureVm { vm_id, client_id } => {
                let token = match self.azure_token(client_id.as_deref()).await {
                    Ok(t) => t,
                    Err(e) => {
                        tracing::warn!("could not get an Azure token: {e}");
                        return Ok(false);
                    }
                };
                let url = format!("https://management.azure.com{}/start?api-version=2024-03-01", vm_id.trim_end_matches('/'));
                match self
                    .call(reqwest::Method::POST, &url, &[("authorization".into(), format!("Bearer {token}"))], Some(String::new()))
                    .await
                {
                    Call::Ok(_) => Ok(true),
                    Call::Status(401 | 403, _) => {
                        Err("The front door isn't allowed to start the VM (check its managed identity's role).".into())
                    }
                    Call::Status(code, json) => {
                        tracing::info!("VM start returned {code}: {}", json["error"]["message"].as_str().unwrap_or(""));
                        Ok(false)
                    }
                    Call::Unreachable => Ok(false),
                }
            }
        }
    }

    /// Portal only: a reported start failure, or a stop while starting, ends the wait early.
    async fn portal_check(&self, wake: &Wake, saw_starting: &mut bool) -> Option<String> {
        let WakeMethod::Portal { url, key, game } = &self.cfg.wake else { return None };
        let Call::Ok(body) =
            self.call(reqwest::Method::GET, &format!("{url}/api/status"), &[("x-portal-key".into(), key.clone())], None).await
        else {
            return None;
        };
        let g = &body["games"][game.as_str()];
        let started = wake.started.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() as i64;
        let failure = &g["lastFailure"];
        if failure.is_object() && failure["time"].as_i64().unwrap_or(0) >= started - 60 {
            return Some(format!("The server could not start: {}", failure["reason"].as_str().unwrap_or("unknown error")));
        }
        match g["state"].as_str() {
            Some("starting") => *saw_starting = true,
            Some("stopped") if *saw_starting && body["vm"] == "running" => {
                return Some("The server was stopped while it was starting.".into());
            }
            _ => {}
        }
        None
    }

    async fn run(&self, wake: &Wake) -> WakeState {
        let deadline = Instant::now() + self.cfg.wake_timeout;
        let (mut accepted, mut next_start, mut next_check, mut saw_starting) = (false, Instant::now(), Instant::now(), false);
        while Instant::now() < deadline {
            if self.backend.status(Duration::ZERO).await.is_some() {
                return WakeState::Ready;
            }
            let now = Instant::now();
            if !accepted && now >= next_start {
                match self.start().await {
                    Ok(true) => accepted = true,
                    Ok(false) => next_start = now + Duration::from_secs(15),
                    Err(reason) => return WakeState::Failed(reason),
                }
            }
            if accepted && now >= next_check {
                next_check = now + Duration::from_secs(10);
                if let Some(reason) = self.portal_check(wake, &mut saw_starting).await {
                    return WakeState::Failed(reason);
                }
            }
            tokio::time::sleep(Duration::from_secs(3)).await;
        }
        WakeState::Failed("The server took too long to start. Try again, or check the portal.".into())
    }
}
