// src/client.rs
//
// Thin HTTP client wrapping the daemon's REST API.

use anyhow::{anyhow, Result};
use reqwest::{multipart, Client, Response};
use serde::{de::DeserializeOwned, Serialize};
use serde_json::json;
use std::path::Path;
use std::time::Duration;

pub struct DroidkerClient {
    base: String,
    http: Client,
}

impl DroidkerClient {
    pub fn new(host: &str) -> Result<Self> {
        let http = Client::builder()
            .timeout(Duration::from_secs(120))
            .build()?;
        Ok(Self {
            base: host.trim_end_matches('/').to_string(),
            http,
        })
    }

    pub async fn health(&self) -> Result<serde_json::Value> {
        self.get_json("/api/v1/health").await
    }

    pub async fn ready(&self) -> Result<serde_json::Value> {
        self.get_json("/api/v1/ready").await
    }

    pub async fn list_containers(&self) -> Result<Vec<serde_json::Value>> {
        let v = self.get_json("/api/v1/containers").await?;
        Ok(serde_json::from_value(v)?)
    }

    pub async fn get_container(&self, id: &str) -> Result<serde_json::Value> {
        self.get_json(&format!("/api/v1/containers/{}", id)).await
    }

    pub async fn create_container(&self, body: &serde_json::Value) -> Result<serde_json::Value> {
        self.post_json("/api/v1/containers", body.clone()).await
    }

    pub async fn start_container(&self, id: &str) -> Result<serde_json::Value> {
        self.post_json(&format!("/api/v1/containers/{}/start", id), serde_json::Value::Null)
            .await
    }

    pub async fn stop_container(&self, id: &str) -> Result<serde_json::Value> {
        self.post_json(&format!("/api/v1/containers/{}/stop", id), serde_json::Value::Null)
            .await
    }

    pub async fn delete_container(&self, id: &str) -> Result<()> {
        let url = format!("{}/api/v1/containers/{}", self.base, id);
        let resp = self.http.delete(&url).send().await?;
        if !resp.status().is_success() {
            return Err(anyhow!("delete failed: {}", resp.status()));
        }
        Ok(())
    }

    /// GET /api/v1/containers/{id}/stats — one-shot stats snapshot.
    pub async fn get_stats(&self, id: &str) -> Result<serde_json::Value> {
        self.get_json(&format!("/api/v1/containers/{}/stats", id))
            .await
    }

    /// GET /api/v1/containers/{id}/logs?kind=<kind> — one-shot log fetch.
    pub async fn get_logs(&self, id: &str, kind: &str) -> Result<String> {
        let url = format!(
            "{}/api/v1/containers/{}/logs?kind={}",
            self.base, id, kind
        );
        let resp = self.http.get(&url).send().await?;
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            return Err(anyhow!("HTTP {}: {}", status, text));
        }
        Ok(text)
    }

    /// POST /api/v1/containers/{id}/exec — one-shot exec, returns
    /// `{ exit_code, stdout, stderr, pid, timed_out }`.
    pub async fn exec(
        &self,
        id: &str,
        cmd: &[String],
        cwd: Option<&str>,
        tty: bool,
    ) -> Result<serde_json::Value> {
        let body = json!({
            "cmd": cmd,
            "cwd": cwd,
            "tty": tty,
        });
        let url = format!("{}/api/v1/containers/{}/exec", self.base, id);
        let resp = self.http.post(&url).json(&body).send().await?;
        decode_json(resp).await
    }

    pub async fn upload_apk(&self, path: &Path) -> Result<serde_json::Value> {
        let url = format!("{}/api/v1/upload/apk", self.base);
        let file_bytes = tokio::fs::read(path).await?;
        let filename = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("upload.apk")
            .to_string();

        let part = multipart::Part::bytes(file_bytes).file_name(filename);
        let form = multipart::Form::new().part("file", part);

        let resp = self.http.post(&url).multipart(form).send().await?;
        decode_json(resp).await
    }

    // ---- Low-level helpers -------------------------------------------------

    async fn get_json<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        let url = format!("{}{}", self.base, path);
        let resp = self.http.get(&url).send().await?;
        decode_json(resp).await
    }

    async fn post_json<T: DeserializeOwned, B: Serialize>(
        &self,
        path: &str,
        body: B,
    ) -> Result<T> {
        let url = format!("{}{}", self.base, path);
        let resp = self.http.post(&url).json(&body).send().await?;
        decode_json(resp).await
    }

    /// POST /api/v1/containers/{id}/screen/touch — fire-and-forget touch inject.
    pub async fn send_touch(
        &self,
        id: &str,
        x: i32,
        y: i32,
        phase: &str,
        pressure: u32,
        slot: u32,
    ) -> Result<()> {
        let url = format!("{}/api/v1/containers/{}/screen/touch", self.base, id);
        let body = json!({
            "x": x,
            "y": y,
            "phase": phase,
            "pressure": pressure,
            "slot": slot,
        });
        let resp = self.http.post(&url).json(&body).send().await?;
        if !resp.status().is_success() {
            let text = resp.text().await?;
            return Err(anyhow!("touch failed: {}", text));
        }
        Ok(())
    }

    /// POST /api/v1/containers/{id}/screen/key — fire-and-forget key inject.
    pub async fn send_key(&self, id: &str, code: &str, down: bool) -> Result<()> {
        let url = format!("{}/api/v1/containers/{}/screen/key", self.base, id);
        let body = json!({ "code": code, "down": down });
        let resp = self.http.post(&url).json(&body).send().await?;
        if !resp.status().is_success() {
            let text = resp.text().await?;
            return Err(anyhow!("key failed: {}", text));
        }
        Ok(())
    }

    /// Build a WebSocket URL by converting the HTTP base URL (http:// or
    /// https://) to ws:// or wss:// and appending the given path.
    pub fn ws_url(&self, path: &str) -> Result<String> {
        let base = &self.base;
        let ws_base = if let Some(rest) = base.strip_prefix("https://") {
            format!("wss://{}", rest)
        } else if let Some(rest) = base.strip_prefix("http://") {
            format!("ws://{}", rest)
        } else {
            return Err(anyhow!("invalid base URL for WS upgrade: {}", base));
        };
        Ok(format!("{}{}", ws_base, path))
    }
}

async fn decode_json<T: DeserializeOwned>(resp: Response) -> Result<T> {
    let status = resp.status();
    let text = resp.text().await?;
    if !status.is_success() {
        return Err(anyhow!("HTTP {}: {}", status, text));
    }
    Ok(serde_json::from_str(&text)?)
}
