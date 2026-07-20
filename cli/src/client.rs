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

    /// M7: inspect an already-uploaded APK to discover its native ABIs.
    /// `filename` is the value returned by `upload_apk()` (typically
    /// `<sha256>.apk`).
    pub async fn inspect_apk(&self, filename: &str) -> Result<serde_json::Value> {
        let body = json!({ "apk": filename });
        self.post_json("/api/v1/apk/inspect", body).await
    }

    /// POST /api/v1/apk/verify — verify an APK's signature (M8.1).
    ///
    /// Returns the signature scheme (`v1` / `v2` / `v3` / `none`), the
    /// signer cert SHA-256 fingerprint (when v2/v3 is present), and
    /// the best-effort cert subject DN.
    pub async fn verify_apk(&self, filename: &str) -> Result<serde_json::Value> {
        let body = json!({ "apk": filename });
        self.post_json("/api/v1/apk/verify", body).await
    }

    /// POST /api/v1/apk/bundle — inspect a split-APK bundle (M8.2).
    ///
    /// `arch` is optional; when supplied, the response includes a
    /// `recommended_install` field listing which inner APKs to install
    /// for that target arch.
    pub async fn inspect_bundle(
        &self,
        filename: &str,
        arch: Option<&str>,
    ) -> Result<serde_json::Value> {
        let body = match arch {
            Some(a) => json!({ "apk": filename, "arch": a }),
            None => json!({ "apk": filename }),
        };
        self.post_json("/api/v1/apk/bundle", body).await
    }

    /// POST /api/v1/apk/extract — extract inner APKs from a split-APK
    /// bundle (M9.1).
    ///
    /// `bundle` is the filename of an already-uploaded `.xapk` / `.apks`
    /// bundle. `zip_paths` controls which inner entries to extract; when
    /// empty, all `.apk` entries are extracted.
    ///
    /// Returns `{ out_dir, format, extracted: [...], total_bytes }` where
    /// each entry in `extracted` has `{ zip_path, filename, sha256, size,
    /// kind, abi }`. The `filename` field is relative to `<data_dir>/apks/`
    /// — i.e. `<bundle_sha>/<filename>`.
    pub async fn extract_bundle(
        &self,
        bundle: &str,
        zip_paths: &[String],
    ) -> Result<serde_json::Value> {
        let body = json!({
            "bundle": bundle,
            "zip_paths": zip_paths,
        });
        self.post_json("/api/v1/apk/extract", body).await
    }

    /// POST /api/v1/containers/{id}/screen/record-mp4 — capture an MP4
    /// video of the container's screen via Android's `screenrecord`
    /// binary (M9.2).
    ///
    /// This endpoint blocks synchronously for `duration_sec` seconds
    /// while `screenrecord` runs inside the container's namespaces.
    /// Returns the raw MP4 bytes as the response body.
    ///
    /// We use a per-request client with a bumped timeout because the
    /// default 120s client would cut off a 3-minute recording.
    pub async fn record_mp4(
        &self,
        id: &str,
        duration_sec: u32,
        bit_rate: u32,
        width: u32,
        height: u32,
        rotate: bool,
    ) -> Result<bytes::Bytes> {
        // Build a one-off client with a per-request timeout = duration + 30s
        // grace. The default client's 120s timeout would cut off a 3-min
        // recording.
        let timeout = std::time::Duration::from_secs((duration_sec as u64) + 30);
        let http = Client::builder()
            .timeout(timeout)
            .build()?;
        let url = format!("{}/api/v1/containers/{}/screen/record-mp4", self.base, id);
        let body = json!({
            "duration_sec": duration_sec,
            "bit_rate": bit_rate,
            "width": width,
            "height": height,
            "rotate": rotate,
        });
        let resp = http.post(&url).json(&body).send().await?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await?;
            return Err(anyhow!("HTTP {}: {}", status, text));
        }
        Ok(resp.bytes().await?)
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

    /// POST /api/v1/containers/{id}/screen/human/tap — humanized tap (M5).
    pub async fn human_tap(&self, id: &str, x: i32, y: i32) -> Result<serde_json::Value> {
        let url = format!("{}/api/v1/containers/{}/screen/human/tap", self.base, id);
        let body = json!({ "x": x, "y": y });
        let resp = self.http.post(&url).json(&body).send().await?;
        if !resp.status().is_success() {
            let text = resp.text().await?;
            return Err(anyhow!("human tap failed: {}", text));
        }
        Ok(resp.json().await?)
    }

    /// POST /api/v1/containers/{id}/screen/human/swipe — humanized swipe (M5).
    pub async fn human_swipe(
        &self,
        id: &str,
        x1: i32,
        y1: i32,
        x2: i32,
        y2: i32,
    ) -> Result<serde_json::Value> {
        let url = format!("{}/api/v1/containers/{}/screen/human/swipe", self.base, id);
        let body = json!({ "start_x": x1, "start_y": y1, "end_x": x2, "end_y": y2 });
        let resp = self.http.post(&url).json(&body).send().await?;
        if !resp.status().is_success() {
            let text = resp.text().await?;
            return Err(anyhow!("human swipe failed: {}", text));
        }
        Ok(resp.json().await?)
    }

    /// POST /api/v1/containers/{id}/screen/human/longpress — humanized long-press (M5).
    pub async fn human_long_press(
        &self,
        id: &str,
        x: i32,
        y: i32,
        hold_ms: u32,
    ) -> Result<serde_json::Value> {
        let url =
            format!("{}/api/v1/containers/{}/screen/human/longpress", self.base, id);
        let body = json!({ "x": x, "y": y, "hold_ms": hold_ms });
        let resp = self.http.post(&url).json(&body).send().await?;
        if !resp.status().is_success() {
            let text = resp.text().await?;
            return Err(anyhow!("human longpress failed: {}", text));
        }
        Ok(resp.json().await?)
    }

    /// POST /api/v1/containers/{id}/screen/human/pinch — pinch-zoom (M8.4).
    ///
    /// Performs a two-finger pinch gesture. When `end_distance >
    /// start_distance`, it's a zoom-in; otherwise zoom-out.
    pub async fn human_pinch(
        &self,
        id: &str,
        center_x: i32,
        center_y: i32,
        start_distance: f64,
        end_distance: f64,
        angle_deg: f64,
    ) -> Result<serde_json::Value> {
        let url =
            format!("{}/api/v1/containers/{}/screen/human/pinch", self.base, id);
        let body = json!({
            "center_x": center_x,
            "center_y": center_y,
            "start_distance": start_distance,
            "end_distance": end_distance,
            "angle_deg": angle_deg,
        });
        let resp = self.http.post(&url).json(&body).send().await?;
        if !resp.status().is_success() {
            let text = resp.text().await?;
            return Err(anyhow!("human pinch failed: {}", text));
        }
        Ok(resp.json().await?)
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
