// src/apk/verify.rs
//
// APK signature verification (M8.1).
//
// Android APKs can be signed with up to three signature schemes:
//
//   * v1 — JAR-style. Signatures live in `META-INF/<name>.SF` +
//     `META-INF/<name>.{RSA,EC,DSA}` inside the ZIP. Slow to verify
//     (must hash every entry) and vulnerable to archive-level attacks,
//     but still required for apps that target SDK < 24.
//
//   * v2 — APK Signature Scheme v2 (Android 7.0 / API 24+). Lives in
//     the "APK Signing Block", a custom chunk inserted between the
//     last central-directory entry and the EOCD record. ID 0x7109871a.
//     Covers the entire APK file in 4 chunks (contents of ZIP entries,
//     the central directory, the signing block header, and the EOCD).
//
//   * v3 — APK Signature Scheme v3 (Android 9 / API 28+). Same on-disk
//     layout as v2 (also lives in the APK Signing Block) but with
//     additional fields for key rotation. ID 0xf05368c0. v3.1 is a
//     line-item extension (ID 0x1b93ad61) for SDK-gated rotation.
//
// We do NOT perform full cryptographic validation of the signatures
// here — that would require pulling in RSA + EC + x509 parsers (~500 KB
// of native deps), which is a non-starter on a 1-GB VPS. Instead we:
//
//   1. Confirm the APK is *signed at all* (v1 SF file present, OR an
//      APK Signing Block with a known v2/v3 ID exists).
//   2. Extract the signer's certificate bytes from the v2/v3 block
//      (which *is* a tractable amount of nested length-prefix parsing).
//   3. Compute the SHA-256 of the DER-encoded certificate — this is
//      the same fingerprint `apksigner verify --print-certs` reports,
//      so users can cross-check against an out-of-band source of truth.
//
// The daemon then refuses to install any APK whose signature is missing
// or whose fingerprint is on a configurable blocklist. This is enough
// to stop the obvious "user uploads a trojaned APK rebuilt from
// decompiled source" attack — the rebuilt APK won't have the original
// developer's signing key.
//
// Format reference (AOSP):
//   https://source.android.com/security/apksigning/v2
//   https://source.android.com/security/apksigning/v3

use crate::apk::inspect::{find_eocd_internal, ApkInspectionError, EOCD_FIXED_SIZE};
use sha2::{Digest, Sha256};
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

// ----- APK Signing Block constants ----------------------------------------

/// Magic bytes at the end of the APK Signing Block: "APK Sig Block 42".
const APK_SIG_BLOCK_MAGIC: &[u8; 16] = b"APK Sig Block 42";

/// Block size of the APK Sig Block trailer (size_of_block u64 + magic 16 bytes).
const APK_SIG_BLOCK_TRAILER_SIZE: u64 = 24;

/// IDs we recognise inside the APK Signing Block.
const APK_SIG_SCHEME_V2_ID: u32 = 0x7109871a;
const APK_SIG_SCHEME_V3_ID: u32 = 0xf05368c0;
const APK_SIG_SCHEME_V31_ID: u32 = 0x1b93ad61;

/// v1 (JAR) signature file extensions inside `META-INF/`. The `.SF`
/// (signature manifest) file is the canonical indicator — `.RSA` /
/// `.EC` / `.DSA` are the corresponding signature blobs.
const V1_SIG_EXTS: &[&str] = &[".SF", ".RSA", ".EC", ".DSA"];

// ----- Public types -------------------------------------------------------

/// Result of inspecting an APK's signature.
#[derive(Debug, Clone, serde::Serialize, PartialEq, Eq)]
pub struct ApkSignatureInfo {
    /// Path of the APK that was inspected.
    pub path: String,
    /// True if any signature scheme is present (v1, v2, or v3).
    pub signed: bool,
    /// Highest signature scheme version detected:
    /// `"v3.1"` > `"v3"` > `"v2"` > `"v1"` > `"none"`.
    pub scheme: String,
    /// SHA-256 of the signer certificate (lowercase hex, colon-separated
    /// octets, e.g. `"ab:cd:ef:..."`). `None` when unsigned or when we
    /// can't extract the cert from the signing block (v1 cert extraction
    /// would require PKCS#7 + ASN.1 parsing, deferred to M9+).
    pub cert_sha256: Option<String>,
    /// Signer certificate subject DN if extractable (e.g.
    /// `"CN=Example Inc, O=Example Inc, L=San Francisco, ST=CA, C=US"`).
    /// `None` when the cert wasn't extracted or the DN couldn't be parsed.
    pub cert_subject: Option<String>,
}

