// src/container/mod.rs
//
// Container lifecycle management. This module is the heart of DroidKer — it
// owns the in-memory container registry, persists state to disk, and
// delegates the actual sandbox creation to `isolation`, `cgroups`,
// `network`, and `rootfs`.

pub mod cgroups;
pub mod isolation;
pub mod manager;
pub mod network;
pub mod ports;
pub mod rootfs;
pub mod runtime;
pub mod translation;

pub use manager::ContainerManager;
