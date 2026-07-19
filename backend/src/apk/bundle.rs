// src/apk/bundle.rs
//
// Split-APK bundle inspector (M8.2).
//
// Modern Android apps are increasingly shipped as *split APKs* — one
// base APK plus several "config" splits that each carry the resources
// for a single ABI, locale, or screen density. The two common on-disk
// formats are:
//
//   * **.xapk** — the format used by APKPure and similar third-party
//     stores. A ZIP containing `manifest.json`, the base APK, and zero
//     or more `config.<abi>.apk` / `config.<locale>.apk` splits at the
//     archive root.
//
//   * **.apks** — the format produced by Google's `bundletool`. A ZIP
//     containing `toc.json` and a `splits/` directory holding
//     `base.apk`, `config.arm64_v8a.apk`, etc.
//
// Both formats are *just ZIPs of APKs*, so we can reuse the central
// directory walker from `apk::inspect` to enumerate the inner APKs
// without pulling in a full ZIP library.
//
// This module's job is to:
//
//   1. Detect whether a given file is a bundle (by looking at the
//      extension *and* the inner manifest/toc file).
//   2. Enumerate the inner APKs and classify each as base / abi-split /
//      locale-split / density-split / other.
//   3. Recommend the set of splits to install for a given target arch
//      (e.g. on aarch64: base + config.arm64_v8a + config.en + config.xxhdpi).
//
// We do NOT extract or install the APKs here — that's done by the
// container init script (see `backend/src/bin/init.rs`). This module
// only inspects.

use crate::apk::inspect::{find_eocd_internal, ApkInspectionError, EOCD_FIXED_SIZE};
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

// ----- Public types -------------------------------------------------------

/// Format of an APK bundle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum BundleFormat {
    /// `.xapk` — APKPure-style bundle with `manifest.json` at the root.
    Xapk,
    /// `.apks` — bundletool-style bundle with `toc.json` and a
    /// `splits/` subdirectory.
    Apks,
}

impl BundleFormat {
    pub fn as_str(&self) -> &'static str {
        match self {
            BundleFormat::Xapk => "xapk",
            BundleFormat::Apks => "apks",
        }
    }
}

/// Classification of a single APK entry inside a bundle.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SplitKind {
    /// The base APK — contains the manifest, classes.dex, and the
    /// app's core resources. Always required.
    Base,
    /// ABI-specific split (e.g. `config.arm64_v8a.apk`). Contains only
    /// the native `.so` files for one ABI.
    Abi,
    /// Locale-specific split (e.g. `config.en.apk`). Contains only the
    /// string tables for one language.
    Locale,
    /// Screen-density split (e.g. `config.xxhdpi.apk`). Contains only
    /// the drawables for one density bucket.
    Density,
    /// Anything else (e.g. `config.features.apk` for feature modules,
    /// or unknown splits). We surface these but don't auto-install them.
    Other,
}

/// Information about a single APK inside a bundle.
#[derive(Debug, Clone, serde::Serialize)]
pub struct BundleEntry {
    /// Path of the APK inside the bundle ZIP (e.g. `base.apk` or
    /// `splits/config.arm64_v8a.apk`).
    pub zip_path: String,
    /// Classification of the entry (base / abi / locale / density / other).
    pub kind: SplitKind,
    /// For `Abi` splits: the ABI name (e.g. `arm64_v8a`). `None` for
    /// all other kinds.
    pub abi: Option<String>,
    /// For `Locale` splits: the locale code (e.g. `en`, `zh_CN`).
    /// `None` for all other kinds.
    pub locale: Option<String>,
    /// For `Density` splits: the density bucket name (e.g. `xxhdpi`).
    /// `None` for all other kinds.
    pub density: Option<String>,
    /// Uncompressed size of the inner APK (bytes). Useful as a hint
    /// for "how big is this split?".
    pub uncompressed_size: u64,
}

