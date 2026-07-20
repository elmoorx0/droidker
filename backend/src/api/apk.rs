// src/api/apk.rs
//
// APK inspection + signature verification + bundle inspection + bundle
// extraction endpoints (M7.1 + M8.1 + M8.2 + M9.1).
//
//   GET  /api/v1/apk/inspect?path=<filename>
//   POST /api/v1/apk/inspect   { "apk": "<filename>" }
//   GET  /api/v1/apk/verify?path=<filename>
//   POST /api/v1/apk/verify   { "apk": "<filename>" }
//   GET  /api/v1/apk/bundle?path=<filename>&arch=<arch>
//   POST /api/v1/apk/bundle   { "apk": "<filename>", "arch": "<arch>" }
//   POST /api/v1/apk/extract  { "bundle": "<filename>", "zip_paths": [...] }
//
// Both `inspect` forms look up the APK under `<data_dir>/apks/<filename>`
// and return the inspect result (list of native ABIs + recommended arch).
// The CLI uses this to implement `droidker run --arch auto`: it uploads
// the APK, then calls /apk/inspect to pick the best arch before creating
// the container.
//
// The `verify` forms (added in M8.1) return the APK's signature info:
// whether it's signed, which scheme (v1/v2/v3), the signer cert SHA-256
// fingerprint, and the (best-effort) cert subject DN. The CLI uses this
// to refuse unsigned APKs by default and to print the fingerprint so
// users can cross-check against an out-of-band source of truth.
//
// The `bundle` forms (added in M8.2) inspect `.xapk` / `.apks` split-APK
// bundles. They enumerate the inner APKs (base + ABI / locale / density
// splits) and recommend which ones to install for a given target arch.
//
// The `extract` endpoint (added in M9.1) actually pulls the inner APKs
// out of the bundle ZIP and writes them to `<data_dir>/apks/<bundle_sha>/`
// on disk so a follow-up `POST /containers` with `extra_apks` can mount
// them as split APKs in the container's `/data/app/<package>/`.

use crate::apk::{
    extract_bundle, inspect_apk, inspect_bundle, verify_signature, ApkSignatureInfo,
    BundleExtractResult, BundleInspectResult, ExtractSpec, InspectResult,
};
use crate::error::{DroidkerError, Result};
use crate::AppState;
use actix_web::{get, post, web, HttpResponse, Responder};
use serde::Deserialize;

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(inspect_by_query)
        .service(inspect_by_body)
        .service(verify_by_query)
        .service(verify_by_body)
        .service(bundle_by_query)
        .service(bundle_by_body)
        .service(extract_bundle_endpoint);
}

/// GET /api/v1/apk/inspect?path=<filename>
///
/// `filename` is the value returned by `POST /api/v1/upload/apk`
/// (typically `<sha256>.apk`). It must not contain path separators —
/// we reject anything that looks like a traversal attempt.
#[get("/apk/inspect")]
async fn inspect_by_query(
    state: web::Data<AppState>,
    query: web::Query<InspectQuery>,
) -> Result<impl Responder> {
    let filename = &query.path;
    let result = perform_inspect(&state, filename)?;
    Ok(HttpResponse::Ok().json(result))
}

/// POST /api/v1/apk/inspect  { "apk": "<filename>" }
///
/// Same semantics as the GET form, but accepts a JSON body for clients
/// that prefer POST (e.g. when the filename is very long or contains
/// query-string-reserved characters).
#[post("/apk/inspect")]
async fn inspect_by_body(
    state: web::Data<AppState>,
    body: web::Json<InspectBody>,
) -> Result<impl Responder> {
    let filename = &body.apk;
    let result = perform_inspect(&state, filename)?;
    Ok(HttpResponse::Ok().json(result))
}

/// GET /api/v1/apk/verify?path=<filename>
///
/// Returns the APK's signature information. Does NOT perform full
/// cryptographic validation — only detects the signature scheme and
/// extracts the signer certificate fingerprint (when v2/v3 is present).
#[get("/apk/verify")]
async fn verify_by_query(
    state: web::Data<AppState>,
    query: web::Query<InspectQuery>,
) -> Result<impl Responder> {
    let filename = &query.path;
    let result = perform_verify(&state, filename)?;
    Ok(HttpResponse::Ok().json(result))
}