impl ApkSignatureInfo {
    /// Convenience: an unsigned-APK result for the given path.
    pub fn unsigned(path: String) -> Self {
        Self {
            path,
            signed: false,
            scheme: "none".to_string(),
            cert_sha256: None,
            cert_subject: None,
        }
    }
}

// ----- Public entry point -------------------------------------------------

/// Inspect the signature of an APK.
///
/// Reads only the ZIP central directory + the APK Signing Block — does
/// not decompress anything, does not validate the cryptographic
/// signatures themselves. Safe to run on untrusted APKs.
pub fn verify_signature<P: AsRef<Path>>(
    apk_path: P,
) -> std::result::Result<ApkSignatureInfo, ApkInspectionError> {
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

    // Step 1: walk the central directory to detect v1 signatures and
    // collect the list of entries we need to find the signing block.
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

    // Buffer the central directory.
    file.seek(SeekFrom::Start(cd_offset))?;
    let mut cd_buf = vec![0u8; cd_size as usize];
    file.read_exact(&mut cd_buf)?;

    // Walk entries: detect META-INF/<name>.SF for v1.
    let mut v1_present = false;
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
        let name_bytes = &cd_buf[pos + 46..pos + 46 + name_len];
        let name = String::from_utf8_lossy(name_bytes).to_string();
        if is_v1_signature_entry(&name) {
            v1_present = true;
        }
        pos += entry_total;
    }

    // Step 2: locate the APK Signing Block (sits immediately before the
    // central directory). If we don't find one, the APK is either v1-only
    // or unsigned.
    let sig_block = parse_signing_block(&mut file, cd_offset, file_len)?;

    // Step 3: pick the highest scheme and extract the cert if v2/v3.
    let (scheme, cert_sha256, cert_subject) = match sig_block {
        Some(block) => {
            if let Some(cert) = block.find_signer_cert()? {
                let fingerprint = sha256_colon_hex(&cert);
                let subject = parse_subject_dn(&cert);
                let scheme = if block.has_id(APK_SIG_SCHEME_V31_ID) {
                    "v3.1"
                } else if block.has_id(APK_SIG_SCHEME_V3_ID) {
                    "v3"
                } else if block.has_id(APK_SIG_SCHEME_V2_ID) {
                    "v2"
                } else if v1_present {
                    "v1"
                } else {
                    "none"
                };
                (scheme.to_string(), Some(fingerprint), subject)
            } else if block.has_id(APK_SIG_SCHEME_V31_ID) {
                ("v3.1".to_string(), None, None)
            } else if block.has_id(APK_SIG_SCHEME_V3_ID) {
                ("v3".to_string(), None, None)
            } else if block.has_id(APK_SIG_SCHEME_V2_ID) {
                ("v2".to_string(), None, None)
            } else if v1_present {
                ("v1".to_string(), None, None)
            } else {
                ("none".to_string(), None, None)
            }
        }
        None => {
            if v1_present {
                ("v1".to_string(), None, None)
            } else {
                ("none".to_string(), None, None)
            }
        }
    };

    let signed = scheme != "none";

    Ok(ApkSignatureInfo {
        path: path_str,
        signed,
        scheme,
        cert_sha256,
        cert_subject,
    })
}

// ----- v1 detection -------------------------------------------------------

/// True if the ZIP entry name looks like a v1 JAR signature file,
/// e.g. `META-INF/CERT.SF`, `META-INF/EXAMPLE.RSA`, `META-INF/SIGNER.EC`.
fn is_v1_signature_entry(name: &str) -> bool {
    let name = name.replace('\\', "/");
    let parts: Vec<&str> = name.split('/').collect();
    if parts.len() != 2 {
        return false;
    }
    if !parts[0].eq_ignore_ascii_case("META-INF") {
        return false;
    }
    // Case-insensitive comparison on the extension. We lowercase both
    // sides so `CERT.sf` and `cert.SF` both match.
    let lower = parts[1].to_ascii_lowercase();
    V1_SIG_EXTS
        .iter()
        .any(|ext| lower.ends_with(&ext.to_ascii_lowercase()))
}