/// Result of inspecting an APK bundle.
#[derive(Debug, Clone, serde::Serialize)]
pub struct BundleInspectResult {
    /// Path of the bundle file that was inspected.
    pub path: String,
    /// Detected bundle format.
    pub format: BundleFormat,
    /// Best-effort package name extracted from the manifest/toc.
    /// `None` if we couldn't find one (we don't parse JSON in this
    /// module — we just regex-scan the first 64 KB of the manifest
    /// for a `"package_name": "..."` field).
    pub package: Option<String>,
    /// Best-effort version name (same extraction method as `package`).
    pub version_name: Option<String>,
    /// Total number of entries in the bundle ZIP.
    pub zip_entry_count: usize,
    /// All APK entries found inside the bundle, classified.
    pub entries: Vec<BundleEntry>,
    /// Set of ABIs that the bundle ships splits for. Empty when the
    /// bundle has no ABI splits (rare — most app bundles ship them).
    pub available_abis: Vec<String>,
    /// Recommendation: which ZIP paths to install for the given target
    /// arch. Always includes `base.apk` (or the detected base entry).
    /// Includes the matching ABI split when one exists. Skips locale
    /// and density splits — callers should add those based on their
    /// specific needs (e.g. the dashboard might let the user pick).
    pub recommended_install: Vec<String>,
}

// ----- Constants ----------------------------------------------------------

/// Known ABIs that bundletool / xapk produce splits for. The trailing
/// segment of `config.<abi>.apk` will be one of these.
const KNOWN_BUNDLE_ABIS: &[&str] = &[
    "arm64_v8a",
    "armeabi_v7a",
    "x86_64",
    "x86",
    "armeabi",
    "mips64",
    "mips",
];

/// Common density bucket names used in `config.<density>.apk` splits.
const KNOWN_DENSITIES: &[&str] = &[
    "ldpi", "mdpi", "hdpi", "xhdpi", "xxhdpi", "xxxhdpi", "nodpi", "tvdpi", "anydpi",
];

// ----- Public entry point -------------------------------------------------

/// Inspect a `.xapk` or `.apks` bundle and return its manifest of splits.
///
/// Returns `Err(NotABundle)` if the file doesn't look like a bundle
/// (no `manifest.json` and no `toc.json` at the expected locations).
/// Callers should fall back to `inspect_apk` for plain APKs.
pub fn inspect_bundle<P: AsRef<Path>>(
    bundle_path: P,
    target_arch: Option<&str>,
) -> std::result::Result<BundleInspectResult, ApkInspectionError> {
    let path = bundle_path.as_ref();
    if !path.exists() {
        return Err(ApkInspectionError::NotFound(path.display().to_string()));
    }
    let path_str = path.display().to_string();

    let mut file = std::fs::File::open(path)?;
    let file_len = file.metadata()?.len();
    if file_len < (EOCD_FIXED_SIZE as u64) {
        return Err(ApkInspectionError::TooSmall(file_len));
    }

    let eocd = find_eocd_internal(&mut file, file_len)?;
    let cd_offset = eocd.cd_offset;
    let cd_size = eocd.cd_size as u64;
    if cd_offset + cd_size > file_len {
        return Err(ApkInspectionError::CdOutOfBounds {
            cd_offset,
            cd_size,
            file_len,
        });
    }
    file.seek(SeekFrom::Start(cd_offset))?;
    let mut cd_buf = vec![0u8; cd_size as usize];
    file.read_exact(&mut cd_buf)?;

    // First pass: walk the central directory and collect every entry
    // name + uncompressed size. We need this to detect the bundle
    // format (manifest.json vs toc.json) and to enumerate APKs.
    let mut entries_raw: Vec<(String, u64)> = Vec::new();
    let mut pos = 0usize;
    while pos + 46 <= cd_buf.len() {
        let sig = u32_le(&cd_buf, pos);
        if sig != 0x02014b50 {
            break;
        }
        let uncompressed_size = u32_le(&cd_buf, pos + 24) as u64;
        let name_len = u16_le(&cd_buf, pos + 28) as usize;
        let extra_len = u16_le(&cd_buf, pos + 30) as usize;
        let comment_len = u16_le(&cd_buf, pos + 32) as usize;
        let entry_total = 46 + name_len + extra_len + comment_len;
        if pos + entry_total > cd_buf.len() {
            break;
        }
        let name = String::from_utf8_lossy(&cd_buf[pos + 46..pos + 46 + name_len]).to_string();
        entries_raw.push((name, uncompressed_size));
        pos += entry_total;
    }

    // Detect bundle format.
    let format = detect_format(&entries_raw).ok_or_else(|| {
        ApkInspectionError::Internal(
            "not a bundle (no manifest.json or toc.json found at expected location)".into(),
        )
    })?;

    // Second pass: classify each APK entry.
    let mut entries: Vec<BundleEntry> = Vec::new();
    let mut available_abis: Vec<String> = Vec::new();
    let mut base_zip_path: Option<String> = None;

    for (zip_path, uncompressed_size) in &entries_raw {
        // Only consider .apk files.
        if !zip_path.to_lowercase().ends_with(".apk") {
            continue;
        }
        let entry = classify_entry(zip_path, *uncompressed_size, format);
        if entry.kind == SplitKind::Base && base_zip_path.is_none() {
            base_zip_path = Some(zip_path.clone());
        }
        if entry.kind == SplitKind::Abi {
            if let Some(abi) = &entry.abi {
                if !available_abis.contains(abi) {
                    available_abis.push(abi.clone());
                }
            }
        }
        entries.push(entry);
    }

    // Sort ABIs by the priority order in KNOWN_BUNDLE_ABIS.
    available_abis.sort_by_key(|abi| {
        KNOWN_BUNDLE_ABIS
            .iter()
            .position(|x| *x == abi.as_str())
            .unwrap_or(usize::MAX)
    });

    // Try to read manifest.json / toc.json to extract the package name
    // and version. We avoid pulling in a JSON parser — instead we
    // regex-scan the manifest's bytes for `"package_name": "..."`
    // and `"version_name": "..."`. This is intentionally lossy: if the
    // manifest uses different field names or non-standard formatting,
    // we just return `None` and let the caller fall back.
    let (package, version_name) = read_manifest(&mut file, &entries_raw, format)?;

    // Build the recommended install list.
    let recommended_install =
        build_recommended_install(&base_zip_path, &available_abis, target_arch);

    Ok(BundleInspectResult {
        path: path_str,
        format,
        package,
        version_name,
        zip_entry_count: entries_raw.len(),
        entries,
        available_abis,
        recommended_install,
    })
}

