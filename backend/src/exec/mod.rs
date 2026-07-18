// src/exec/mod.rs
//
// `droidker exec` implementation: spawn a process inside a running
// container's namespaces (pid, mount, net, uts, ipc, user) using nsenter.
//
// Why nsenter instead of a custom fork+setns dance?
//   - nsenter is already installed (util-linux, ships with every distro).
//   - It handles the SETNS dance correctly: nsopen + setns for each
//     namespace, then fork+exec.
//   - It works whether the daemon was the parent of the container's PID 1
//     or not (which is our case — the child is reparented to init).
//
// Two execution modes:
//   - **Detached** (default for one-shot CLI runs): the exec'd process runs
//     as a sibling of the daemon; its stdout/stderr are streamed back to
//     the caller via a WebSocket.
//   - **Attached** (used for interactive shells): the WebSocket is hooked
//     up directly to the child's stdin/stdout/stderr, so the user can type
//     commands and see output live.

pub mod session;

pub use session::{ExecRequest, ExecSession};