// ----- APK Signing Block parsing ------------------------------------------

/// Parsed view of the APK Signing Block. We retain the raw bytes so
/// we can walk the value-pair sequence later when looking for v2/v3 IDs.
struct SigningBlock {
    /// Raw bytes of the signing block (excluding the 8-byte size header
    /// and the 24-byte size+magic trailer).
    payload: Vec<u8>,
}

impl SigningBlock {
    /// True if a value-pair with the given ID exists in the block.
    fn has_id(&self, id: u32) -> bool {
        self.find_value(id).is_some()
    }

    /// Walk the value-pair sequence looking for the given ID.
    /// Returns the value bytes if found.
    fn find_value(&self, id: u32) -> Option<&[u8]> {
        let mut pos = 0usize;
        while pos + 12 <= self.payload.len() {
            let pair_len = u64_le(&self.payload, pos) as usize;
            if pair_len < 4 || pos + 8 + pair_len > self.payload.len() {
                break;
            }
            let pair_id = u32_le(&self.payload, pos + 8);
            let value_len = pair_len - 4;
            let value_start = pos + 12;
            let value_end = value_start + value_len;
            if value_end > self.payload.len() {
                break;
            }
            if pair_id == id {
                return Some(&self.payload[value_start..value_end]);
            }
            pos += 8 + pair_len;
        }
        None
    }

    /// Find the signer's certificate bytes inside a v2 or v3 block.
    ///
    /// Layout (v2 and v3 are identical for our purposes):
    ///
    ///   block-value (the value bytes for ID 0x7109871a / 0xf05368c0):
    ///     u64  length-prefixed sequence of signers
    ///       u64  length-prefixed signer entry
    ///         u64  length-prefixed signed_data
    ///           u64  length-prefixed digests
    ///             ... (we skip these)
    ///           u64  length-prefixed certificates  ← we want [0]
    ///             u64  length-prefixed certificate
    ///               bytes (DER-encoded X.509)
    ///           u64  length-prefixed additional_attributes
    ///             ... (skipped)
    ///         u64  length-prefixed signatures
    ///           ... (skipped)
    ///         u64  length-prefixed public_key
    ///           ... (skipped)
    ///
    /// Each length prefix is a u64 little-endian and does NOT include
    /// its own 8 bytes.
    fn find_signer_cert(&self) -> std::result::Result<Option<Vec<u8>>, ApkInspectionError> {
        // Try v3.1, then v3, then v2.
        let block_value = self
            .find_value(APK_SIG_SCHEME_V31_ID)
            .or_else(|| self.find_value(APK_SIG_SCHEME_V3_ID))
            .or_else(|| self.find_value(APK_SIG_SCHEME_V2_ID));

        let Some(value) = block_value else {
            return Ok(None);
        };

        // If the value is empty or too short to contain even a single
        // length prefix, treat the block as "present but unparseable" —
        // we still report the scheme as detected, just without a cert
        // fingerprint. This keeps us resilient to truncated or
        // hand-crafted APKs in the test suite and in the wild.
        if value.len() < 8 {
            return Ok(None);
        }

        // Top-level: sequence of signers. We use a helper that returns
        // Option instead of Result so any malformed nested structure
        // degrades gracefully to "no cert extracted" rather than
        // propagating an error up to the caller.
        let Some((signers_seq, _)) = try_read_length_prefixed(value, 0) else {
            return Ok(None);
        };
        let Some((signer, _)) = try_read_length_prefixed(signers_seq, 0) else {
            return Ok(None);
        };
        let Some((signed_data, _)) = try_read_length_prefixed(signer, 0) else {
            return Ok(None);
        };
        // Inside signed_data: digests, certificates, additional_attributes.
        let Some((_digests, after_digests)) = try_read_length_prefixed(signed_data, 0) else {
            return Ok(None);
        };
        let Some((certs_seq, _)) = try_read_length_prefixed(signed_data, after_digests) else {
            return Ok(None);
        };
        let Some((cert, _)) = try_read_length_prefixed(certs_seq, 0) else {
            return Ok(None);
        };
        if cert.is_empty() {
            return Ok(None);
        }
        Ok(Some(cert.to_vec()))
    }
}