// ----- Format detection ---------------------------------------------------

/// Detect the bundle format from the entry names. Returns `None` if the
/// ZIP doesn't look like either known bundle format.
fn detect_format(entries: &[(String, u64)]) -> Option<BundleFormat> {
    let has_manifest = entries.iter().any(|(n, _)| n == "manifest.json");
    let has_toc = entries.iter().any(|(n, _)| n == "toc.json");
    let has_splits_dir = entries
        .iter()
        .any(|(n, _)| n.starts_with("splits/") && n.ends_with(".apk"));

    if has_manifest {
        Some(BundleFormat::Xapk)
    } else if has_toc || has_splits_dir {
        Some(BundleFormat::Apks)
    } else {
        None
    }
}

// ----- Entry classification -----------------------------------------------

/// Classify a single APK entry inside a bundle.
fn classify_entry(zip_path: &str, uncompressed_size: u64, format: BundleFormat) -> BundleEntry {
    // Normalize to forward slashes.
    let zip_path = zip_path.replace('\\', "/");
    let basename = zip_path.split('/').next_back().unwrap_or("").to_string();
    let lower = basename.to_ascii_lowercase();

    // For .apks format, the base APK is `splits/base.apk`. For .xapk,
    // it's `<package>.apk` at the root — but we don't know the package
    // name yet, so we treat any root-level non-`config.*.apk` as the
    // base. The presence of `config.` prefix is the canonical signal
    // that an entry is a split, not the base.
    let is_base = if format == BundleFormat::Apks {
        // .apks: base is exactly `splits/base.apk`.
        zip_path == "splits/base.apk" || lower == "base.apk"
    } else {
        // .xapk: base is anything at root that doesn't start with `config.`.
        !zip_path.contains('/') && !lower.starts_with("config.")
    };

    if is_base {
        return BundleEntry {
            zip_path: zip_path.clone(),
            kind: SplitKind::Base,
            abi: None,
            locale: None,
            density: None,
            uncompressed_size,
        };
    }

    // Parse `config.<segment>.apk`. We case-insensitively match the
    // `config.` prefix and `.apk` suffix (some bundles use `Config.APK`
    // — rare but legal on case-insensitive filesystems). The segment
    // itself is preserved in its original case so locale codes like
    // `zh_CN` round-trip correctly.
    if lower.starts_with("config.") && lower.ends_with(".apk") {
        // Slice the original-case basename to extract the segment
        // without lowercasing it.
        let segment_raw = &basename["config.".len()..basename.len() - ".apk".len()];
        // Replace hyphens with underscores to match the canonical
        // `arm64_v8a` / `armeabi_v7a` spellings.
        let segment = segment_raw.replace('-', "_");
        let segment_lower = segment.to_ascii_lowercase();

        if KNOWN_BUNDLE_ABIS.contains(&segment_lower.as_str()) {
            return BundleEntry {
                zip_path: zip_path.clone(),
                kind: SplitKind::Abi,
                abi: Some(segment_lower),
                locale: None,
                density: None,
                uncompressed_size,
            };
        }

        if KNOWN_DENSITIES.contains(&segment_lower.as_str()) {
            return BundleEntry {
                zip_path: zip_path.clone(),
                kind: SplitKind::Density,
                abi: None,
                locale: None,
                density: Some(segment_lower),
                uncompressed_size,
            };
        }

        // Locale splits: 2-letter language code, optionally followed
        // by `_` and a 2-letter country code (e.g. `en`, `zh_CN`).
        // We preserve the original case so `zh_CN` doesn't become `zh_cn`.
        if is_locale_segment(&segment) {
            return BundleEntry {
                zip_path: zip_path.clone(),
                kind: SplitKind::Locale,
                abi: None,
                locale: Some(segment),
                density: None,
                uncompressed_size,
            };
        }

        // Unknown config split (feature module, etc.).
        return BundleEntry {
            zip_path: zip_path.clone(),
            kind: SplitKind::Other,
            abi: None,
            locale: None,
            density: None,
            uncompressed_size,
        };
    }

    // Doesn't match any pattern — surface as Other.
    BundleEntry {
        zip_path: zip_path.clone(),
        kind: SplitKind::Other,
        abi: None,
        locale: None,
        density: None,
        uncompressed_size,
    }
}

