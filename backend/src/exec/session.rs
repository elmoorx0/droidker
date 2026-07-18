// src/exec/session.rs
//
// An `ExecSession` wraps a single nsenter child process and exposes its
// stdin/stdout/stderr as byte streams. The WebSocket handler in `ws/exec.rs`
// bridges these streams to the browser.
//
// Two modes:
//   - **Pipe mode** (`tty=false`, default): three independent pipes for
//     stdin/stdout/stderr. Used by `droidker exec --json` and one-shot
//     command runners.
//   - **PTY mode** (`tty=true`): a single pseudo-terminal pair. The child
//     gets the slave end as stdin/stdout/stderr; we hold the master end as
//     a `tokio::fs::File`. Used by interactive shells (`droidker exec -it
//     /system/bin/sh`) and by the dashboard's terminal widget. PTY mode
//     enables line editing, terminal resizing (TIOCSWINSZ), signal
//     delivery (Ctrl-C, Ctrl-Z), and ANSI color.

use crate::error::{DroidkerError, Result};
use serde::{Deserialize, Serialize};
use std::ffi::CString;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout};
use tokio::sync::Mutex;
use uuid::Uuid;

#[derive(Debug, Clone, Deserialize)]
pub struct ExecRequest {
    /// Command to run, e.g. ["/system/bin/sh", "-c", "ls /data"].
    pub cmd: Vec<String>,
    /// Working directory inside the container.
    pub cwd: Option<String>,
    /// Extra environment variables to set.
    pub env: Option<Vec<(String, String)>>,
    /// If true, allocate a PTY (interactive shell). Otherwise pipe stdin/stdout.
    pub tty: bool,
    /// User to run as (inside the container's user namespace). Defaults to
    /// root (uid 0) — the mapped root inside the namespace.
    #[serde(default)]
    pub user: Option<String>,
    /// Initial TTY rows (only used when `tty=true`). Default 24.
    #[serde(default = "default_rows")]
    pub rows: u16,
    /// Initial TTY cols (only used when `tty=true`). Default 80.
    #[serde(default = "default_cols")]
    pub cols: u16,
}

fn default_rows() -> u16 { 24 }
fn default_cols() -> u16 { 80 }

#[derive(Debug, Clone, Serialize)]
pub struct ExecStarted {
    pub session_id: Uuid,
    pub pid: u32,
}

/// One live exec session. The WebSocket actor holds an `Arc<Mutex<ExecSession>>`
/// and routes bytes between the socket and the child.
///
/// In pipe mode (`tty=false`):
///   - `stdin` is the child's stdin pipe (writes go to it).
///   - `stdout` and `stderr` are the child's output pipes (reads come from them).
///   - `pty_master` is None.
///
/// In PTY mode (`tty=true`):
///   - `stdin` is None (writes go through `pty_master`).
///   - `stdout` is None (reads come from `pty_master`).
///   - `stderr` is None.
///   - `pty_master` is the master end of the pty pair.
pub struct ExecSession {
    pub id: Uuid,
    pub child: Child,
    pub stdin: Option<ChildStdin>,
    pub stdout: Option<ChildStdout>,
    pub stderr: Option<ChildStderr>,
    /// PTY master in tty mode; None in pipe mode.
    pub pty_master: Option<tokio::fs::File>,
    pub tty: bool,
}

impl ExecSession {
    /// Spawn an exec session inside the container whose PID-1 is `target_pid`.
    pub async fn spawn(target_pid: u32, req: ExecRequest) -> Result<Self> {
        if req.cmd.is_empty() {
            return Err(DroidkerError::BadRequest("cmd must not be empty".into()));
        }

        if req.tty {
            Self::spawn_pty(target_pid, req).await
        } else {
            Self::spawn_pipe(target_pid, req).await
        }
    }

    /// Pipe-mode spawn (the original M2 path).
    async fn spawn_pipe(target_pid: u32, req: ExecRequest) -> Result<Self> {
        let mut cmd = Self::base_nsenter_cmd(target_pid, &req);

        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = cmd.spawn().map_err(|e| {
            DroidkerError::Syscall(format!("spawn nsenter: {e}"))
        })?;

        let stdin = child.stdin.take();
        let stdout = child.stdout.take().ok_or_else(|| {
            DroidkerError::Syscall("nsenter stdout pipe missing".into())
        })?;
        let stderr = child.stderr.take().ok_or_else(|| {
            DroidkerError::Syscall("nsenter stderr pipe missing".into())
        })?;

        let id = Uuid::new_v4();
        tracing::info!(
            session_id = %id,
            target_pid,
            cmd = ?req.cmd,
            mode = "pipe",
            "Exec session spawned"
        );

        Ok(Self {
            id,
            child,
            stdin,
            stdout: Some(stdout),
            stderr: Some(stderr),
            pty_master: None,
            tty: false,
        })
    }