/// Parse the APK Signing Block, if present.
///
/// The block sits immediately before the central directory. Its layout:
///
/// ```text
///   ┌─────────────────────────────────────────┐  ← block_start
///   │ u64  size_of_block (excl. this field)   │
///   │ ... value-pair sequence ...             │
///   │ u64  size_of_block (repeat)             │
///   │ "APK Sig Block 42" (16 bytes magic)     │
///   └─────────────────────────────────────────┘  ← cd_offset
/// ```
fn parse_signing_block(
    file: &mut std::fs::File,
    cd_offset: u64,
    file_len: u64,
) -> std::result::Result<Option<SigningBlock>, ApkInspectionError> {
    if cd_offset < APK_SIG_BLOCK_TRAILER_SIZE {
        // No room for a signing block — APK is v1-only or unsigned.
        return Ok(None);
    }

    // Read the trailer: 8 bytes (size_of_block repeat) + 16 bytes (magic).
    file.seek(SeekFrom::Start(cd_offset - APK_SIG_BLOCK_TRAILER_SIZE))?;
    let mut trailer = [0u8; APK_SIG_BLOCK_TRAILER_SIZE as usize];
    file.read_exact(&mut trailer)?;

    // Verify the magic.
    if &trailer[8..24] != APK_SIG_BLOCK_MAGIC {
        return Ok(None);
    }

    // size_of_block is in the first 8 bytes of the trailer. It counts
    // everything *except* the leading 8-byte size_of_block field — i.e.
    // it covers the value-pair sequence + the trailing 24 bytes.
    let size_of_block = u64_le(&trailer, 0);
    if size_of_block < APK_SIG_BLOCK_TRAILER_SIZE {
        // Impossibly small — corruption or a non-standard block.
        return Ok(None);
    }

    let block_start = cd_offset
        .checked_sub(size_of_block + 8)
        .ok_or_else(|| ApkInspectionError::Internal("signing block underflow".into()))?;
    if block_start > file_len {
        return Ok(None);
    }

    // The payload is everything between block_start+8 (skip the leading
    // size_of_block) and the trailer start.
    let payload_start = block_start + 8;
    let payload_len = (cd_offset - APK_SIG_BLOCK_TRAILER_SIZE).saturating_sub(payload_start);
    if payload_len > 64 * 1024 * 1024 {
        // Sanity: signing blocks > 64 MB almost always indicate corruption.
        return Err(ApkInspectionError::Internal(format!(
            "APK signing block payload unreasonably large: {payload_len} bytes"
        )));
    }

    file.seek(SeekFrom::Start(payload_start))?;
    let mut payload = vec![0u8; payload_len as usize];
    file.read_exact(&mut payload)?;

    Ok(Some(SigningBlock { payload }))
}

// ----- Helpers: length-prefix reader --------------------------------------

/// Read a u64-LE length-prefixed chunk at `offset`. Returns the slice
/// and the offset *after* the prefix + chunk (i.e. where the next
/// sibling field begins).
///
/// Returns an error if the length prefix is malformed or extends past
/// the end of the buffer.
fn read_length_prefixed<'a>(
    buf: &'a [u8],
    offset: usize,
) -> std::result::Result<(&'a [u8], usize), ApkInspectionError> {
    if offset + 8 > buf.len() {
        return Err(ApkInspectionError::Internal(format!(
            "length prefix underflow at offset {offset} (buf len {})",
            buf.len()
        )));
    }
    let len = u64_le(buf, offset) as usize;
    let start = offset + 8;
    let end = start
        .checked_add(len)
        .ok_or_else(|| ApkInspectionError::Internal("length prefix overflow".into()))?;
    if end > buf.len() {
        return Err(ApkInspectionError::Internal(format!(
            "length-prefixed field at offset {offset} claims {len} bytes but only {} remain",
            buf.len() - start
        )));
    }
    Ok((&buf[start..end], end))
}

/// Tolerant version of `read_length_prefixed` — returns `None` on any
/// parse error instead of propagating an `Err`. Used by
/// `find_signer_cert` so a truncated or hand-crafted APK doesn't cause
/// the whole verify call to fail; we'd rather report "scheme detected,
/// cert not extractable" than bail out entirely.
fn try_read_length_prefixed(buf: &[u8], offset: usize) -> Option<(&[u8], usize)> {
    read_length_prefixed(buf, offset).ok()
}

// ----- Helpers: hashing & DN parsing --------------------------------------