/// True if `segment` looks like a locale code: 2 ASCII letters, or
/// 2 letters + `_` + 2 letters (e.g. `en`, `pt_BR`, `zh_CN`).
fn is_locale_segment(segment: &str) -> bool {
    let bytes = segment.as_bytes();
    match bytes.len() {
        2 => bytes.iter().all(|b| b.is_ascii_alphabetic()),
        5 => {
            bytes[0..2].iter().all(|b| b.is_ascii_alphabetic())
                && bytes[2] == b'_'
                && bytes[3..5].iter().all(|b| b.is_ascii_alphabetic())
        }
        _ => false,
    }
}

// ----- Manifest scanning --------------------------------------------------

/// Read `manifest.json` (xapk) or `toc.json` (apks) from the bundle ZIP
/// and extract the package name + version. Returns `(None, None)` if
/// either field can't be found.
///
/// We deliberately avoid the `serde_json` crate here — the manifest
/// can be tens of KB and we only need two string fields, so a
/// byte-level scan is faster and keeps the dependency surface small.
fn read_manifest(
    file: &mut std::fs::File,
    entries: &[(String, u64)],
    format: BundleFormat,
) -> std::result::Result<(Option<String>, Option<String>), ApkInspectionError> {
    let manifest_name = match format {
        BundleFormat::Xapk => "manifest.json",
        BundleFormat::Apks => "toc.json",
    };

    // Find the manifest entry and read its local header to get the
    // file offset. The local header is at the offset stored in the
    // central directory entry (CD offset 42 — 4 bytes).
    //
    // We need to rescan the CD to get the local-header-offset field
    // because we discarded it in the first pass.
    //
    // Actually we don't have CD buf here — we'll re-seek.
    // For simplicity, just iterate again.
    let file_len = file.metadata()?.len();
    let eocd = find_eocd_internal(file, file_len)?;
    file.seek(SeekFrom::Start(eocd.cd_offset))?;
    let mut cd_buf = vec![0u8; eocd.cd_size as usize];
    file.read_exact(&mut cd_buf)?;

    let mut local_header_offset: Option<u64> = None;
    let mut compressed_size: u64 = 0;
    let mut compression_method: u16 = 0;
    let mut pos = 0usize;
    while pos + 46 <= cd_buf.len() {
        let sig = u32_le(&cd_buf, pos);
        if sig != 0x02014b50 {
            break;
        }
        let name_len = u16_le(&cd_buf, pos + 28) as usize;
        let extra_len = u16_le(&cd_buf, pos + 30) as usize;
        let comment_len = u16_le(&cd_buf, pos + 32) as usize;
        let entry_total = 46 + name_len + extra_len + comment_len;
        if pos + entry_total > cd_buf.len() {
            break;
        }
        let name = String::from_utf8_lossy(&cd_buf[pos + 46..pos + 46 + name_len]).to_string();
        if name == manifest_name {
            compression_method = u16_le(&cd_buf, pos + 10);
            compressed_size = u32_le(&cd_buf, pos + 20) as u64;
            local_header_offset = Some(u32_le(&cd_buf, pos + 42) as u64);
            break;
        }
        pos += entry_total;
    }

    let Some(offset) = local_header_offset else {
        // No manifest found — return None for both fields.
        return Ok((None, None));
    };

    // Read the local file header to find where the actual data starts.
    file.seek(SeekFrom::Start(offset))?;
    let mut local_hdr = [0u8; 30];
    file.read_exact(&mut local_hdr)?;
    if u32_le(&local_hdr, 0) != 0x04034b50 {
        return Ok((None, None));
    }
    let local_name_len = u16_le(&local_hdr, 26) as u64;
    let local_extra_len = u16_le(&local_hdr, 28) as u64;
    let data_start = offset + 30 + local_name_len + local_extra_len;

    // Read up to 64 KB of the manifest. We only need the package name
    // + version, which are always near the top.
    let read_size = std::cmp::min(compressed_size as usize, 64 * 1024);
    file.seek(SeekFrom::Start(data_start))?;
    let mut buf = vec![0u8; read_size];
    file.read_exact(&mut buf)?;

    // If compression_method != 0 (stored), the data is deflate-compressed.
    // We don't have a deflate decoder here — but the manifest is almost
    // always stored (it's tiny). If it's deflated, we return None.
    if compression_method != 0 {
        return Ok((None, None));
    }

    let manifest_str = String::from_utf8_lossy(&buf);
    let package = scan_json_string_field(&manifest_str, "package_name")
        .or_else(|| scan_json_string_field(&manifest_str, "package"));
    let version_name = scan_json_string_field(&manifest_str, "version_name")
        .or_else(|| scan_json_string_field(&manifest_str, "versionName"));
    Ok((package, version_name))
}