/// POST /api/v1/apk/verify  { "apk": "<filename>" }
///
/// Same semantics as the GET form, but accepts a JSON body.
#[post("/apk/verify")]
async fn verify_by_body(
    state: web::Data<AppState>,
    body: web::Json<InspectBody>,
) -> Result<impl Responder> {
    let filename = &body.apk;
    let result = perform_verify(&state, filename)?;
    Ok(HttpResponse::Ok().json(result))
}

/// GET /api/v1/apk/bundle?path=<filename>&arch=<arch>
///
/// Inspects a `.xapk` or `.apks` split-APK bundle and returns the list
/// of inner APKs (base + splits) plus a recommendation of which ones to
/// install for the given target arch. `arch` is optional — when
/// omitted, the recommendation includes only the base APK.
#[get("/apk/bundle")]
async fn bundle_by_query(
    state: web::Data<AppState>,
    query: web::Query<BundleQuery>,
) -> Result<impl Responder> {
    let filename = &query.path;
    let arch = query.arch.as_deref();
    let result = perform_bundle(&state, filename, arch)?;
    Ok(HttpResponse::Ok().json(result))
}

/// POST /api/v1/apk/bundle  { "apk": "<filename>", "arch": "<arch>" }
///
/// Same semantics as the GET form, but accepts a JSON body. `arch` is
/// optional in both forms.
#[post("/apk/bundle")]
async fn bundle_by_body(
    state: web::Data<AppState>,
    body: web::Json<BundleBody>,
) -> Result<impl Responder> {
    let filename = &body.apk;
    let arch = body.arch.as_deref();
    let result = perform_bundle(&state, filename, arch)?;
    Ok(HttpResponse::Ok().json(result))
}

#[derive(Deserialize)]
struct InspectQuery {
    path: String,
}

#[derive(Deserialize)]
struct InspectBody {
    apk: String,
}

#[derive(Deserialize)]
struct BundleQuery {
    path: String,
    /// Optional target arch (`arm`, `arm64`, `x86`, `x86_64`). When
    /// supplied, the recommendation includes the matching ABI split.
    arch: Option<String>,
}

#[derive(Deserialize)]
struct BundleBody {
    apk: String,
    arch: Option<String>,
}

/// Body for `POST /api/v1/apk/extract` (M9.1).
#[derive(Deserialize)]
struct ExtractBody {
    /// Filename of the already-uploaded bundle (as returned by `upload`).
    /// Must end with `.xapk` or `.apks`.
    bundle: String,
    /// Which inner APK entries to extract, identified by their `zip_path`
    /// (as reported by `GET /apk/bundle`). When empty or omitted, all
    /// `.apk` entries in the bundle are extracted.
    #[serde(default)]
    zip_paths: Vec<String>,
}

fn perform_inspect(state: &AppState, filename: &str) -> Result<InspectResult> {
    let path = resolve_apk_path(state, filename, ApkKind::PlainApk)?;
    let result = inspect_apk(&path).map_err(|e| DroidkerError::Internal(format!("{e}")))?;
    Ok(result)
}

fn perform_verify(state: &AppState, filename: &str) -> Result<ApkSignatureInfo> {
    let path = resolve_apk_path(state, filename, ApkKind::PlainApk)?;
    let info = verify_signature(&path).map_err(|e| DroidkerError::Internal(format!("{e}")))?;
    Ok(info)
}

fn perform_bundle(state: &AppState, filename: &str, arch: Option<&str>) -> Result<BundleInspectResult> {
    let path = resolve_apk_path(state, filename, ApkKind::Bundle)?;
    let result =
        inspect_bundle(&path, arch).map_err(|e| DroidkerError::Internal(format!("{e}")))?;
    Ok(result)
}