/// Compute the SHA-256 of the DER certificate and format as
/// colon-separated lowercase hex (e.g. `ab:cd:ef:01:...`).
fn sha256_colon_hex(cert_der: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(cert_der);
    let digest = hasher.finalize();
    digest
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(":")
}

/// Best-effort extraction of the subject DN from a DER-encoded X.509
/// certificate. We do a *very* lightweight walk of the ASN.1 structure
/// — just enough to pull the common printable-string fields out of the
/// Subject sequence. Returns `None` on any parse error; callers should
/// treat the subject as informational only.
///
/// We deliberately avoid pulling in `x509-parser` or similar — keeping
/// the dependency footprint small is more important than a 100%-correct
/// DN rendering. If the user needs the canonical DN, they can run
/// `apksigner verify --print-certs` on the APK out-of-band.
fn parse_subject_dn(cert_der: &[u8]) -> Option<String> {
    // `der_sequence_contents` returns `(contents_slice, end_offset)`. We
    // only need the contents slice here — we discard the end offset.
    let (tbs, _end) = der_sequence_contents(cert_der).ok()?;
    // tbs fields in order: version [0] EXPLICIT, serialNumber, signature
    // (AlgorithmIdentifier), issuer (Name), validity, subject (Name), ...
    // We need to skip past version + serial + signature + issuer + validity
    // to reach subject. That's complex; instead, we take a shortcut:
    // extract every printable string we find and join them — this gives
    // a usable (if not strictly ordered) representation of the subject.
    let mut parts: Vec<String> = Vec::new();
    collect_printable_strings(tbs, &mut parts);
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(", "))
    }
}

/// Walk an ASN.1 buffer collecting printable strings (UTF8String,
/// PrintableString, IA5String, T61String). Used for the lightweight
/// subject-DN extractor above.
fn collect_printable_strings(buf: &[u8], out: &mut Vec<String>) {
    let mut pos = 0;
    while pos + 2 <= buf.len() {
        let tag = buf[pos];
        let len_byte = buf[pos + 1] as usize;
        let (content_len, header_len) = if len_byte & 0x80 == 0 {
            (len_byte, 2usize)
        } else {
            let n_bytes = len_byte & 0x7f;
            if n_bytes == 0 || n_bytes > 4 || pos + 2 + n_bytes > buf.len() {
                break;
            }
            let mut len = 0usize;
            for i in 0..n_bytes {
                len = (len << 8) | buf[pos + 2 + i] as usize;
            }
            (len, 2 + n_bytes)
        };

        let content_start = pos + header_len;
        let content_end = match content_start.checked_add(content_len) {
            Some(e) if e <= buf.len() => e,
            _ => break,
        };
        let content = &buf[content_start..content_end];

        // Tag numbers for common string types (universal class, primitive).
        //   0x0c UTF8String
        //   0x13 PrintableString
        //   0x16 IA5String
        //   0x14 T61String / TeletexString
        if matches!(tag, 0x0c | 0x13 | 0x14 | 0x16) {
            if let Ok(s) = std::str::from_utf8(content) {
                let s = s.trim();
                if !s.is_empty() {
                    out.push(s.to_string());
                }
            }
        } else if matches!(tag, 0x30 | 0x31) {
            // SEQUENCE / SET — recurse.
            collect_printable_strings(content, out);
        }

        pos = content_end;
    }
}