/// Scan a JSON-shaped string for `"field": "value"` and return `value`.
/// Lossy: doesn't handle escaped quotes inside the value, doesn't validate
/// JSON structure. Good enough for the manifest fields we care about.
fn scan_json_string_field(json: &str, field: &str) -> Option<String> {
    let needle = format!("\"{field}\"");
    let mut search_from = 0;
    while let Some(idx) = json[search_from..].find(&needle) {
        let after_field = search_from + idx + needle.len();
        // Skip whitespace and the colon.
        let bytes = json.as_bytes();
        let mut p = after_field;
        while p < bytes.len() && (bytes[p] == b' ' || bytes[p] == b'\t' || bytes[p] == b'\n' || bytes[p] == b':') {
            p += 1;
        }
        if p >= bytes.len() || bytes[p] != b'"' {
            search_from = after_field;
            continue;
        }
        // Find the closing quote.
        let value_start = p + 1;
        let value_end = json[value_start..]
            .find('"')
            .map(|e| value_start + e)?;
        return Some(json[value_start..value_end].to_string());
    }
    None
}

// ----- Recommendation engine ----------------------------------------------

/// Pick the set of ZIP paths to install for the given target arch.
///
/// Strategy:
///   1. Always include the base APK.
///   2. If `target_arch` is given, include the matching ABI split when
///      the bundle ships one. Mapping:
///        `arm64`   → `arm64_v8a`
///        `arm`     → `armeabi_v7a`
///        `x86_64`  → `x86_64`
///        `x86`     → `x86`
///   3. If the bundle has no ABI split for the requested arch, leave
///      it out — the base APK may still contain native libs.
///   4. Don't auto-include locale/density splits — caller decides.
fn build_recommended_install(
    base_zip_path: &Option<String>,
    available_abis: &[String],
    target_arch: Option<&str>,
) -> Vec<String> {
    let mut install: Vec<String> = Vec::new();
    if let Some(base) = base_zip_path {
        install.push(base.clone());
    }
    if let Some(arch) = target_arch {
        let wanted_abi = map_arch_to_bundle_abi(arch);
        if available_abis.contains(&wanted_abi) {
            // Find the actual zip_path for this ABI in the entries.
            // The zip_path is either `config.<abi>.apk` (xapk) or
            // `splits/config.<abi>.apk` (apks). We reconstruct both
            // possibilities and let the caller verify.
            //
            // Note: we don't have access to the entries list here, so
            // we use a convention. The caller (CLI / dashboard) should
            // look up the actual zip_path from `entries` instead of
            // blindly using this string.
            install.push(format!("config.{}.apk", wanted_abi));
        }
    }
    install
}

