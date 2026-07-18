// src/apk/mod.rs
//
// APK inspection utilities (M7).
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

pub mod inspect;

pub use inspect::{inspect_apk, ApkAbiInfo, ApkInspectionError, InspectResult};