    /// PTY-mode spawn.
    ///
    /// We open `/dev/ptmx` to get a master fd, unlock the corresponding slave
    /// via `grantpt`/`unlockpt`, then pass the slave path to `nsenter` by
    /// spawning the child with stdin/stdout/stderr all set to the slave fd.
    /// The master fd stays with us for read/write.
    async fn spawn_pty(target_pid: u32, req: ExecRequest) -> Result<Self> {
        // 1. Open /dev/ptmx (the multiplexer). This auto-allocates a new
        //    pty pair and returns the master fd.
        let master_fd = unsafe { libc::open(b"/dev/ptmx\0".as_ptr() as *const _, libc::O_RDWR | libc::O_NOCTTY) };
        if master_fd < 0 {
            return Err(DroidkerError::Syscall(format!(
                "open /dev/ptmx: {}",
                std::io::Error::last_os_error()
            )));
        }
        let master: OwnedFd = unsafe { OwnedFd::from_raw_fd(master_fd) };

        // 2. grantpt + unlockpt (no-ops on Linux but required by POSIX).
        unsafe {
            if libc::grantpt(master.as_raw_fd()) < 0 {
                return Err(DroidkerError::Syscall(format!("grantpt: {}", std::io::Error::last_os_error())));
            }
            if libc::unlockpt(master.as_raw_fd()) < 0 {
                return Err(DroidkerError::Syscall(format!("unlockpt: {}", std::io::Error::last_os_error())));
            }
        }

        // 3. ptsname_r — get the slave device path.
        let slave_path = {
            let mut buf = [0u8; 256];
            let rc = unsafe { libc::ptsname_r(master.as_raw_fd(), buf.as_mut_ptr() as *mut _, buf.len()) };
            if rc != 0 {
                return Err(DroidkerError::Syscall(format!("ptsname_r: {}", std::io::Error::from_raw_os_error(rc))));
            }
            let cstr = unsafe { std::ffi::CStr::from_ptr(buf.as_ptr() as *const _) };
            cstr.to_string_lossy().into_owned()
        };

        // 4. Set the initial window size via TIOCSWINSZ.
        Self::set_winsize(master.as_raw_fd(), req.rows, req.cols)?;

        // 5. Open the slave end. We'll dup it onto stdin/stdout/stderr of
        //    the child below.
        let slave_c = CString::new(slave_path.as_str()).map_err(|e| {
            DroidkerError::Internal(format!("CString: {e}"))
        })?;
        let slave_fd = unsafe { libc::open(slave_c.as_ptr(), libc::O_RDWR | libc::O_NOCTTY) };
        if slave_fd < 0 {
            return Err(DroidkerError::Syscall(format!(
                "open pty slave {}: {}",
                slave_path,
                std::io::Error::last_os_error()
            )));
        }

        // 6. Build the nsenter command. The child gets the slave fd as
        //    stdin/stdout/stderr. We use `pre_exec` to setsid + TIOCSCTTY
        //    so the slave becomes the controlling tty.
        let mut cmd = Self::base_nsenter_cmd(target_pid, &req);

        // dup() the slave twice to get two extra copies for stdout/stderr.
        let slave_fd_2 = unsafe { libc::dup(slave_fd) };
        let slave_fd_3 = unsafe { libc::dup(slave_fd) };
        if slave_fd_2 < 0 || slave_fd_3 < 0 {
            unsafe { libc::close(slave_fd); }
            if slave_fd_2 >= 0 { unsafe { libc::close(slave_fd_2); } }
            return Err(DroidkerError::Syscall(format!("dup slave: {}", std::io::Error::last_os_error())));
        }

        // Make the slave the controlling tty in the child (via pre_exec).
        unsafe {
            cmd.pre_exec(move || {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                // TIOCSCTTY makes the slave our controlling terminal.
                // The exact constant differs per arch but libc exposes it.
                let rc = libc::ioctl(slave_fd, libc::TIOCSCTTY, 0);
                if rc < 0 {
                    // Non-fatal: shell still works without controlling tty,
                    // just no job-control signals.
                }
                Ok(())
            });
        }

        // Take ownership of the slave fds via Stdio::from.
        let stdin_stdio = unsafe { Stdio::from_raw_fd(slave_fd) };
        let stdout_stdio = unsafe { Stdio::from_raw_fd(slave_fd_2) };
        let stderr_stdio = unsafe { Stdio::from_raw_fd(slave_fd_3) };

        cmd.stdin(stdin_stdio)
            .stdout(stdout_stdio)
            .stderr(stderr_stdio)
            .kill_on_drop(true);

        let mut child = cmd.spawn().map_err(|e| {
            DroidkerError::Syscall(format!("spawn nsenter (pty): {e}"))
        })?;

        // We don't care about child.stdout/stderr in PTY mode — they point
        // at the slave, which is the same fd as our master's pair. Drop them.
        child.stdout.take();
        child.stderr.take();
        child.stdin.take();

        // Wrap the master fd in a tokio::fs::File so we can use AsyncReadExt
        // / AsyncWriteExt on it. tokio::fs::File::from_raw_fd takes ownership.
        let master_dup = unsafe { libc::dup(master.as_raw_fd()) };
        if master_dup < 0 {
            return Err(DroidkerError::Syscall(format!("dup master: {}", std::io::Error::last_os_error())));
        }
        let master_file = unsafe { tokio::fs::File::from_raw_fd(master_dup) };

        let id = Uuid::new_v4();
        tracing::info!(
            session_id = %id,
            target_pid,
            cmd = ?req.cmd,
            mode = "pty",
            rows = req.rows,
            cols = req.cols,
            slave = %slave_path,
            "Exec session spawned (PTY)"
        );

        // We keep the OwnedFd alive too (it owns the original master fd);
        // master_file is a dup of it. To avoid confusion, we transfer
        // ownership of the OwnedFd to /dev/null by... actually, just leak it
        // for the lifetime of the session. The kernel cleans up on child exit.
        std::mem::forget(master);

        Ok(Self {
            id,
            child,
            stdin: None,
            stdout: None,
            stderr: None,
            pty_master: Some(master_file),
            tty: true,
        })
    }