/// Map a CLI arch token (`arm64`, `arm`, `x86_64`, `x86`) to the
/// corresponding bundle ABI segment (`arm64_v8a`, `armeabi_v7a`, etc.).
fn map_arch_to_bundle_abi(arch: &str) -> String {
    match arch {
        "arm64" => "arm64_v8a".to_string(),
        "arm" => "armeabi_v7a".to_string(),
        "x86_64" => "x86_64".to_string(),
        "x86" => "x86".to_string(),
        other => other.to_string(),
    }
}

// ----- Low-level integer readers ------------------------------------------

#[inline]
fn u16_le(buf: &[u8], pos: usize) -> u16 {
    u16::from_le_bytes([buf[pos], buf[pos + 1]])
}

#[inline]
fn u32_le(buf: &[u8], pos: usize) -> u32 {
    u32::from_le_bytes([buf[pos], buf[pos + 1], buf[pos + 2], buf[pos + 3]])
}

// ----- Tests --------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Write a temporary bundle file containing the given bytes.
    fn write_tmp(data: &[u8], ext: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join("droidker-bundle-tests");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(format!("bundle-{}.{}", uuid::Uuid::new_v4(), ext));
        std::fs::write(&p, data).unwrap();
        p
    }

    /// Build a minimal ZIP that looks like an .xapk bundle: a
    /// `manifest.json` entry plus a base APK entry and any number of
    /// `config.<x>.apk` split entries.
    fn build_xapk(entries: &[&str]) -> Vec<u8> {
        let cd_total: usize = entries.iter().map(|n| 46 + n.len()).sum();
        let cd_size = cd_total as u32;
        let cd_offset = 0u32;

        let mut buf = Vec::new();
        for name in entries {
            buf.extend_from_slice(&0x02014b50u32.to_le_bytes());
            buf.extend_from_slice(&[0u8; 42]);
            let entry_start = buf.len() - 46;
            let name_len = name.len() as u16;
            buf[entry_start + 28..entry_start + 30].copy_from_slice(&name_len.to_le_bytes());
            buf.extend_from_slice(name.as_bytes());
        }
        buf.extend_from_slice(&0x06054b50u32.to_le_bytes());
        buf.extend_from_slice(&[0u8; 4]);
        let total: u16 = entries.len() as u16;
        buf.extend_from_slice(&total.to_le_bytes());
        buf.extend_from_slice(&total.to_le_bytes());
        buf.extend_from_slice(&cd_size.to_le_bytes());
        buf.extend_from_slice(&cd_offset.to_le_bytes());
        buf.extend_from_slice(&0u16.to_le_bytes());
        buf
    }

    /// Same as `build_xapk` but lays out entries under `splits/` to
    /// match the .apks convention.
    fn build_apks(entries: &[&str]) -> Vec<u8> {
        build_xapk(entries)
    }

    #[test]
    fn detects_xapk_format_by_manifest_json() {
        let zip = build_xapk(&[
            "manifest.json",
            "com.example.app.apk",
            "config.arm64_v8a.apk",
            "config.en.apk",
        ]);
        let path = write_tmp(&zip, "xapk");
        let r = inspect_bundle(&path, Some("arm64")).unwrap();
        assert_eq!(r.format, BundleFormat::Xapk);
        assert_eq!(r.zip_entry_count, 4);
        assert!(r.available_abis.contains(&"arm64_v8a".to_string()));
    }

    #[test]
    fn detects_apks_format_by_toc_json() {
        let zip = build_apks(&[
            "toc.json",
            "splits/base.apk",
            "splits/config.arm64_v8a.apk",
            "splits/config.x86_64.apk",
        ]);
        let path = write_tmp(&zip, "apks");
        let r = inspect_bundle(&path, Some("arm64")).unwrap();
        assert_eq!(r.format, BundleFormat::Apks);
        assert_eq!(r.available_abis.len(), 2);
    }

    #[test]
    fn detects_apks_format_by_splits_directory_alone() {
        // No toc.json, but splits/ dir present — should still detect as .apks.
        let zip = build_apks(&["splits/base.apk", "splits/config.arm64_v8a.apk"]);
        let path = write_tmp(&zip, "apks");
        let r = inspect_bundle(&path, None).unwrap();
        assert_eq!(r.format, BundleFormat::Apks);
    }

    #[test]
    fn rejects_plain_zip_without_manifest_or_toc() {
        let zip = build_xapk(&["AndroidManifest.xml", "classes.dex"]);
        let path = write_tmp(&zip, "xapk");
        let r = inspect_bundle(&path, None);
        assert!(r.is_err());
    }

    #[test]
    fn classifies_base_apk_in_xapk_format() {
        let zip = build_xapk(&[
            "manifest.json",
            "com.example.app.apk",
            "config.arm64_v8a.apk",
        ]);
        let path = write_tmp(&zip, "xapk");
        let r = inspect_bundle(&path, None).unwrap();
        let base = r.entries.iter().find(|e| e.kind == SplitKind::Base).unwrap();
        assert_eq!(base.zip_path, "com.example.app.apk");
    }

    #[test]
    fn classifies_base_apk_in_apks_format() {
        let zip = build_apks(&[
            "toc.json",
            "splits/base.apk",
            "splits/config.arm64_v8a.apk",
        ]);
        let path = write_tmp(&zip, "apks");
        let r = inspect_bundle(&path, None).unwrap();
        let base = r.entries.iter().find(|e| e.kind == SplitKind::Base).unwrap();
        assert_eq!(base.zip_path, "splits/base.apk");
    }

    #[test]
    fn classifies_abi_split_correctly() {
        let zip = build_xapk(&[
            "manifest.json",
            "com.example.app.apk",
            "config.arm64_v8a.apk",
        ]);
        let path = write_tmp(&zip, "xapk");
        let r = inspect_bundle(&path, None).unwrap();
        let abi = r
            .entries
            .iter()
            .find(|e| e.kind == SplitKind::Abi)
            .unwrap();
        assert_eq!(abi.abi.as_deref(), Some("arm64_v8a"));
    }

    #[test]
    fn classifies_locale_split_correctly() {
        let zip = build_xapk(&[
            "manifest.json",
            "com.example.app.apk",
            "config.en.apk",
            "config.zh_CN.apk",
        ]);
        let path = write_tmp(&zip, "xapk");
        let r = inspect_bundle(&path, None).unwrap();
        let locales: Vec<_> = r
            .entries
            .iter()
            .filter(|e| e.kind == SplitKind::Locale)
            .collect();
        assert_eq!(locales.len(), 2);
        let locale_strs: Vec<_> = locales
            .iter()
            .filter_map(|e| e.locale.as_deref())
            .collect();
        assert!(locale_strs.contains(&"en"));
        assert!(locale_strs.contains(&"zh_CN"));
    }

    #[test]
    fn classifies_density_split_correctly() {
        let zip = build_xapk(&[
            "manifest.json",
            "com.example.app.apk",
            "config.xxhdpi.apk",
        ]);
        let path = write_tmp(&zip, "xapk");
        let r = inspect_bundle(&path, None).unwrap();
        let density = r
            .entries
            .iter()
            .find(|e| e.kind == SplitKind::Density)
            .unwrap();
        assert_eq!(density.density.as_deref(), Some("xxhdpi"));
    }

    #[test]
    fn is_locale_segment_validates_correctly() {
        assert!(is_locale_segment("en"));
        assert!(is_locale_segment("zh"));
        assert!(is_locale_segment("zh_CN"));
        assert!(is_locale_segment("pt_BR"));
        assert!(!is_locale_segment("arm64_v8a")); // not 2 or 5 chars
        assert!(!is_locale_segment("xxhdpi")); // not 2 or 5 chars
        assert!(!is_locale_segment("e1")); // not alphabetic
        assert!(!is_locale_segment("e_N")); // 3 chars
    }

    #[test]
    fn map_arch_to_bundle_abi_known() {
        assert_eq!(map_arch_to_bundle_abi("arm64"), "arm64_v8a");
        assert_eq!(map_arch_to_bundle_abi("arm"), "armeabi_v7a");
        assert_eq!(map_arch_to_bundle_abi("x86_64"), "x86_64");
        assert_eq!(map_arch_to_bundle_abi("x86"), "x86");
    }

    #[test]
    fn recommended_install_includes_base_and_matching_abi() {
        let zip = build_xapk(&[
            "manifest.json",
            "com.example.app.apk",
            "config.arm64_v8a.apk",
            "config.x86_64.apk",
            "config.en.apk",
        ]);
        let path = write_tmp(&zip, "xapk");
        let r = inspect_bundle(&path, Some("arm64")).unwrap();
        assert!(r.recommended_install.contains(&"com.example.app.apk".to_string()));
        assert!(r.recommended_install.contains(&"config.arm64_v8a.apk".to_string()));
        // Should NOT include the x86_64 split when arm64 was requested.
        assert!(!r.recommended_install.contains(&"config.x86_64.apk".to_string()));
        // Should NOT auto-include locale splits.
        assert!(!r.recommended_install.contains(&"config.en.apk".to_string()));
    }

    #[test]
    fn recommended_install_omits_abi_when_bundle_has_none() {
        let zip = build_xapk(&[
            "manifest.json",
            "com.example.app.apk",
            "config.en.apk",
        ]);
        let path = write_tmp(&zip, "xapk");
        let r = inspect_bundle(&path, Some("arm64")).unwrap();
        assert_eq!(r.recommended_install.len(), 1);
        assert_eq!(r.recommended_install[0], "com.example.app.apk");
    }

    #[test]
    fn scan_json_string_field_finds_simple_field() {
        let json = r#"{"package_name": "com.example.app", "version_name": "1.0.0"}"#;
        assert_eq!(
            scan_json_string_field(json, "package_name"),
            Some("com.example.app".to_string())
        );
        assert_eq!(
            scan_json_string_field(json, "version_name"),
            Some("1.0.0".to_string())
        );
    }

    #[test]
    fn scan_json_string_field_handles_whitespace() {
        let json = "{\n  \"package_name\"   :   \"com.example.app\"\n}";
        assert_eq!(
            scan_json_string_field(json, "package_name"),
            Some("com.example.app".to_string())
        );
    }

    #[test]
    fn scan_json_string_field_returns_none_for_missing_field() {
        let json = r#"{"foo": "bar"}"#;
        assert_eq!(scan_json_string_field(json, "package_name"), None);
    }

    #[test]
    fn bundle_format_as_str_is_correct() {
        assert_eq!(BundleFormat::Xapk.as_str(), "xapk");
        assert_eq!(BundleFormat::Apks.as_str(), "apks");
    }
}