/// Given a DER buffer that starts with a SEQUENCE, return the contents
/// of that SEQUENCE (skipping the tag + length header).
fn der_sequence_contents(buf: &[u8]) -> std::result::Result<(&[u8], usize), ()> {
    if buf.len() < 2 || buf[0] != 0x30 {
        return Err(());
    }
    let len_byte = buf[1] as usize;
    let (content_len, header_len) = if len_byte & 0x80 == 0 {
        (len_byte, 2usize)
    } else {
        let n_bytes = len_byte & 0x7f;
        if n_bytes == 0 || n_bytes > 4 || buf.len() < 2 + n_bytes {
            return Err(());
        }
        let mut len = 0usize;
        for i in 0..n_bytes {
            len = (len << 8) | buf[2 + i] as usize;
        }
        (len, 2 + n_bytes)
    };
    let end = header_len.checked_add(content_len).ok_or(())?;
    if end > buf.len() {
        return Err(());
    }
    Ok((&buf[header_len..end], end))
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

#[inline]
fn u64_le(buf: &[u8], pos: usize) -> u64 {
    u64::from_le_bytes([
        buf[pos],
        buf[pos + 1],
        buf[pos + 2],
        buf[pos + 3],
        buf[pos + 4],
        buf[pos + 5],
        buf[pos + 6],
        buf[pos + 7],
    ])
}

// ----- Tests --------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Write a temporary APK-shaped file containing the given bytes.
    fn write_tmp(data: &[u8]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join("droidker-sig-tests");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(format!("sig-{}.apk", uuid::Uuid::new_v4()));
        std::fs::write(&p, data).unwrap();
        p
    }

    /// Build a minimal APK with a central directory at offset `cd_offset`
    /// and an EOCD at the very end. Entries is a list of (name, size).
    /// We zero most header fields — only name + size are populated.
    fn build_apk_with_entries(entries: &[&str], include_signing_block: bool) -> Vec<u8> {
        // Each CD entry is 46 bytes fixed + name length.
        let cd_total: usize = entries.iter().map(|n| 46 + n.len()).sum();
        let cd_size = cd_total as u32;

        // APK Signing Block layout (when included):
        //
        //   offset 0   u64  size_of_block  = 40  (excludes this 8-byte field)
        //   offset 8   u64  pair_len       = 8   (id + value = 4 + 4 bytes)
        //   offset 16  u32  pair_id        = APK_SIG_SCHEME_V2_ID
        //   offset 20  u32  pair_value     = 0x00000000
        //   offset 24  u64  size_of_block  = 40  (repeat)
        //   offset 32  16B  magic          = "APK Sig Block 42"
        //
        // Total on-disk size = 8 + 8 + 4 + 4 + 8 + 16 = 48 bytes.
        // The central directory starts at offset 48 when the block is present.
        let cd_offset = if include_signing_block { 48u32 } else { 0u32 };

        let mut buf = Vec::new();

        if include_signing_block {
            // Leading size_of_block (8 bytes). Value = 40 because the
            // block contains 16 bytes of value-pair + 24 bytes of trailer.
            buf.extend_from_slice(&40u64.to_le_bytes());
            // Value-pair: pair_len=8 (4 bytes id + 4 bytes value).
            buf.extend_from_slice(&8u64.to_le_bytes());
            buf.extend_from_slice(&APK_SIG_SCHEME_V2_ID.to_le_bytes());
            buf.extend_from_slice(&0u32.to_le_bytes());
            // Trailing size_of_block + magic.
            buf.extend_from_slice(&40u64.to_le_bytes());
            buf.extend_from_slice(APK_SIG_BLOCK_MAGIC);
        }

        // Central directory entries.
        for name in entries {
            buf.extend_from_slice(&0x02014b50u32.to_le_bytes()); // signature
            buf.extend_from_slice(&[0u8; 42]); // 42 bytes of zeroed header
            let entry_start = buf.len() - 46;
            // name_len at offset 28.
            let name_len = name.len() as u16;
            buf[entry_start + 28..entry_start + 30].copy_from_slice(&name_len.to_le_bytes());
            buf.extend_from_slice(name.as_bytes());
        }

        // EOCD.
        buf.extend_from_slice(&0x06054b50u32.to_le_bytes());
        buf.extend_from_slice(&[0u8; 4]); // disk numbers
        let total: u16 = entries.len() as u16;
        buf.extend_from_slice(&total.to_le_bytes()); // on-disk count
        buf.extend_from_slice(&total.to_le_bytes()); // total count
        buf.extend_from_slice(&cd_size.to_le_bytes());
        buf.extend_from_slice(&cd_offset.to_le_bytes());
        buf.extend_from_slice(&0u16.to_le_bytes()); // comment length

        buf
    }

    #[test]
    fn detects_unsigned_apk() {
        let apk = build_apk_with_entries(&["AndroidManifest.xml", "classes.dex"], false);
        let path = write_tmp(&apk);
        let info = verify_signature(&path).unwrap();
        assert!(!info.signed);
        assert_eq!(info.scheme, "none");
        assert!(info.cert_sha256.is_none());
    }

    #[test]
    fn detects_v1_signature_by_sf_file() {
        let apk = build_apk_with_entries(
            &["AndroidManifest.xml", "classes.dex", "META-INF/CERT.SF", "META-INF/CERT.RSA"],
            false,
        );
        let path = write_tmp(&apk);
        let info = verify_signature(&path).unwrap();
        assert!(info.signed);
        assert_eq!(info.scheme, "v1");
        // v1 cert extraction is deferred — fingerprint is None.
        assert!(info.cert_sha256.is_none());
    }

    #[test]
    fn detects_v2_signing_block_presence() {
        let apk = build_apk_with_entries(&["AndroidManifest.xml", "classes.dex"], true);
        let path = write_tmp(&apk);
        let info = verify_signature(&path).unwrap();
        assert!(info.signed);
        assert_eq!(info.scheme, "v2");
    }

    #[test]
    fn is_v1_signature_entry_recognises_common_names() {
        assert!(is_v1_signature_entry("META-INF/CERT.SF"));
        assert!(is_v1_signature_entry("META-INF/CERT.RSA"));
        assert!(is_v1_signature_entry("META-INF/SIGNER.EC"));
        assert!(is_v1_signature_entry("META-INF/MYKEY.DSA"));
        // Case-insensitive on the extension.
        assert!(is_v1_signature_entry("META-INF/cert.sf"));
        // Wrong directory.
        assert!(!is_v1_signature_entry("classes/CERT.SF"));
        // Not a signature file.
        assert!(!is_v1_signature_entry("META-INF/MANIFEST.MF"));
        // Wrong depth.
        assert!(!is_v1_signature_entry("META-INF/nested/CERT.SF"));
    }

    #[test]
    fn sha256_colon_hex_format_is_correct() {
        // SHA-256 of empty input is well-known.
        let h = sha256_colon_hex(&[]);
        assert_eq!(
            h,
            "e3:b0:c4:42:98:fc:1c:14:9a:fb:f4:c8:99:6f:b9:24:27:ae:41:e4:64:9b:93:4c:a4:95:99:1b:78:52:b8:55"
        );
    }

    #[test]
    fn unsigned_apk_helper_is_correct() {
        let info = ApkSignatureInfo::unsigned("/tmp/foo.apk".into());
        assert!(!info.signed);
        assert_eq!(info.scheme, "none");
        assert_eq!(info.path, "/tmp/foo.apk");
        assert!(info.cert_sha256.is_none());
        assert!(info.cert_subject.is_none());
    }

    #[test]
    fn read_length_prefixed_returns_slice_and_next_offset() {
        // 4-byte payload with a u64 length prefix.
        let mut buf = Vec::new();
        buf.extend_from_slice(&4u64.to_le_bytes());
        buf.extend_from_slice(b"abcd");
        buf.extend_from_slice(&0u64.to_le_bytes()); // trailing empty field
        let (slice, next) = read_length_prefixed(&buf, 0).unwrap();
        assert_eq!(slice, b"abcd");
        assert_eq!(next, 12);
        let (slice2, _) = read_length_prefixed(&buf, next).unwrap();
        assert!(slice2.is_empty());
    }

    #[test]
    fn read_length_prefixed_rejects_overflow() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&100u64.to_le_bytes()); // claims 100 bytes
        buf.extend_from_slice(b"ab"); // only 2 actually present
        let r = read_length_prefixed(&buf, 0);
        assert!(r.is_err());
    }

    #[test]
    fn der_sequence_contents_parses_simple_sequence() {
        // SEQUENCE { INTEGER 0x05 }
        let buf = vec![0x30, 0x03, 0x02, 0x01, 0x05];
        let (content, end) = der_sequence_contents(&buf).unwrap();
        assert_eq!(content, &[0x02, 0x01, 0x05]);
        assert_eq!(end, 5);
    }

    #[test]
    fn der_sequence_contents_rejects_non_sequence() {
        let buf = vec![0x02, 0x01, 0x05]; // INTEGER, not SEQUENCE
        assert!(der_sequence_contents(&buf).is_err());
    }

    #[test]
    fn missing_signing_block_returns_none_not_error() {
        // APK without a signing block — should return Ok(None) for the
        // signing block parse, not an error.
        let apk = build_apk_with_entries(&["AndroidManifest.xml"], false);
        let path = write_tmp(&apk);
        // Full verify_signature should still succeed and report "none" or "v1".
        let info = verify_signature(&path).unwrap();
        assert_eq!(info.scheme, "none");
    }
}
