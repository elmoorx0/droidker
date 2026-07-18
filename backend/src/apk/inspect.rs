// src/apk/inspect.rs
//
// Minimal APK (ZIP) inspector — reads only the ZIP central directory to
// enumerate file names, then extracts the list of native ABIs shipped
// under `lib/<abi>/*.so`.
//
// Why hand-rolled instead of the `zip` crate?
//
//   * We only need file *names*, not contents — adding 200 KB of
//     `zip` + `flate2` to the binary just to read a few hundred bytes
//     of central-directory records is wasteful on a 1-GB VPS.
//   * The ZIP central directory format is well-defined and stable
//     (PKZIP 2.04g, 1993). A 150-LOC parser is plenty.
//   * No decompression needed = no compression library required.
//
// Format reference (PKZIP APPNOTE 6.3.10):
//
//   End-of-central-directory record (EOCD):
//     offset 0  u32  signature  0x06054b50
//     offset 4  u16  disk number
//     offset 6  u16  disk where CD starts
//     offset 8  u16  num CD records on this disk
//     offset 10 u16  total num CD records
//     offset 12 u32  size of central directory
//     offset 16 u32  offset of central directory
//     offset 20 u16  comment length
//     offset 22 ..   comment
//
//   Central directory file header:
//     offset 0  u32  signature  0x02014b50
//     offset 4  u16  version made by
//     offset 6  u16  version needed
//     offset 8  u16  general purpose bit flag
//     offset 10 u16  compression method
//     offset 12 u16  last mod time
//     offset 14 u16  last mod date
//     offset 16 u32  CRC-32
//     offset 20 u32  compressed size
//     offset 24 u32  uncompressed size
//     offset 28 u16  file name length
//     offset 30 u16  extra field length
//     offset 32 u16  file comment length
//     offset 34 u16  disk number start
//     offset 36 u16  internal attrs
//     offset 38 u32  external attrs
//     offset 42 u32  local header offset
//     offset 46 ..   file name (file_name_length bytes)
//                    extra field (extra_field_length bytes)
//                    file comment (file_comment_length bytes)
//
// We read entries sequentially starting from the CD offset recorded in
// the EOCD, stopping when we hit a non-signature byte.

use crate::error::{DroidkerError, Result};
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

/// ABIs that APKs may legally ship under `lib/<abi>/`.
///
/// Order matters: it's the priority order we recommend to the caller
/// when an APK ships multiple ABIs (most apps ship arm64-v8a +
/// armeabi-v7a + x86_64, and we want to pick arm64-v8a on ARM hosts
/// and x86_64 on x86_64 hosts when both are present).
pub const KNOWN_ABIS: &[&str] = &[
    "arm64-v8a",
    "armeabi-v7a",
    "x86_64",
    "x86",
    "armeabi",
    "mips",
    "mips64",
];

/// Information about a single ABI shipped by an APK.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ApkAbiInfo {
    /// ABI directory name (e.g. `arm64-v8a`).
    pub abi: String,
    /// Number of `.so` files under `lib/<abi>/`.
    pub so_count: usize,
    /// Total uncompressed size of those `.so` files (bytes).
    /// Useful as a hint for "is this ABI worth installing?".
    pub total_uncompressed_bytes: u64,
}

/// Result of inspecting an APK file.
#[derive(Debug, Clone, serde::Serialize)]
pub struct InspectResult {
    /// Path of the APK that was inspected.
    pub path: String,
    /// Total number of entries in the ZIP central directory.
    pub zip_entry_count: usize,
    /// ABIs found under `lib/<abi>/*.so`, in the priority order defined
    /// by `KNOWN_ABIS`. ABIs not in the known list are appended at the
    /// end in alphabetic order.
    pub abis: Vec<ApkAbiInfo>,
    /// True if the APK has no `lib/` directory at all (i.e. it's a
    /// pure-Java/Kotlin app with no native code). For such APKs the
    /// target arch doesn't matter — the host's native arch is fine.
    pub has_no_native_libs: bool,
    /// Best arch recommendation (CLI string form). Picked by:
    ///   1. If `abis` is empty → `None` (caller should default to host).
    ///   2. Else the first ABI in `abis` (which is already sorted by
    ///      `KNOWN_ABIS` priority).
    pub recommended_arch: Option<String>,
}

