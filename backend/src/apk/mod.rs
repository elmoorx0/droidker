// src/apk/mod.rs
//
// APK inspection + signature verification + bundle support
// (M7 + M8.1 + M8.2).
//
// An APK is just a ZIP file. To pick the right translation strategy we
// need to know which native ABIs the APK ships — that information lives
// in the names of the `lib/<abi>/*.so` entries inside the ZIP.
//
// We *don't* want to pull in the `zip` crate (or `flate2`) just for this:
// on a 1-GB VPS every shared lib counts, and we only need to enumerate
// file names, not decompress contents. So `inspect.rs` ships a tiny
// hand-rolled central-directory walker (~150 LOC) that reads only the
// EOCD record at the end of the file + the central directory entries.
//
// `verify.rs` (added in M8.1) layers signature detection on top of the
// same ZIP walker. It detects v1 (JAR) signatures via `META-INF/*.SF`
// entries, and v2/v3 signatures via the APK Signing Block that sits
// immediately before the central directory. It extracts the signer
// certificate's SHA-256 fingerprint so users can cross-check against an
// out-of-band source of truth (e.g. `apksigner verify --print-certs`).
//
// `bundle.rs` (added in M8.2) handles split-APK bundles in `.xapk` and
// `.apks` format. These are ZIP-of-APKs archives used to ship one base
// APK plus ABI/locale/density splits. The inspector enumerates the
// inner APKs and recommends which splits to install for a given arch.
//
// M9.1 adds `extract_bundle` to the same module — it actually pulls the
// inner APKs out of the bundle ZIP (handling both STORED and DEFLATED
// entries) and writes them to disk under `<data_dir>/apks/<bundle_sha>/`
// so the container init script can `pm install-multiple` them.

pub mod bundle;
pub mod inspect;
pub mod verify;

pub use bundle::{
    extract_bundle, inspect_bundle, BundleEntry, BundleExtractResult, BundleFormat,
    BundleInspectResult, ExtractedApk, ExtractSpec, SplitKind,
};
pub use inspect::{inspect_apk, ApkAbiInfo, ApkInspectionError, InspectResult};
pub use verify::{verify_signature, ApkSignatureInfo};
