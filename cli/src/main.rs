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

    /// Inspect an uploaded APK and list its native ABIs (M7).
    ///
    /// Shows which `lib/<abi>/*.so` directories the APK ships, the
    /// number of `.so` files per ABI, the total uncompressed bytes,
    /// and the recommended target arch for `droidker run --arch`.
    InspectApk {
        /// Filename of an already-uploaded APK (as returned by `upload`).
        apk: String,
    },

    /// Verify an APK's signature (M8.1).
    ///
    /// Detects which signature scheme (v1 / v2 / v3 / v3.1) the APK
    /// uses and extracts the signer certificate SHA-256 fingerprint
    /// (for v2/v3). Does NOT perform full cryptographic validation —
    /// only checks that the APK is signed at all and reports the
    /// signer's cert fingerprint so you can cross-check it against
    /// an out-of-band source of truth.
    VerifyApk {
        /// Filename of an already-uploaded APK (as returned by `upload`).
        apk: String,
    },

    /// Inspect a split-APK bundle (.xapk / .apks) (M8.2).
    ///
    /// Lists the inner APKs (base + ABI / locale / density splits),
    /// shows which ABIs the bundle ships splits for, and recommends
    /// which APKs to install for a given target arch.
    InspectBundle {
        /// Filename of an already-uploaded bundle (as returned by `upload`).
        bundle: String,
        /// Optional target arch (`arm`, `arm64`, `x86`, `x86_64`).
        /// When supplied, the response includes a `recommended_install`
        /// field listing which inner APKs to install.
        #[arg(long, value_name = "ARCH")]
        arch: Option<String>,
    },

    /// Extract + run a split-APK bundle in one shot (M9.1).
    ///
    /// Uploads the `.xapk` / `.apks` bundle, inspects its structure to
    /// find the base APK + ABI splits, extracts the recommended install
    /// set to `<data_dir>/apks/<bundle_sha>/`, creates a container with
    /// the base APK as the primary and the splits as `extra_apks`, then
    /// starts it. Equivalent to running `upload` + `inspect-bundle` +
    /// `extract` + `create` + `start` manually.
    RunBundle {
        /// Path to the `.xapk` or `.apks` bundle file.
        bundle: PathBuf,

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

        /// Target CPU architecture. Accepted values: `arm`, `arm64`,
        /// `x86`, `x86_64`, or `auto`. `auto` (the default) picks the
        /// first available ABI split from the bundle.
        #[arg(long, value_name = "ARCH")]
        arch: Option<String>,

        /// Translation strategy override (M7.2). See `droidker run
        /// --translation-strategy`.
        #[arg(long, value_name = "STRATEGY")]
        translation_strategy: Option<String>,

        /// Extra split to install alongside the base + ABI split. Pass
        /// the ZIP path (as reported by `inspect-bundle`), e.g.
        /// `config.en.apk` for the English locale split. Can be repeated.
        #[arg(long = "split", value_name = "ZIP_PATH")]
        extra_splits: Vec<String>,
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

        /// Target CPU architecture (M6). Accepted values: `arm`, `arm64`,
        /// `x86`, `x86_64`, or `auto` (M7). When omitted, the container
        /// runs on the host's native arch (no translation). On an x86_64
        /// host with libhoudini or libndk_translation installed,
        /// `--arch arm64` lets ARM-only APKs run via transparent binary
        /// translation. `--arch auto` (M7) uploads the APK, inspects its
        /// `lib/<abi>/*.so` entries, and picks the best target arch
        /// automatically.
        #[arg(long, value_name = "ARCH")]
        arch: Option<String>,

        /// Translation strategy override (M7.2). Accepted values:
        /// `houdini`, `ndk_translation`, `qemu-user`, `native`. When
        /// omitted, the manager auto-resolves based on the host and the
        /// requested `--arch`. Useful for apps that crash under
        /// libhoudini but work under qemu-user.
        #[arg(long, value_name = "STRATEGY")]
        translation_strategy: Option<String>,
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
        /// Target CPU architecture (M6). See `droidker run --arch`.
        /// `auto` is not supported on `create` (the APK must already
        /// have been uploaded); use `droidker run --arch auto` instead.
        #[arg(long, value_name = "ARCH")]
        arch: Option<String>,
        /// Translation strategy override (M7.2). See `droidker run --translation-strategy`.
        #[arg(long, value_name = "STRATEGY")]
        translation_strategy: Option<String>,
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

    /// Humanized two-finger pinch-zoom gesture (M8.4).
    ///
    /// Sends two fingers down at `start_distance` apart and moves them
    /// to `end_distance` apart. When `end_distance > start_distance`,
    /// it's a zoom-in; otherwise zoom-out.
    Hpinch {
        id_or_name: String,
        /// X coordinate of the pinch center.
        center_x: i32,
        /// Y coordinate of the pinch center.
        center_y: i32,
        /// Initial distance between the two fingers, in pixels.
        #[arg(long, default_value = "30")]
        start_distance: f64,
        /// Final distance between the two fingers, in pixels.
        #[arg(long, default_value = "200")]
        end_distance: f64,
        /// Orientation of the pinch line in degrees (0 = horizontal,
        /// 90 = vertical, 45 = diagonal — the human default).
        #[arg(long, default_value = "45")]
        angle_deg: f64,
    },

    /// Record a container's screen stream to an MJPEG file (M5).
    /// Useful for CI artifacts — drop a recording into your test report
    /// so reviewers can see exactly what the test saw.
    Record {
        id_or_name: String,
        /// Output file path. Default: <id>-record.mjpeg
        #[arg(short = 'o', long)]
        out: Option<PathBuf>,
        /// Recording duration in seconds. Default: 10.
        #[arg(short = 'd', long, default_value = "10")]
        duration: u64,
        /// Frames per second to capture (1..=30). Default: 5 — low enough
        /// for CI but high enough to catch UI flicker.
        #[arg(short = 'f', long, default_value = "5")]
        fps: u32,
        /// JPEG quality (10..=95). Default: 70.
        #[arg(short = 'q', long, default_value = "70")]
        quality: u8,
    },

    /// Capture an MP4 video of the container's screen via Android's
    /// `screenrecord` binary (M9.2).
    ///
    /// Unlike `record` (M5.4) which produces an MJPEG via the WebSocket
    /// screen stream, this subcommand invokes the real `screenrecord`
    /// binary inside the container's namespaces — so you get a proper
    /// H.264 MP4 file with audio + smaller size + better quality.
    /// Useful for product demos, app store trailers, and CI artifacts
    /// that need to play in standard video players.
    Mp4 {
        id_or_name: String,
        /// Output file path. Default: <id>-<timestamp>.mp4
        #[arg(short = 'o', long)]
        out: Option<PathBuf>,
        /// Recording duration in seconds. Capped at 180 (the
        /// `screenrecord` per-file hard limit). Default: 10.
        #[arg(short = 'd', long, default_value = "10")]
        duration: u32,
        /// Video bit rate in bits per second. Default: 4 Mbps.
        /// Bump to 8 Mbps for game captures with rapid motion.
        #[arg(short = 'b', long, default_value = "4000000")]
        bit_rate: u32,
        /// Capture width in pixels. Default: 540 (qHD).
        #[arg(long, default_value = "540")]
        width: u32,
        /// Capture height in pixels. Default: 960.
        #[arg(long, default_value = "960")]
        height: u32,
        /// Rotate the recording 90 degrees. Useful for portrait apps
        /// being recorded in landscape orientation.
        #[arg(long)]
        rotate: bool,
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
        Cmd::InspectApk { apk } => commands::inspect_apk(&client, &apk, output_json).await,
        Cmd::VerifyApk { apk } => commands::verify_apk(&client, &apk, output_json).await,
        Cmd::InspectBundle { bundle, arch } => {
            commands::inspect_bundle(&client, &bundle, arch.as_deref(), output_json).await
        }
        Cmd::RunBundle {
            bundle,
            name,
            memory,
            cpu,
            notes,
            ports,
            arch,
            translation_strategy,
            extra_splits,
        } => commands::run_bundle(
            &client,
            &bundle,
            name,
            memory,
            cpu,
            notes,
            &ports,
            arch,
            translation_strategy,
            &extra_splits,
            output_json,
        )
        .await,
        Cmd::Run {
            apk,
            name,
            memory,
            cpu,
            notes,
            ports,
            arch,
            translation_strategy,
        } => commands::run(
            &client,
            &apk,
            name,
            memory,
            cpu,
            notes,
            &ports,
            arch,
            translation_strategy,
            output_json,
        )
        .await,
        Cmd::Create {
            apk,
            name,
            memory,
            cpu,
            notes,
            ports,
            arch,
            translation_strategy,
        } => commands::create(
            &client,
            &apk,
            name,
            memory,
            cpu,
            notes,
            &ports,
            arch,
            translation_strategy,
            output_json,
        )
        .await,
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
        Cmd::Hpinch {
            id_or_name,
            center_x,
            center_y,
            start_distance,
            end_distance,
            angle_deg,
        } => {
            commands::hpinch(
                &client,
                &id_or_name,
                center_x,
                center_y,
                start_distance,
                end_distance,
                angle_deg,
            )
            .await
        }
        Cmd::Record {
            id_or_name,
            out,
            duration,
            fps,
            quality,
        } => {
            commands::record(
                &client,
                &id_or_name,
                out.as_deref(),
                duration,
                fps,
                quality,
            )
            .await
        }
        Cmd::Mp4 {
            id_or_name,
            out,
            duration,
            bit_rate,
            width,
            height,
            rotate,
        } => {
            commands::mp4(
                &client,
                &id_or_name,
                out.as_deref(),
                duration,
                bit_rate,
                width,
                height,
                rotate,
            )
            .await
        }
        Cmd::Exec { id_or_name, cmd, cwd } => {
            commands::exec(&client, &id_or_name, &cmd, cwd.as_deref(), output_json).await
        }
    }
}
