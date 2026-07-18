// src/logs/streamer.rs
//
// Tail a container log file forever, emitting byte chunks as they arrive.

use crate::error::{DroidkerError, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::Duration;
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncSeekExt, SeekFrom};
use tokio::sync::mpsc;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogKind {
    /// droidker-init boot-phase logs.
    Init,
    /// ART / app_process runtime output.
    Runtime,
    /// Captured `logcat` (the Android system log buffer).
    Logcat,
}

impl LogKind {
    pub fn filename(&self) -> &'static str {
        match self {
            LogKind::Init => "droidker.init.log",
            LogKind::Runtime => "droidker.runtime.log",
            LogKind::Logcat => "droidker.logcat.log",
        }
    }

    pub fn from_str_lossy(s: &str) -> Self {
        match s {
            "init" => LogKind::Init,
            "runtime" => LogKind::Runtime,
            "logcat" => LogKind::Logcat,
            _ => LogKind::Runtime,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct LogTailRequest {
    pub kind: LogKind,
    /// If true, start from the beginning of the file. Otherwise start from
    /// the last 8KB (default behavior — shows recent history only).
    #[serde(default)]
    pub follow_from_start: bool,
}

pub struct LogStreamer {
    pub container_id: Uuid,
    pub overlay_dir: PathBuf,
}

impl LogStreamer {
    pub fn new(container_id: Uuid, overlay_dir: PathBuf) -> Self {
        Self {
            container_id,
            overlay_dir,
        }
    }

    /// Resolve the path of the requested log file inside the container's
    /// overlay upperdir.
    pub fn log_path(&self, kind: LogKind) -> PathBuf {
        self.overlay_dir
            .join(self.container_id.to_string())
            .join("upper")
            .join(kind.filename())
    }

    /// Begin tailing. Returns a receiver that yields raw byte chunks.
    ///
    /// The tailer loops:
    ///   1. Open the file (wait for it to appear if missing).
    ///   2. Seek to the requested start position.
    ///   3. Read available bytes; push each chunk to the channel.
    ///   4. When EOF, sleep 200ms and try again (handles log rotation +
    ///      slow writers).
    ///   5. Exit when the receiver is dropped (channel closed).
    pub async fn tail(
        &self,
        req: LogTailRequest,
    ) -> Result<mpsc::Receiver<Vec<u8>>> {
        let path = self.log_path(req.kind);
        let (tx, rx) = mpsc::channel(64);

        let from_start = req.follow_from_start;
        tokio::spawn(async move {
            if let Err(e) = tail_loop(path, from_start, tx).await {
                tracing::warn!(error = %e, "log tail loop exited with error");
            }
        });

        Ok(rx)
    }

    /// Read the entire log file once (no following). Returns the full
    /// contents as a single byte vector. Used by `GET /containers/{id}/logs`
    /// for one-shot fetches.
    pub async fn snapshot(&self, kind: LogKind) -> Result<Vec<u8>> {
        let path = self.log_path(kind);
        if !path.exists() {
            return Ok(Vec::new());
        }
        tokio::fs::read(&path).await.map_err(|e| {
            DroidkerError::Io(std::io::Error::new(
                e.kind(),
                format!("read {}: {}", path.display(), e),
            ))
        })
    }
}

async fn tail_loop(
    path: PathBuf,
    from_start: bool,
    tx: mpsc::Sender<Vec<u8>>,
) -> Result<()> {
    // 1. Wait for the file to appear (up to 30 seconds).
    let mut waited = 0u64;
    while !path.exists() {
        if waited >= 30 {
            tracing::warn!(
                path = %path.display(),
                "log file did not appear after 30s; tailer giving up"
            );
            return Ok(());
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
        waited += 1;
    }

    // 2. Open the file.
    let mut file = File::open(&path).await?;
    let mut pos = if from_start {
        0u64
    } else {
        // Seek to the last 8KB (or start if the file is smaller).
        let meta = file.metadata().await?;
        meta.len().saturating_sub(8 * 1024)
    };
    file.seek(SeekFrom::Start(pos)).await?;

    let mut tmp = vec![0u8; 8192];
    tracing::info!(path = %path.display(), from_start, "tailing log");

    loop {
        match file.read(&mut tmp).await {
            Ok(0) => {
                // EOF — wait for more data. We also re-check that the
                // file hasn't been rotated (size shrunk below `pos`).
                if let Ok(meta) = file.metadata().await {
                    if meta.len() < pos {
                        // Rotation: reopen from start.
                        tracing::info!(path = %path.display(), "log rotated; reopening");
                        file = File::open(&path).await?;
                        pos = 0;
                        continue;
                    }
                }
                if tx.is_closed() {
                    return Ok(());
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            Ok(n) => {
                let chunk = tmp[..n].to_vec();
                pos += n as u64;
                if tx.send(chunk).await.is_err() {
                    // Receiver dropped — caller is gone.
                    return Ok(());
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_kind_round_trips_strings() {
        assert!(matches!(LogKind::from_str_lossy("init"), LogKind::Init));
        assert!(matches!(LogKind::from_str_lossy("runtime"), LogKind::Runtime));
        assert!(matches!(LogKind::from_str_lossy("logcat"), LogKind::Logcat));
        // Unknown → defaults to Runtime.
        assert!(matches!(LogKind::from_str_lossy("nope"), LogKind::Runtime));
    }

    #[test]
    fn log_kind_filename_is_stable() {
        assert_eq!(LogKind::Init.filename(), "droidker.init.log");
        assert_eq!(LogKind::Runtime.filename(), "droidker.runtime.log");
        assert_eq!(LogKind::Logcat.filename(), "droidker.logcat.log");
    }
}