/// Errors specific to APK inspection.
#[derive(Debug, thiserror::Error)]
pub enum ApkInspectionError {
    #[error("APK file not found: {0}")]
    NotFound(String),
    #[error("APK is too small to be a valid ZIP ({0} bytes)")]
    TooSmall(u64),
    #[error("EOCD signature not found in last {0} bytes of APK")]
    NoEocd(usize),
    #[error("central directory offset {cd_offset} + size {cd_size} exceeds file length {file_len}")]
    CdOutOfBounds {
        cd_offset: u64,
        cd_size: u64,
        file_len: u64,
    },
    #[error("truncated central directory entry at offset {0}")]
    TruncatedEntry(u64),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

impl From<ApkInspectionError> for DroidkerError {
    fn from(e: ApkInspectionError) -> Self {
        DroidkerError::Internal(format!("apk inspect: {e}"))
    }
}

// ----- ZIP signatures -----------------------------------------------------

const EOCD_SIGNATURE: u32 = 0x06054b50;
const CD_HEADER_SIGNATURE: u32 = 0x02014b50;
/// Max size of the EOCD comment field per PKZIP spec.
const EOCD_COMMENT_MAX: usize = 0xFFFF;
/// Total size of the fixed EOCD record (without the comment).
const EOCD_FIXED_SIZE: usize = 22;
/// Total size of the fixed central-directory header (without name/extra/comment).
const CD_FIXED_SIZE: usize = 46;

// ----- Public entry point -------------------------------------------------

/// Inspect an APK file and return its native-ABI manifest.
///
/// This is a *pure read* operation: we open the file, seek to the end,
/// walk the central directory, and close. We do not decompress, validate
/// signatures, or touch the Android manifest. That makes it safe to run
/// on untrusted APKs (e.g. uploaded by a multi-tenant dashboard user).
pub fn inspect_apk<P: AsRef<Path>>(apk_path: P) -> std::result::Result<InspectResult, ApkInspectionError> {
    let path = apk_path.as_ref();
    if !path.exists() {
        return Err(ApkInspectionError::NotFound(path.display().to_string()));
    }
    let path_str = path.display().to_string();

    let mut file = std::fs::File::open(path)?;
    let file_len = file.metadata()?.len();
    if file_len < (EOCD_FIXED_SIZE as u64) {
        return Err(ApkInspectionError::TooSmall(file_len));
    }

    // --- 1. Find the EOCD record by scanning backwards from EOF -----------
    let eocd = find_eocd(&mut file, file_len)?;

    // --- 2. Read the central directory ------------------------------------
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

    // Buffer the central directory in one shot — APK CD is typically a
    // few hundred KB even for large apps, so a single read is fine.
    let mut cd_buf = vec![0u8; cd_size as usize];
    file.read_exact(&mut cd_buf)?;

    // --- 3. Walk entries, collecting lib/<abi>/*.so hits ------------------
    let mut entries_walked = 0usize;
    let mut abi_map: std::collections::HashMap<String, ApkAbiInfo> =
        std::collections::HashMap::new();
    let mut has_any_lib_entry = false;

    let mut pos = 0usize;
    while pos + CD_FIXED_SIZE <= cd_buf.len() {
        let sig = u32_le(&cd_buf, pos);
        if sig != CD_HEADER_SIGNATURE {
            // End of central directory (or corruption) — stop walking.
            break;
        }
        let uncompressed_size = u32_le(&cd_buf, pos + 24) as u64;
        let name_len = u16_le(&cd_buf, pos + 28) as usize;
        let extra_len = u16_le(&cd_buf, pos + 30) as usize;
        let comment_len = u16_le(&cd_buf, pos + 32) as usize;
        let entry_total = CD_FIXED_SIZE + name_len + extra_len + comment_len;
        if pos + entry_total > cd_buf.len() {
            return Err(ApkInspectionError::TruncatedEntry(pos as u64));
        }
        let name_bytes = &cd_buf[pos + CD_FIXED_SIZE..pos + CD_FIXED_SIZE + name_len];
        let name = String::from_utf8_lossy(name_bytes).to_string();

        if let Some((abi, _so_name)) = parse_lib_entry(&name) {
            has_any_lib_entry = true;
            let info = abi_map.entry(abi.clone()).or_insert_with(|| ApkAbiInfo {
                abi: abi.clone(),
                so_count: 0,
                total_uncompressed_bytes: 0,
            });
            info.so_count += 1;
            info.total_uncompressed_bytes += uncompressed_size;
        }

        entries_walked += 1;
        pos += entry_total;
    }

    // --- 4. Sort ABIs by KNOWN_ABIS priority, then alphabetic -------------
    let mut abis: Vec<ApkAbiInfo> = abi_map.into_values().collect();
    abis.sort_by(|a, b| {
        let pa = KNOWN_ABIS.iter().position(|x| *x == a.abi);
        let pb = KNOWN_ABIS.iter().position(|x| *x == b.abi);
        match (pa, pb) {
            (Some(i), Some(j)) => i.cmp(&j),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => a.abi.cmp(&b.abi),
        }
    });

    let recommended_arch = abis.first().map(|i| map_abi_to_arch_token(&i.abi));
    let has_no_native_libs = !has_any_lib_entry;

    Ok(InspectResult {
        path: path_str,
        zip_entry_count: entries_walked,
        abis,
        has_no_native_libs,
        recommended_arch,
    })
}

// ----- Helpers ------------------------------------------------------------

/// Parse a ZIP entry name like `lib/arm64-v8a/libfoo.so` into
/// `(abi, so_name)`. Returns `None` for entries that aren't under `lib/<abi>/`.
///
/// Also accepts the legacy `libs/<abi>/` spelling used by some old NDK
/// build scripts.
fn parse_lib_entry(name: &str) -> Option<(String, String)> {
    // Normalize backslashes (some old APKs use Windows-style separators).
    let name = name.replace('\\', "/");
    let parts: Vec<&str> = name.split('/').collect();
    if parts.len() < 3 {
        return None;
    }
    if parts[0] != "lib" && parts[0] != "libs" {
        return None;
    }
    let abi = parts[1];
    // The third component must end in `.so` to count as a native lib.
    // (Some APKs ship `lib/<abi>/` with placeholder files like `README`;
    // we don't want to count those as a real ABI presence.)
    let so_name = parts[2..].join("/");
    if !so_name.ends_with(".so") {
        return None;
    }
    // Reject empty / whitespace-only ABI names (e.g. `lib//libfoo.so`).
    if abi.is_empty() {
        return None;
    }
    // Sanity-check the ABI name: only accept alphanumeric + `-_`.
    if !abi
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return None;
    }
    Some((abi.to_string(), so_name))
}