    /// Build the base `nsenter` Command with all the env vars set.
    /// Both pipe and pty paths share this.
    fn base_nsenter_cmd(target_pid: u32, req: &ExecRequest) -> tokio::process::Command {
        let mut cmd = tokio::process::Command::new("nsenter");
        cmd.arg(format!("--target={}", target_pid));
        cmd.args([
            "--pid",
            "--mount",
            "--net",
            "--uts",
            "--ipc",
            "--user",
            "--",
        ]);
        for arg in &req.cmd {
            cmd.arg(arg);
        }

        cmd.env_clear();
        cmd.env("PATH", "/system/bin:/system/xbin:/vendor/bin:/sbin:/usr/sbin:/usr/bin");
        cmd.env("TERM", if req.tty { "xterm-256color" } else { "dumb" });
        cmd.env("HOME", "/data");
        cmd.env("ANDROID_DATA", "/data");
        cmd.env("ANDROID_ROOT", "/system");
        cmd.env("BOOTCLASSPATH", "/system/framework/core-libart.jar:/system/framework/conscrypt.jar:/system/framework/okhttp.jar:/system/framework/core-junit.jar:/system/framework/bouncycastle.jar");
        cmd.env("LD_LIBRARY_PATH", "/system/lib64:/system/lib:/vendor/lib64:/vendor/lib");
        if let Some(cwd) = &req.cwd {
            cmd.current_dir(cwd);
        }
        if let Some(envs) = &req.env {
            for (k, v) in envs {
                cmd.env(k, v);
            }
        }
        cmd
    }

    /// Write bytes to the child's stdin (pipe mode) or to the PTY master
    /// (tty mode — the slave end receives them as if typed on a terminal).
    pub async fn write_stdin(&mut self, bytes: &[u8]) -> Result<()> {
        if self.tty {
            if let Some(master) = self.pty_master.as_mut() {
                master.write_all(bytes).await?;
                master.flush().await?;
                Ok(())
            } else {
                Err(DroidkerError::InvalidState("pty_master missing in tty mode".into()))
            }
        } else if let Some(stdin) = self.stdin.as_mut() {
            stdin.write_all(bytes).await?;
            stdin.flush().await?;
            Ok(())
        } else {
            Err(DroidkerError::InvalidState(
                "stdin is not connected (child has already exited?)".into(),
            ))
        }
    }