/// POST /api/v1/apk/extract  (M9.1)
///
/// Pulls inner APK entries out of a `.xapk` / `.apks` bundle and writes
/// them to `<data_dir>/apks/<bundle_sha>/` on disk.
///
/// Request body:
///   {
///     "bundle": "<filename>",       // .xapk or .apks under <data_dir>/apks/
///     "zip_paths": [...]            // optional; empty = extract all .apk entries
///   }
///
/// Response body:
///   {
///     "out_dir": "<absolute path>",
///     "format": "xapk" | "apks",
///     "extracted": [
///       {
///         "zip_path": "splits/base.apk",
///         "filename": "base.apk",
///         "sha256": "<hex>",
///         "size": 12345678,
///         "kind": "base" | "abi" | "locale" | "density" | "other",
///         "abi": "arm64_v8a" | null
///       },
///       ...
///     ],
///     "total_bytes": 23456789
///   }
///
/// The `filename` field of each extracted APK is the path the caller
/// should pass to `POST /containers` as either `apk` (for the base) or
/// an entry in `extra_apks` (for splits). The path is relative to
/// `<data_dir>/apks/` — i.e. `<bundle_sha>/<filename>`.
#[post("/apk/extract")]
async fn extract_bundle_endpoint(
    state: web::Data<AppState>,
    body: web::Json<ExtractBody>,
) -> Result<impl Responder> {
    let bundle_path = resolve_apk_path(&state, &body.bundle, ApkKind::Bundle)?;

    // Compute SHA-256 of the bundle file so the extraction lands under a
    // dedup'd directory. We don't dedup across bundles of the same SHA —
    // the second extract just overwrites the first, which is fine because
    // the APKs inside are byte-identical.
    let bundle_bytes = tokio::fs::read(&bundle_path).await?;
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(&bundle_bytes);
    let bundle_sha = hex::encode(hasher.finalize());

    let out_dir = state.settings.data_dir.join("apks").join(&bundle_sha);
    let spec = ExtractSpec {
        out_dir: out_dir.clone(),
        zip_paths: body.zip_paths.clone(),
    };

    // The extraction itself is synchronous + potentially long-running
    // (deflating a 100 MB base APK takes ~1s on a 1-vCPU VPS). Run it on
    // a blocking thread so we don't stall the actix worker.
    let result = web::block(move || extract_bundle(&bundle_path, &spec))
        .await
        .map_err(|e| DroidkerError::Internal(format!("extract_bundle join error: {e}")))?
        .map_err(|e| DroidkerError::Internal(format!("{e}")))?;

    tracing::info!(
        bundle = %body.bundle,
        bundle_sha = %bundle_sha,
        extracted = result.extracted.len(),
        total_bytes = result.total_bytes,
        "bundle extracted"
    );

    Ok(HttpResponse::Ok().json(result))
}

/// Which kind of APK file we're resolving — affects the allowed
/// extension. Plain APKs accept only `.apk`; bundles accept `.xapk`
/// and `.apks`.
enum ApkKind {
    PlainApk,
    Bundle,
}

/// Resolve `<filename>` to an absolute path under `<data_dir>/apks/`.
///
/// We reject path separators and `..` to prevent traversal — the
/// filename must be a bare name, never an arbitrary host path. We also
/// enforce the extension as a sanity check (the upload endpoint
/// enforces the same rule, so this is purely defensive).
fn resolve_apk_path(state: &AppState, filename: &str, kind: ApkKind) -> Result<std::path::PathBuf> {
    if filename.contains('/')
        || filename.contains('\\')
        || filename.contains("..")
        || filename.is_empty()
    {
        return Err(DroidkerError::BadRequest(format!(
            "invalid apk filename: {:?} (must be a bare name, no path separators)",
            filename
        )));
    }
    let lower = filename.to_lowercase();
    let ok = match kind {
        ApkKind::PlainApk => lower.ends_with(".apk"),
        ApkKind::Bundle => lower.ends_with(".xapk") || lower.ends_with(".apks"),
    };
    if !ok {
        return Err(DroidkerError::BadRequest(match kind {
            ApkKind::PlainApk => "apk filename must end with .apk".into(),
            ApkKind::Bundle => "bundle filename must end with .xapk or .apks".into(),
        }));
    }
    Ok(state.settings.data_dir.join("apks").join(filename))
}
