// src/main.rs
//
// `droidker` CLI — talks to the running `droidkerd` daemon over HTTP.
//
// Design goals:
//   - Familiar Docker-like UX: `droidker ps`, `droidker run app.apk`, ...
//   - Zero config: defaults to http://127.0.0.1:8080, override with
//     --host or DROIDKER_HOST env var.
//   - Tiny binary (no async TLS bloat beyond rustls) so it can be scp'd to a
//     low-resource VPS without second thought.

mod client;
mod commands;
mod fmt;

use clap::{Parser, Subcommand};
use std::path::PathBuf;

/// DroidKer CLI — manage Android micro-containers from the command line.
#[derive(Parser, Debug)]
#[command(name = "droidker", version, about, long_about = None)]
struct Cli {
    /// URL of the running droidkerd daemon.
    #[arg(long, env = "DROIDKER_HOST", default_value = "http://127.0.0.1:8080")]
    host: String,

    /// Output format: pretty (default) or json.
    #[arg(long, env = "DROIDKER_OUTPUT", default_value = "pretty")]
    output: String,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Show daemon version + connectivity status.
    Info,

    /// List running + stopped containers.
    #[command(alias = "ls")]
    Ps,

    /// Upload an APK to the daemon's APK store.
    Upload {
        /// Path to the .apk file.
        path: PathBuf,
    },

    /// Create + start a container from an uploaded APK in one step.
    /// Equivalent to `create` + `start`.
    Run {
        /// Path to the .apk file (will be uploaded if not already known).
        apk: PathBuf,

        /// Friendly name for the container.
        #[arg(short, long)]
        name: Option<String>,

        /// Memory limit in MB.
        #[arg(short = 'm', long)]
        memory: Option<u32>,

        /// CPU quota (% of one core, 1-100).
        #[arg(short = 'c', long)]
        cpu: Option<u32>,

        /// Free-form notes.
        #[arg(long)]
        notes: Option<String>,

        /// Publish a host:container TCP port pair. Can be repeated.
        /// Example: `-p 8080:80 -p 8443:443`.
        #[arg(short = 'p', long = "port", value_name = "HOST:CONTAINER")]
        ports: Vec<String>,
    },

    /// Create a container without starting it.
    Create {
        apk: String,
        #[arg(short, long)]
        name: Option<String>,
        #[arg(short = 'm', long)]
        memory: Option<u32>,
        #[arg(short = 'c', long)]
        cpu: Option<u32>,
        #[arg(long)]
        notes: Option<String>,
        #[arg(short = 'p', long = "port", value_name = "HOST:CONTAINER")]
        ports: Vec<String>,
    },

    /// Start a stopped container.
    Start { id_or_name: String },

    /// Stop a running container.
    Stop { id_or_name: String },

    /// Restart a container.
    Restart { id_or_name: String },

    /// Remove a stopped container.
    #[command(alias = "del")]
    Rm { id_or_name: String },

    /// Show detailed info about one container.
    Inspect { id_or_name: String },

    /// Show resource usage stats (memory, CPU, PIDs, top processes).
    Stats { id_or_name: String },

    /// Fetch the container's log file.
    Logs {
        id_or_name: String,
        /// Which log to fetch: `init`, `runtime` (default), or `logcat`.
        #[arg(short, long, default_value = "runtime")]
        kind: String,
    },

    /// Save one frame of the container's screen to a PNG file.
    /// Useful for CI screenshots and offline debugging.
    Screenshot {
        id_or_name: String,
        /// Output file path. Defaults to <id>.jpg.
        #[arg(short = 'o', long)]
        out: Option<PathBuf>,
    },

    /// Inject a tap (down+up) at the given screen coordinate.
    Tap {
        id_or_name: String,
        /// X coordinate in container-screen pixels.
        x: i32,
        /// Y coordinate in container-screen pixels.
        y: i32,
    },

    /// Inject a swipe gesture from (x1,y1) to (x2,y2) over `duration_ms`.
    Swipe {
        id_or_name: String,
        x1: i32,
        y1: i32,
        x2: i32,
        y2: i32,
        /// Swipe duration in milliseconds.
        #[arg(short = 'd', long, default_value = "300")]
        duration_ms: u32,
    },

