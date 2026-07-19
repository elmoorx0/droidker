// src/api/upload.rs
//
// Multipart APK upload endpoint. Files land in <data_dir>/apks/<sha256>.apk
// so duplicate uploads are deduplicated automatically.

use crate::error::{DroidkerError, Result};
use crate::AppState;
use actix_multipart::Multipart;
use actix_web::{post, web, HttpResponse, Responder};
use futures_util::TryStreamExt;
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use tokio::io::AsyncWriteExt;

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(upload_apk);
}

/// POST /api/v1/upload/apk  (multipart, field name "file")
///
/// Returns:
///   { "filename": "<sha256>.apk", "size": <bytes>, "sha256": "<hex>" }
#[post("/upload/apk")]
async fn upload_apk(state: web::Data<AppState>, mut payload: Multipart) -> Result<impl Responder> {
    let apks_dir = state.settings.data_dir.join("apks");
    tokio::fs::create_dir_all(&apks_dir).await?;

    // We expect exactly one field named "file".
    while let Some(mut field) = payload.try_next().await? {
        let content_disp = field.content_disposition();
        let original_name = content_disp
            .get_filename()
            .unwrap_or("upload.apk")
            .to_string();

        if !is_apk_filename(&original_name) {
            return Err(DroidkerError::InvalidApk(format!(
                "file must have .apk, .xapk, or .apks extension (got: {original_name})"
            )));
        }

        // Stream the upload into a temp file while hashing it.
        let temp_path = apks_dir.join(format!(
            ".upload-{}.tmp",
            uuid::Uuid::new_v4()
        ));
        let mut hasher = Sha256::new();
        let mut file = tokio::fs::File::create(&temp_path).await?;
        let mut size: u64 = 0;

        while let Some(chunk) = field.try_next().await? {
            hasher.update(&chunk);
            file.write_all(&chunk).await?;
            size += chunk.len() as u64;
        }
        file.flush().await?;
        drop(file);

        let sha = hex::encode(hasher.finalize());
        let final_path: PathBuf = apks_dir.join(format!("{}.apk", sha));

        // Dedup: if the file already exists, drop the temp upload.
        if final_path.exists() {
            tokio::fs::remove_file(&temp_path).await.ok();
        } else {
            tokio::fs::rename(&temp_path, &final_path).await?;
        }

        tracing::info!(sha = %sha, size, original = %original_name, "APK uploaded");

        return Ok(HttpResponse::Ok().json(serde_json::json!({
            "filename": final_path.file_name().unwrap().to_string_lossy(),
            "sha256": sha,
            "size": size,
            "original_name": original_name,
        })));
    }

    Err(DroidkerError::BadRequest("no file field in multipart upload".into()))
}

/// True if the filename ends with `.apk`, `.xapk`, or `.apks`
/// (case-insensitive). The latter two are split-APK bundle formats
/// handled by `apk::bundle` (M8.2).
fn is_apk_filename(name: &str) -> bool {
    let lower = name.to_lowercase();
    lower.ends_with(".apk") || lower.ends_with(".xapk") || lower.ends_with(".apks")
}
