// src/config/mod.rs
//
// Configuration loader for DroidKer. Settings can come from (in priority order):
//   1. Environment variables (DROIDKER_HOST, DROIDKER_PORT, ...)
//   2. A TOML/YAML file at /etc/droidker/config.toml (optional)
//   3. Built-in defaults tuned for a 1 GB / 1 vCPU VPS.

pub mod settings;

pub use settings::Settings;