    /// Read up to 4KB from the child's stdout (pipe mode) or from the PTY
    /// master (tty mode). Returns the bytes read; empty vec means EOF.
    pub async fn read_stdout_chunk(&mut self) -> Result<Vec<u8>> {
        let mut tmp = vec![0u8; 4096];
        if self.tty {
            if let Some(master) = self.pty_master.as_mut() {
                let n = master.read(&mut tmp).await?;
                tmp.truncate(n);
                Ok(tmp)
            } else {
                Err(DroidkerError::InvalidState("pty_master missing in tty mode".into()))
            }
        } else if let Some(stdout) = self.stdout.as_mut() {
            let n = stdout.read(&mut tmp).await?;
            tmp.truncate(n);
            Ok(tmp)
        } else {
            Err(DroidkerError::InvalidState("stdout missing in pipe mode".into()))
        }
    }

    /// Read up to 4KB from the child's stderr. Returns None in PTY mode
    /// (stderr is merged into stdout via the pty).
    pub async fn read_stderr_chunk(&mut self) -> Result<Option<Vec<u8>>> {
        if self.tty {
            return Ok(None);
        }
        if let Some(stderr) = self.stderr.as_mut() {
            let mut tmp = vec![0u8; 4096];
            let n = stderr.read(&mut tmp).await?;
            tmp.truncate(n);
            Ok(Some(tmp))
        } else {
            Ok(None)
        }
    }

    /// Resize the TTY. No-op in pipe mode.
    pub async fn resize_tty(&mut self, rows: u16, cols: u16) -> Result<()> {
        if !self.tty {
            return Ok(());
        }
        if let Some(master) = self.pty_master.as_ref() {
            Self::set_winsize(master.as_raw_fd(), rows, cols)?;
            tracing::debug!(rows, cols, "pty resized");
        }
        Ok(())
    }

    /// Wait for the child to exit and return its exit code.
    pub async fn wait(mut self) -> Result<i32> {
        let status = self.child.wait().await?;
        Ok(status.code().unwrap_or(-1))
    }

    fn set_winsize(fd: RawFd, rows: u16, cols: u16) -> Result<()> {
        #[repr(C)]
        struct WinSize {
            ws_row: u16,
            ws_col: u16,
            ws_xpixel: u16,
            ws_ypixel: u16,
        }
        let ws = WinSize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let rc = unsafe { libc::ioctl(fd, libc::TIOCSWINSZ, &ws) };
        if rc < 0 {
            return Err(DroidkerError::Syscall(format!(
                "TIOCSWINSZ: {}",
                std::io::Error::last_os_error()
            )));
        }
        Ok(())
    }
}

/// Build a JSON message suitable for sending over the exec WebSocket.
/// Format: { "type": "stdout"|"stderr"|"exit", "data": "..."|"<code>" }
pub fn encode_stream_message(kind: &str, data: &[u8]) -> String {
    let b64 = base64_encode(data);
    serde_json::json!({
        "type": kind,
        "data": b64,
    })
    .to_string()
}

pub fn encode_exit_message(code: i32) -> String {
    serde_json::json!({
        "type": "exit",
        "code": code,
    })
    .to_string()
}

/// Cheap base64 encoder (no external dep).
fn base64_encode(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((input.len() + 2) / 3 * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[((n >> 18) & 0x3F) as usize] as char);
        out.push(TABLE[((n >> 12) & 0x3F) as usize] as char);
        if chunk.len() > 1 {
            out.push(TABLE[((n >> 6) & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(TABLE[(n & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

/// Container-scoped Arc<Mutex<...>> alias used by the WS actor.
pub type SharedExecSession = Arc<Mutex<ExecSession>>;

#[allow(dead_code)]
pub fn dummy_path() -> PathBuf {
    PathBuf::from(".")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_roundtrips_short_strings() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn encode_stream_message_is_valid_json() {
        let s = encode_stream_message("stdout", b"hello\n");
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["type"], "stdout");
        assert_eq!(v["data"], "aGVsbG8K");
    }

    #[test]
    fn encode_exit_message_is_valid_json() {
        let s = encode_exit_message(0);
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["type"], "exit");
        assert_eq!(v["code"], 0);
    }
}