    /// Inject a Home / Back / Recent key tap.
    Key {
        id_or_name: String,
        /// Which key: home | back | recent.
        key: String,
    },

    /// Humanized tap (M5): Bezier-jittered down+up with Gaussian pressure.
    /// Use this instead of `tap` when you want the gesture to look like a
    /// real finger — bot detectors flag instant down+up pairs.
    Htap {
        id_or_name: String,
        x: i32,
        y: i32,
    },

    /// Humanized swipe (M5): curved Bezier path with Gaussian-jittered
    /// inter-sample delays. Use this instead of `swipe` for automation
    /// that needs to evade bot detection.
    Hswipe {
        id_or_name: String,
        x1: i32,
        y1: i32,
        x2: i32,
        y2: i32,
    },

    /// Humanized long-press (M5): holds with small position drift so
    /// Android's InputReader fires the long-press callback.
    Hlongpress {
        id_or_name: String,
        x: i32,
        y: i32,
        /// Hold duration in milliseconds.
        hold_ms: u32,
    },

    /// Run a command inside a running container.
    Exec {
        id_or_name: String,
        /// Command to run (argv array). Use `--` to separate from droidker's
        /// own flags, e.g. `droidker exec my-app -- /system/bin/ls /data`.
        #[arg(required = true, last = true, num_args = 1..)]
        cmd: Vec<String>,
        /// Working directory inside the container.
        #[arg(short = 'C', long)]
        cwd: Option<String>,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "warn".into()),
        )
        .with_target(false)
        .init();

    let cli = Cli::parse();
    let client = client::DroidkerClient::new(&cli.host)?;
    let output_json = cli.output == "json";

    match cli.cmd {
        Cmd::Info => commands::info(&client, output_json).await,
        Cmd::Ps => commands::ps(&client, output_json).await,
        Cmd::Upload { path } => commands::upload(&client, &path, output_json).await,
        Cmd::Run {
            apk,
            name,
            memory,
            cpu,
            notes,
            ports,
        } => commands::run(&client, &apk, name, memory, cpu, notes, &ports, output_json).await,
        Cmd::Create {
            apk,
            name,
            memory,
            cpu,
            notes,
            ports,
        } => commands::create(&client, &apk, name, memory, cpu, notes, &ports, output_json).await,
        Cmd::Start { id_or_name } => commands::start(&client, &id_or_name, output_json).await,
        Cmd::Stop { id_or_name } => commands::stop(&client, &id_or_name, output_json).await,
        Cmd::Restart { id_or_name } => commands::restart(&client, &id_or_name, output_json).await,
        Cmd::Rm { id_or_name } => commands::rm(&client, &id_or_name, output_json).await,
        Cmd::Inspect { id_or_name } => commands::inspect(&client, &id_or_name, output_json).await,
        Cmd::Stats { id_or_name } => commands::stats(&client, &id_or_name, output_json).await,
        Cmd::Logs { id_or_name, kind } => commands::logs(&client, &id_or_name, &kind).await,
        Cmd::Screenshot { id_or_name, out } => {
            commands::screenshot(&client, &id_or_name, out.as_deref()).await
        }
        Cmd::Tap { id_or_name, x, y } => commands::tap(&client, &id_or_name, x, y).await,
        Cmd::Swipe {
            id_or_name,
            x1,
            y1,
            x2,
            y2,
            duration_ms,
        } => commands::swipe(&client, &id_or_name, x1, y1, x2, y2, duration_ms).await,
        Cmd::Key { id_or_name, key } => commands::key(&client, &id_or_name, &key).await,
        Cmd::Htap { id_or_name, x, y } => commands::htap(&client, &id_or_name, x, y).await,
        Cmd::Hswipe {
            id_or_name,
            x1,
            y1,
            x2,
            y2,
        } => commands::hswipe(&client, &id_or_name, x1, y1, x2, y2).await,
        Cmd::Hlongpress {
            id_or_name,
            x,
            y,
            hold_ms,
        } => commands::hlongpress(&client, &id_or_name, x, y, hold_ms).await,
        Cmd::Exec { id_or_name, cmd, cwd } => {
            commands::exec(&client, &id_or_name, &cmd, cwd.as_deref(), output_json).await
        }
    }
}
