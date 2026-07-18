// src/logs/mod.rs
//
// Log streaming for DroidKer containers.
//
// Each container's droidker-init writes a few well-known log files into
// the overlay upperdir:
//
//   <overlay>/<container-id>/upper/droidker.init.log   — init-phase logs
//   <overlay>/<container-id>/upper/droidker.runtime.log — ART/app_process output
//   <overlay>/<container-id>/upper/droidker.logcat.log  — captured logcat
//
// The `LogStreamer` opens the requested file and tails it forever (similar
// to `tail -f`), pushing new bytes through a tokio channel. The WebSocket
// actor relays those bytes to the browser.

pub mod streamer;

pub use streamer::{LogKind, LogStreamer, LogTailRequest};