/// Map an APK ABI directory name to the canonical CLI arch token used
/// by `droidker run --arch <ARCH>` and the container model.
///
/// Returns one of: `arm`, `arm64`, `x86`, `x86_64`. Unknown ABIs map
/// to themselves (so the caller can decide what to do).
fn map_abi_to_arch_token(abi: &str) -> String {
    match abi {
        "arm64-v8a" | "arm64" => "arm64".to_string(),
        "armeabi-v7a" | "armeabi" => "arm".to_string(),
        "x86_64" => "x86_64".to_string(),
        "x86" | "i686" => "x86".to_string(),
        other => other.to_string(),
    }
}

struct EocdRecord {
    cd_offset: u64,
    cd_size: u32,
}

/// Find the EOCD record by scanning the last 65 KB of the file for the
/// signature. Per spec, the EOCD can be preceded by up to 64 KB of
/// archive comment, plus the 22-byte EOCD itself.
fn find_eocd(file: &mut std::fs::File, file_len: u64) -> std::result::Result<EocdRecord, ApkInspectionError> {
    let scan_size = std::cmp::min(file_len as usize, EOCD_COMMENT_MAX + EOCD_FIXED_SIZE);
    file.seek(SeekFrom::End(-(scan_size as i64)))?;
    let mut tail = vec![0u8; scan_size];
    file.read_exact(&mut tail)?;

    // Scan backwards from the end for the EOCD signature. Scanning
    // backwards (rather than forwards) finds the *last* occurrence,
    // which is the correct one if the comment itself happens to contain
    // the signature bytes.
    let sig_bytes = EOCD_SIGNATURE.to_le_bytes();
    for i in (0..=tail.len().saturating_sub(EOCD_FIXED_SIZE)).rev() {
        if tail[i..i + 4] == sig_bytes {
            let cd_size = u32_le(&tail, i + 12);
            let cd_offset = u32_le(&tail, i + 16) as u64;
            return Ok(EocdRecord { cd_offset, cd_size });
        }
    }
    Err(ApkInspectionError::NoEocd(scan_size))
}

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

    /// Build a tiny in-memory "APK" ZIP that the inspector can parse.
    /// We construct the bare minimum: EOCD + a single central directory
    /// entry pointing at a (non-existent) local header. The inspector
    /// only walks the central directory, so local headers don't matter.
    fn build_minimal_apk(entries: &[(&str, u64)]) -> Vec<u8> {
        // Each CD entry: signature(4) + 42 bytes of header fields + name.
        // We zero all header fields except uncompressed_size (offset 24)
        // and name_len (offset 28).
        let cd_total: usize = entries
            .iter()
            .map(|(name, _)| CD_FIXED_SIZE + name.len())
            .sum();
        let cd_offset = 0u32; // we put CD at the start of the file
        let cd_size = cd_total as u32;

        let mut buf = Vec::with_capacity(cd_total + EOCD_FIXED_SIZE);
        for (name, size) in entries {
            buf.extend_from_slice(&CD_HEADER_SIGNATURE.to_le_bytes());
            buf.extend_from_slice(&[0u8; 42]); // 42 bytes of zeroed header
            // Patch uncompressed_size at offset 24 (relative to entry start).
            // Note: `buf.len()` here is one past the entry's fixed header end,
            // so we compute the entry's start as `buf.len() - CD_FIXED_SIZE`.
            let entry_start = buf.len() - CD_FIXED_SIZE;
            buf[entry_start + 24..entry_start + 28]
                .copy_from_slice(&(*size as u32).to_le_bytes());
            // Patch name_len at offset 28 so the parser knows how many
            // bytes to consume as the file name.
            let name_len = name.len() as u16;
            buf[entry_start + 28..entry_start + 30].copy_from_slice(&name_len.to_le_bytes());
            buf.extend_from_slice(name.as_bytes());
        }
        // EOCD
        buf.extend_from_slice(&EOCD_SIGNATURE.to_le_bytes());
        buf.extend_from_slice(&[0u8; 4]); // disk numbers
        // Total + on-disk counts.
        let total: u16 = entries.len() as u16;
        let on_disk: u16 = entries.len() as u16;
        buf.extend_from_slice(&on_disk.to_le_bytes());
        buf.extend_from_slice(&total.to_le_bytes());
        buf.extend_from_slice(&cd_size.to_le_bytes());
        buf.extend_from_slice(&cd_offset.to_le_bytes());
        buf.extend_from_slice(&0u16.to_le_bytes()); // comment length
        buf
    }

    fn write_tmp(data: &[u8]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join("droidker-apk-tests");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(format!("test-{}.apk", uuid::Uuid::new_v4()));
        std::fs::write(&p, data).unwrap();
        p
    }

    #[test]
    fn inspect_finds_all_four_common_abis() {
        let apk = build_minimal_apk(&[
            ("lib/arm64-v8a/libfoo.so", 100_000),
            ("lib/armeabi-v7a/libfoo.so", 80_000),
            ("lib/x86_64/libfoo.so", 120_000),
            ("lib/x86/libfoo.so", 90_000),
            ("AndroidManifest.xml", 4096),
            ("classes.dex", 200_000),
        ]);
        let path = write_tmp(&apk);
        let r = inspect_apk(&path).unwrap();
        assert_eq!(r.zip_entry_count, 6);
        assert_eq!(r.abis.len(), 4);
        // arm64-v8a should come first (priority order).
        assert_eq!(r.abis[0].abi, "arm64-v8a");
        assert_eq!(r.abis[0].so_count, 1);
        assert_eq!(r.abis[0].total_uncompressed_bytes, 100_000);
        // Then armeabi-v7a, x86_64, x86.
        assert_eq!(r.abis[1].abi, "armeabi-v7a");
        assert_eq!(r.abis[2].abi, "x86_64");
        assert_eq!(r.abis[3].abi, "x86");
        assert!(!r.has_no_native_libs);
        assert_eq!(r.recommended_arch.as_deref(), Some("arm64"));
    }

    #[test]
    fn inspect_handles_no_lib_directory() {
        let apk = build_minimal_apk(&[
            ("AndroidManifest.xml", 4096),
            ("classes.dex", 200_000),
            ("resources.arsc", 50_000),
        ]);
        let path = write_tmp(&apk);
        let r = inspect_apk(&path).unwrap();
        assert_eq!(r.zip_entry_count, 3);
        assert!(r.abis.is_empty());
        assert!(r.has_no_native_libs);
        assert!(r.recommended_arch.is_none());
    }

    #[test]
    fn inspect_ignores_non_so_files_under_lib() {
        let apk = build_minimal_apk(&[
            ("lib/arm64-v8a/README.txt", 100),
            ("lib/arm64-v8a/libreal.so", 50_000),
            ("lib/armeabi-v7a/", 0),
        ]);
        let path = write_tmp(&apk);
        let r = inspect_apk(&path).unwrap();
        // Only the .so file counts.
        assert_eq!(r.abis.len(), 1);
        assert_eq!(r.abis[0].abi, "arm64-v8a");
        assert_eq!(r.abis[0].so_count, 1);
    }

    #[test]
    fn inspect_aggregates_multiple_so_per_abi() {
        let apk = build_minimal_apk(&[
            ("lib/arm64-v8a/libfoo.so", 100_000),
            ("lib/arm64-v8a/libbar.so", 200_000),
            ("lib/arm64-v8a/libbaz.so", 300_000),
        ]);
        let path = write_tmp(&apk);
        let r = inspect_apk(&path).unwrap();
        assert_eq!(r.abis.len(), 1);
        assert_eq!(r.abis[0].abi, "arm64-v8a");
        assert_eq!(r.abis[0].so_count, 3);
        assert_eq!(r.abis[0].total_uncompressed_bytes, 600_000);
    }

    #[test]
    fn inspect_rejects_too_small_file() {
        let path = write_tmp(&[0u8; 10]);
        let r = inspect_apk(&path);
        assert!(matches!(r, Err(ApkInspectionError::TooSmall(_))));
    }

    #[test]
    fn inspect_rejects_missing_eocd() {
        // 1 KB of zeros — has the size but no EOCD signature.
        let path = write_tmp(&[0u8; 1024]);
        let r = inspect_apk(&path);
        assert!(matches!(r, Err(ApkInspectionError::NoEocd(_))));
    }

    #[test]
    fn inspect_recommends_x86_64_when_only_x86_64_present() {
        let apk = build_minimal_apk(&[("lib/x86_64/libfoo.so", 100_000)]);
        let path = write_tmp(&apk);
        let r = inspect_apk(&path).unwrap();
        assert_eq!(r.recommended_arch.as_deref(), Some("x86_64"));
    }

    #[test]
    fn inspect_recommends_arm_when_only_armv7_present() {
        let apk = build_minimal_apk(&[("lib/armeabi-v7a/libfoo.so", 100_000)]);
        let path = write_tmp(&apk);
        let r = inspect_apk(&path).unwrap();
        assert_eq!(r.recommended_arch.as_deref(), Some("arm"));
    }

    #[test]
    fn inspect_handles_legacy_libs_prefix() {
        let apk = build_minimal_apk(&[("libs/arm64-v8a/libfoo.so", 100_000)]);
        let path = write_tmp(&apk);
        let r = inspect_apk(&path).unwrap();
        assert_eq!(r.abis.len(), 1);
        assert_eq!(r.abis[0].abi, "arm64-v8a");
    }

    #[test]
    fn parse_lib_entry_rejects_garbage() {
        assert!(parse_lib_entry("classes.dex").is_none());
        assert!(parse_lib_entry("META-INF/MANIFEST.MF").is_none());
        assert!(parse_lib_entry("lib/arm64-v8a/").is_none()); // no .so name
        assert!(parse_lib_entry("lib//libfoo.so").is_none()); // empty ABI
        assert!(parse_lib_entry("lib/arm64-v8a/README.txt").is_none()); // not .so
        assert!(parse_lib_entry("lib/arm64-v8a/libfoo.so").is_some());
    }

    #[test]
    fn map_abi_to_arch_token_known() {
        assert_eq!(map_abi_to_arch_token("arm64-v8a"), "arm64");
        assert_eq!(map_abi_to_arch_token("armeabi-v7a"), "arm");
        assert_eq!(map_abi_to_arch_token("x86_64"), "x86_64");
        assert_eq!(map_abi_to_arch_token("x86"), "x86");
    }

    #[test]
    fn map_abi_to_arch_token_unknown_passthrough() {
        // Unknown ABIs pass through unchanged so the caller can decide.
        assert_eq!(map_abi_to_arch_token("riscv64"), "riscv64");
    }
}
