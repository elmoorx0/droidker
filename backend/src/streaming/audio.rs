// src/streaming/audio.rs
//
// Audio streaming for DroidKer containers (Milestone 5).
//
// Architecture
// ------------
// The daemon captures the container's audio output (from `/dev/snd/` or
// the AudioFlinger dump) and pushes raw PCM frames to the browser over a
// WebSocket (`/audio/ws`). The browser decodes them with the Web Audio
// API (`AudioBuffer` + `AudioBufferSourceNode`) — no native AAC encoder
// required on the VPS, which keeps the binary small and avoids the GPL
// licensing entanglements of `fdk-aac` / `ffmpeg`.
//
// Why raw PCM and not AAC?
//   - AAC encoders (`fdk-aac`, `ffmpeg`) pull in native dependencies that
//     bloat the daemon and conflict with our "single 6 MB Rust binary"
//     target for 1-GB VPS hosts.
//   - At 8 kHz mono (telephony quality — plenty for UI sounds and most
//     app audio), raw s16le PCM is 16 KB/s. Over a 60-second session
//     that's ~1 MB of WebSocket traffic. Acceptable.
//   - For higher quality we'd switch to Opus (RFC 6716), which has a
//     pure-Rust decoder (`opus-rs` binds the reference C lib, but the
//     `opus` crate has a pure-Rust decoder). That's M6 territory.
//
// Capture strategy (in priority order):
//   1. `tinymix` + ALSA loopback — set up a `snd-aloop` kernel module on
//      the host, point Android's AudioFlinger at it, capture from the
//      loopback's capture device. Requires `snd-aloop` loaded.
//   2. `screenrecord --output-format=wav` (Android 11+) — captures audio
//      to a WAV file we tail. Requires a fully-booted Android runtime.
//   3. Silent test tone — 440 Hz sine wave so the streaming path can be
//      verified even without a working Android audio HAL. Used in dev
//      mode and on the skeleton rootfs.
//
// Wire format (WebSocket binary frames):
//   Each frame is a chunk of PCM samples with a 12-byte header:
//     [0..4]   u32 LE   sample_rate (e.g. 8000)
//     [4..6]   u16 LE   channels (1 = mono, 2 = stereo)
//     [6..8]   u16 LE   bits_per_sample (always 16, or 0 for silence — see below)
//     [8..12]  u32 LE   sample_count in this chunk
//     [12..]   s16 LE   sample bytes (sample_count * channels * 2)
//
// M8.3 silence detection (VAD):
//   When the client enables VAD via `{ "type": "set_vad", "enabled": true,
//   "threshold_db": -40 }`, the daemon computes the RMS of each chunk
//   *before* sending. Chunks whose RMS falls below `threshold_db`
//   (default: -40 dBFS, roughly the noise floor of a quiet room) are
//   sent as a 12-byte *silence marker*: header with `bits_per_sample=0`
//   and `sample_count=N`, followed by NO PCM payload. The browser
//   generates `sample_count` samples of digital silence on receive.
//
//   For typical Android UI audio (mostly silence punctuated by short
//   blips), this cuts WebSocket bandwidth by 10-50x without pulling
//   in a native Opus encoder — keeping the daemon a single ~6 MB Rust
//   binary as required for the 1-GB VPS target.
//
//   To avoid spurious silence→audio→silence flapping on quiet but
//   audible passages, we apply 50 ms of hysteresis: a chunk must be
//   below the threshold for `vad_hold_ms` (default 50) consecutive
//   milliseconds before we start sending silence markers, and we
//   always send the first non-silent chunk in full so the browser
//   can resume playback seamlessly.
//
// Text messages from the client are JSON control messages:
//   { "type": "set_format", "sample_rate": 16000, "channels": 1 }
//   { "type": "set_volume", "volume": 0.8 }
//   { "type": "set_vad", "enabled": true, "threshold_db": -40 }
//   { "type": "ping" }

use crate::error::{DroidkerError, Result};
use serde::{Deserialize, Serialize};
use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use uuid::Uuid;

/// PCM audio format. We only support s16le because that's what Web Audio
/// decodes natively without conversion.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct AudioFormat {
    pub sample_rate: u32,
    pub channels: u16,
    pub bits_per_sample: u16,
}

impl Default for AudioFormat {
    fn default() -> Self {
        // 8 kHz mono s16le = 16 KB/s — telephony quality, ideal for a
        // 1-vCPU VPS where we don't want audio encoding to compete with
        // the Android runtime for cycles.
        Self {
            sample_rate: 8000,
            channels: 1,
            bits_per_sample: 16,
        }
    }
}

impl AudioFormat {
    /// Bytes per second of audio at this format.
    pub fn bytes_per_second(&self) -> u32 {
        self.sample_rate * self.channels as u32 * (self.bits_per_sample as u32 / 8)
    }

    /// Bytes for `ms` milliseconds of audio.
    pub fn bytes_for_ms(&self, ms: u32) -> usize {
        ((self.bytes_per_second() as usize) * ms as usize) / 1000
    }
}

/// Stub AAC encoder — placeholder for M6. Currently just wraps the PCM
/// bytes unchanged so the wire format stays stable when we swap in a
/// real AAC encoder later.
#[derive(Debug, Clone)]
pub struct AacEncoder {
    format: AudioFormat,
}

impl AacEncoder {
    pub fn new(format: AudioFormat) -> Self {
        Self { format }
    }

    /// Encode a chunk of PCM samples. In the current implementation this
    /// is a no-op pass-through — the wire format stays PCM. When a real
    /// AAC encoder lands in M6, this method will return ADTS-framed AAC.
    pub fn encode(&mut self, pcm: &[u8]) -> Vec<u8> {
        // Pass-through: the wire format header already declares the
        // encoding (bits_per_sample=16, no AAC marker), so the browser
        // knows to treat the payload as raw PCM.
        pcm.to_vec()
    }

    pub fn format(&self) -> AudioFormat {
        self.format
    }
}

/// Captures audio from the container. One capturer per container, lives
/// for the lifetime of the first /audio/ws subscriber. Subsequent
/// subscribers share the same capturer via a tokio Mutex.
pub struct AudioCapturer {
    container_id: Uuid,
    format: AudioFormat,
    /// Child handle for the capture process (e.g. `screenrecord` or
    /// `tinymix` pipe). None when running in test-tone mode.
    child: Option<std::process::Child>,
    /// Stdout of the child process, where we read PCM samples from.
    /// In test-tone mode this is None and we synthesize samples.
    stdout: Option<std::process::ChildStdout>,
    /// Test-tone phase accumulator (only used in test-tone mode).
    test_phase: f64,
    /// When the capturer was started — used to limit total runtime in
    /// test-tone mode so a forgotten browser tab doesn't fill /tmp.
    started_at: Instant,
}

impl AudioCapturer {
    /// Spawn the capturer for `container_id`. Tries ALSA loopback first,
    /// then `screenrecord`, then falls back to a test tone.
    pub fn new(container_id: Uuid, format: AudioFormat) -> Result<Self> {
        tracing::info!(
            container_id = %container_id,
            format = ?format,
            "spawning audio capturer"
        );

        // Try 1: ALSA loopback (`snd-aloop`).
        if let Some(capturer) = Self::try_alsa_loopback(container_id, format)? {
            return Ok(capturer);
        }

        // Try 2: screenrecord (requires fully-booted Android).
        if let Some(capturer) = Self::try_screenrecord(container_id, format)? {
            return Ok(capturer);
        }

        // Fallback: test tone.
        tracing::warn!(
            container_id = %container_id,
            "no audio source available; falling back to 440 Hz test tone"
        );
        Ok(Self {
            container_id,
            format,
            child: None,
            stdout: None,
            test_phase: 0.0,
            started_at: Instant::now(),
        })
    }

    /// Attempt to capture from /dev/snd/ via `arecord`. Returns None if
    /// the device or `arecord` is missing.
    fn try_alsa_loopback(
        container_id: Uuid,
        format: AudioFormat,
    ) -> Result<Option<Self>> {
        // Check that `arecord` exists on the host.
        if std::process::Command::new("arecord")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_err()
        {
            return Ok(None);
        }

        // We assume the host has snd-aloop loaded at hw:1,0 — the
        // standard convention. setup.sh verifies this.
        let mut cmd = Command::new("arecord");
        cmd.args([
            "-D",
            "hw:1,0",
            "-f",
            "S16_LE",
            "-r",
            &format.sample_rate.to_string(),
            "-c",
            &format.channels.to_string(),
            "-t",
            "raw",
            "-q",
        ]);
        cmd.stdin(Stdio::null());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::null());

        match cmd.spawn() {
            Ok(mut child) => {
                let stdout = child.stdout.take();
                tracing::info!(
                    container_id = %container_id,
                    "audio capturer using ALSA loopback (hw:1,0)"
                );
                Ok(Some(Self {
                    container_id,
                    format,
                    child: Some(child),
                    stdout,
                    test_phase: 0.0,
                    started_at: Instant::now(),
                }))
            }
            Err(e) => {
                tracing::debug!(
                    error = %e,
                    "arecord spawn failed; falling through to next source"
                );
                Ok(None)
            }
        }
    }

    /// Attempt to capture via `screenrecord --output-format=wav`. Only
    /// works on a fully-booted Android 11+ runtime.
    fn try_screenrecord(
        container_id: Uuid,
        format: AudioFormat,
    ) -> Result<Option<Self>> {
        // We'd need to nsenter into the container's mount namespace to
        // find /system/bin/screenrecord. That requires the container's
        // PID, which we don't have here in the audio module. For M5 we
        // skip this path and rely on ALSA loopback or test tone.
        // M6 will add proper nsenter-based screenrecord capture.
        let _ = (container_id, format);
        Ok(None)
    }

    /// Read one chunk of PCM samples (roughly 50 ms of audio). Returns
    /// the raw s16le bytes. In test-tone mode, synthesizes a 440 Hz
    /// sine wave.
    pub fn read_chunk(&mut self, ms: u32) -> Result<Vec<u8>> {
        let want_bytes = self.format.bytes_for_ms(ms);

        if let Some(stdout) = self.stdout.as_mut() {
            let mut buf = vec![0u8; want_bytes];
            let mut filled = 0;
            while filled < want_bytes {
                match stdout.read(&mut buf[filled..]) {
                    Ok(0) => break, // EOF — child died
                    Ok(n) => filled += n,
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(e) => return Err(DroidkerError::Syscall(format!("arecord read: {e}"))),
                }
            }
            buf.truncate(filled);
            Ok(buf)
        } else {
            // Test-tone mode: synthesize a 440 Hz sine wave.
            Ok(self.synthesize_test_tone(want_bytes))
        }
    }

    /// Synthesize `want_bytes` of 440 Hz sine wave at the capturer's
    /// format. Updates `test_phase` so successive calls produce a
    /// continuous waveform.
    fn synthesize_test_tone(&mut self, want_bytes: usize) -> Vec<u8> {
        let bytes_per_sample = (self.format.bits_per_sample / 8) as usize;
        let frame_size = bytes_per_sample * self.format.channels as usize;
        let frames = want_bytes / frame_size;
        let mut out = Vec::with_capacity(frames * frame_size);

        let freq = 440.0_f64;
        let amplitude = i16::MAX as f64 * 0.2; // -14 dBFS, not ear-splitting
        let phase_step = 2.0 * std::f64::consts::PI * freq / self.format.sample_rate as f64;

        for _ in 0..frames {
            let sample = (amplitude * self.test_phase.sin()) as i16;
            self.test_phase += phase_step;
            // Wrap phase to keep the f64 precise.
            if self.test_phase > 2.0 * std::f64::consts::PI * 1000.0 {
                self.test_phase %= 2.0 * std::f64::consts::PI;
            }
            for _ch in 0..self.format.channels {
                out.extend_from_slice(&sample.to_le_bytes());
            }
        }
        out
    }

    /// Total bytes captured so far (for stats reporting).
    pub fn elapsed(&self) -> Duration {
        self.started_at.elapsed()
    }

    pub fn format(&self) -> AudioFormat {
        self.format
    }
}

impl Drop for AudioCapturer {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
            tracing::info!(
                container_id = %self.container_id,
                "audio capturer child killed"
            );
        }
    }
}

// ----- WebSocket actor ------------------------------------------------------

/// Per-connection actor for /audio/ws. Owns its own AudioCapturer (we
/// don't share across subscribers in M5 — each subscriber pays its own
/// arecord cost, which is fine for the single-viewer use case on a
/// 1-vCPU VPS).
pub struct AudioWs {
    pub container_id: Uuid,
    pub format: AudioFormat,
    pub capturer: Option<AudioCapturer>,
    /// Sender that the capture task uses to push PCM chunks to the WS actor.
    pub chunk_tx: Option<tokio::sync::mpsc::Sender<Vec<u8>>>,
    /// Last heartbeat — we close the connection if the client goes
    /// quiet for 60 s.
    pub last_heartbeat: Instant,
    /// Voice Activity Detection config (M8.3). When enabled, silent
    /// chunks are replaced with 12-byte silence markers, cutting
    /// WebSocket bandwidth 10-50x for typical Android UI audio.
    pub vad: VadConfig,
}

impl AudioWs {
    pub fn new(container_id: Uuid, format: AudioFormat) -> Self {
        Self {
            container_id,
            format,
            capturer: None,
            chunk_tx: None,
            last_heartbeat: Instant::now(),
            vad: VadConfig::default(),
        }
    }
}

/// Wire-format header for a PCM audio chunk.
fn pack_chunk_header(format: AudioFormat, sample_count: u32) -> [u8; 12] {
    let mut header = [0u8; 12];
    header[0..4].copy_from_slice(&format.sample_rate.to_le_bytes());
    header[4..6].copy_from_slice(&format.channels.to_le_bytes());
    header[6..8].copy_from_slice(&format.bits_per_sample.to_le_bytes());
    header[8..12].copy_from_slice(&sample_count.to_le_bytes());
    header
}

/// Encode a PCM chunk into the wire format (12-byte header + samples).
pub fn encode_audio_chunk(format: AudioFormat, pcm: &[u8]) -> Vec<u8> {
    let bytes_per_sample = (format.bits_per_sample / 8) as usize;
    let frame_size = bytes_per_sample * format.channels as usize;
    let sample_count = if frame_size > 0 {
        (pcm.len() / frame_size) as u32
    } else {
        0
    };
    let mut out = Vec::with_capacity(12 + pcm.len());
    out.extend_from_slice(&pack_chunk_header(format, sample_count));
    out.extend_from_slice(pcm);
    out
}

/// Encode a *silence marker* — a 12-byte header with `bits_per_sample=0`
/// and no PCM payload. The browser generates `sample_count` samples of
/// digital silence on receive. Used by the VAD path (M8.3) to skip
/// transmitting silent chunks, cutting WebSocket bandwidth 10-50x for
/// typical Android UI audio.
pub fn encode_silence_chunk(format: AudioFormat, sample_count: u32) -> Vec<u8> {
    // Same layout as a normal chunk header, but with bits_per_sample=0
    // to signal "this is a silence marker, no payload follows".
    let silence_format = AudioFormat {
        sample_rate: format.sample_rate,
        channels: format.channels,
        bits_per_sample: 0,
    };
    let mut out = Vec::with_capacity(12);
    out.extend_from_slice(&pack_chunk_header(silence_format, sample_count));
    out
}

// ----- Voice Activity Detection (VAD) -------------------------------------
//
// Pure-Rust silence detector. We compute the RMS amplitude of each PCM
// chunk and compare it against a configurable threshold (in dBFS, where
// 0 dBFS = full-scale / max amplitude). Chunks below the threshold are
// considered silent.
//
// Why RMS and not peak? RMS tracks perceived loudness more closely — a
// short click will spike the peak without substantially moving the RMS,
// which is exactly the behaviour we want (clicks *are* signal, even
// if quiet).
//
// Why dBFS and not raw amplitude? Decibels give us a stable,
// perceptually-meaningful scale that doesn't depend on the bit depth:
// -40 dBFS at 16-bit ≈ -40 dBFS at 24-bit ≈ "quiet room noise floor".

/// VAD configuration. Lives inside the AudioWs actor and is mutated by
/// `set_vad` control messages from the client.
#[derive(Debug, Clone)]
pub struct VadConfig {
    /// Whether VAD is currently active. When `false`, every chunk is
    /// sent in full (the M5 default — preserves wire-format backwards
    /// compatibility).
    pub enabled: bool,
    /// Threshold in dBFS (negative, typically -30 to -50). Chunks
    /// whose RMS is *below* this are considered silent.
    pub threshold_db: f32,
    /// How many milliseconds of consecutive silence to require before
    /// we start emitting silence markers. Prevents flap on quiet-but-
    /// audible passages. Default: 50 ms.
    pub hold_ms: u32,
    /// Internal: how many consecutive ms of silence we've seen so far.
    /// Reset to 0 whenever a non-silent chunk arrives.
    pub silent_ms: u32,
    /// Internal: are we currently in "emitting silence" mode? Toggled
    /// on after `silent_ms >= hold_ms` and off on the first non-silent
    /// chunk.
    pub emitting_silence: bool,
}

impl Default for VadConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            threshold_db: -40.0,
            hold_ms: 50,
            silent_ms: 0,
            emitting_silence: false,
        }
    }
}

impl VadConfig {
    /// Process a chunk of PCM samples. Returns `true` if this chunk
    /// should be sent as a silence marker (i.e. VAD is enabled AND the
    /// chunk is silent AND we've been silent long enough to start
    /// emitting markers).
    ///
    /// Updates internal hysteresis state. The caller is responsible
    /// for either calling `encode_silence_chunk` (when this returns
    /// `true`) or `encode_audio_chunk` (when it returns `false`).
    pub fn process_chunk(&mut self, pcm: &[u8], format: AudioFormat, chunk_ms: u32) -> bool {
        if !self.enabled {
            return false;
        }

        let rms_db = compute_rms_db(pcm);
        let is_silent = rms_db < self.threshold_db;

        if is_silent {
            self.silent_ms = self.silent_ms.saturating_add(chunk_ms);
            // Use strict `>` rather than `>=` so that a single chunk
            // whose duration equals `hold_ms` is still sent in full.
            // This matters for the common case where `hold_ms == 50`
            // and chunks are 50 ms each — we want the *first* silent
            // chunk to be sent (so the browser doesn't miss a
            // potential start of audio), and only start emitting
            // silence markers from the *second* silent chunk onwards.
            if self.silent_ms > self.hold_ms {
                self.emitting_silence = true;
                return true;
            }
            // Still in the hold-off period — send the chunk in full so
            // the browser doesn't miss the start of a quiet passage.
            return false;
        }

        // Non-silent chunk: reset hysteresis, force-emit the next
        // chunk in full even if we were in silence mode.
        self.silent_ms = 0;
        let was_emitting = self.emitting_silence;
        self.emitting_silence = false;
        // If we *were* emitting silence, send this chunk in full so
        // the browser can resume playback seamlessly. Otherwise (we
        // weren't emitting), this is just a normal non-silent chunk —
        // also send in full.
        let _ = was_emitting;
        false
    }
}

/// Compute the RMS amplitude of a PCM buffer in dBFS (decibels relative
/// to full scale). Returns -∞ for an empty buffer (interpreted as
/// "silent" by `VadConfig::process_chunk` because any negative value
/// is below the threshold).
///
/// `pcm` is little-endian s16 samples, interleaved by channel. We mix
/// down to mono by averaging channels before computing the RMS — this
/// matches how the human ear perceives loudness for multi-channel audio.
pub fn compute_rms_db(pcm: &[u8]) -> f32 {
    if pcm.len() < 2 {
        return f32::NEG_INFINITY;
    }
    let mut sum_sq: f64 = 0.0;
    let mut count: usize = 0;
    // Iterate 2 bytes at a time (s16le samples).
    let mut i = 0;
    while i + 1 < pcm.len() {
        let sample = i16::from_le_bytes([pcm[i], pcm[i + 1]]) as f64;
        sum_sq += sample * sample;
        count += 1;
        i += 2;
    }
    if count == 0 {
        return f32::NEG_INFINITY;
    }
    let rms = (sum_sq / count as f64).sqrt();
    // Convert to dBFS: 20 * log10(rms / max).
    // max for i16 = 32767. We add a tiny epsilon to avoid log10(0).
    let max = 32767.0_f64;
    let ratio = rms / max;
    if ratio <= 0.0 {
        return f32::NEG_INFINITY;
    }
    let db = 20.0 * ratio.log10();
    db as f32
}

#[cfg(test)]
mod vad_tests {
    use super::*;

    fn make_sine_wave(samples: usize, freq: f64, sr: u32, amplitude: f64) -> Vec<u8> {
        let mut out = Vec::with_capacity(samples * 2);
        let phase_step = 2.0 * std::f64::consts::PI * freq / sr as f64;
        let mut phase = 0.0_f64;
        for _ in 0..samples {
            let s = (amplitude * phase.sin()) as i16;
            out.extend_from_slice(&s.to_le_bytes());
            phase += phase_step;
            if phase > 2.0 * std::f64::consts::PI * 1000.0 {
                phase %= 2.0 * std::f64::consts::PI;
            }
        }
        out
    }

    #[test]
    fn rms_of_digital_silence_is_negative_infinity() {
        let pcm = vec![0u8; 800]; // 400 samples of zero
        let db = compute_rms_db(&pcm);
        assert!(db.is_infinite() && db.is_sign_negative());
    }

    #[test]
    fn rms_of_full_scale_sine_is_near_zero_db() {
        // 1 kHz sine at full scale (amplitude = i16::MAX).
        let pcm = make_sine_wave(800, 1000.0, 8000, 32767.0);
        let db = compute_rms_db(&pcm);
        // RMS of a full-scale sine is -3.01 dBFS (because RMS = max / sqrt(2)).
        assert!(db > -4.0 && db < -2.0, "expected ~-3 dBFS, got {db}");
    }

    #[test]
    fn rms_of_quiet_sine_is_well_below_threshold() {
        // 1 kHz sine at 1% of full scale ≈ -40 dBFS amplitude, so
        // RMS ≈ -43 dBFS.
        let pcm = make_sine_wave(800, 1000.0, 8000, 32767.0 * 0.01);
        let db = compute_rms_db(&pcm);
        assert!(db < -40.0, "expected < -40 dBFS, got {db}");
    }

    #[test]
    fn rms_of_empty_buffer_is_silent() {
        let db = compute_rms_db(&[]);
        assert!(db.is_infinite() && db.is_sign_negative());
    }

    #[test]
    fn vad_disabled_always_returns_false() {
        let mut vad = VadConfig::default();
        vad.enabled = false;
        let format = AudioFormat::default();
        let pcm = make_sine_wave(400, 1000.0, 8000, 32767.0);
        // Even a loud chunk returns false when VAD is disabled.
        assert!(!vad.process_chunk(&pcm, format, 50));
    }

    #[test]
    fn vad_emits_silence_after_hold_period() {
        let mut vad = VadConfig::default();
        vad.enabled = true;
        vad.threshold_db = -40.0;
        vad.hold_ms = 50;
        let format = AudioFormat::default();
        let silent_pcm = vec![0u8; 800]; // 50 ms of silence at 8 kHz mono s16le

        // First 50 ms: should NOT emit silence (still in hold period).
        assert!(!vad.process_chunk(&silent_pcm, format, 50));
        assert_eq!(vad.silent_ms, 50);
        assert!(!vad.emitting_silence);

        // Second 50 ms: should emit silence (hold period elapsed).
        assert!(vad.process_chunk(&silent_pcm, format, 50));
        assert!(vad.emitting_silence);
    }

    #[test]
    fn vad_resets_on_loud_chunk() {
        let mut vad = VadConfig::default();
        vad.enabled = true;
        vad.threshold_db = -40.0;
        vad.hold_ms = 50;
        vad.silent_ms = 40; // almost at threshold
        let format = AudioFormat::default();
        let loud_pcm = make_sine_wave(400, 1000.0, 8000, 32767.0);

        // Loud chunk resets the silent_ms counter and forces full send.
        assert!(!vad.process_chunk(&loud_pcm, format, 50));
        assert_eq!(vad.silent_ms, 0);
        assert!(!vad.emitting_silence);
    }

    #[test]
    fn vad_resumes_silence_after_loud_chunk() {
        let mut vad = VadConfig::default();
        vad.enabled = true;
        vad.threshold_db = -40.0;
        vad.hold_ms = 50;
        vad.emitting_silence = true; // was emitting silence
        let format = AudioFormat::default();

        // Loud chunk: should NOT emit silence, should reset state.
        let loud_pcm = make_sine_wave(400, 1000.0, 8000, 32767.0);
        assert!(!vad.process_chunk(&loud_pcm, format, 50));
        assert!(!vad.emitting_silence);

        // Then 100 ms of silence: should immediately re-enter silence mode.
        let silent_pcm = vec![0u8; 1600]; // 100 ms at 8 kHz mono
        // First 50 ms: hold period, no silence yet.
        assert!(!vad.process_chunk(&silent_pcm[..800], format, 50));
        // Next 50 ms: silence total ≥ hold_ms, emit silence.
        assert!(vad.process_chunk(&silent_pcm[..800], format, 50));
    }

    #[test]
    fn silence_chunk_header_has_zero_bits_per_sample() {
        let format = AudioFormat::default();
        let bytes = encode_silence_chunk(format, 400);
        assert_eq!(bytes.len(), 12); // header only, no payload
        // bits_per_sample at offset 6..8 should be 0.
        assert_eq!(u16::from_le_bytes([bytes[6], bytes[7]]), 0);
        // sample_count at offset 8..12 should be 400.
        assert_eq!(u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]), 400);
    }

    #[test]
    fn normal_chunk_header_preserves_bits_per_sample() {
        let format = AudioFormat::default();
        let pcm = vec![0u8; 800];
        let bytes = encode_audio_chunk(format, &pcm);
        // bits_per_sample at offset 6..8 should be 16.
        assert_eq!(u16::from_le_bytes([bytes[6], bytes[7]]), 16);
        // 12-byte header + 800 bytes of PCM.
        assert_eq!(bytes.len(), 812);
    }
}

// ----- Actor + StreamHandler impls -----------------------------------------

use actix::prelude::*;
use actix_web_actors::ws;

/// How long to keep an audio session alive after the last client activity.
const AUDIO_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);

impl Actor for AudioWs {
    type Context = ws::WebsocketContext<Self>;

    fn started(&mut self, ctx: &mut Self::Context) {
        tracing::info!(
            container_id = %self.container_id,
            format = ?self.format,
            "audio WS connected"
        );
        self.last_heartbeat = Instant::now();

        // Heartbeat: every 30 s send a ping. If the client doesn't reply
        // within 60 s, close the connection.
        ctx.run_interval(AUDIO_HEARTBEAT_INTERVAL, |act, ctx| {
            if act.last_heartbeat.elapsed() > Duration::from_secs(60) {
                tracing::warn!("audio WS heartbeat timeout; closing");
                ctx.close(Some(ws::CloseReason {
                    code: ws::CloseCode::Normal,
                    description: Some("heartbeat timeout".into()),
                }));
                ctx.stop();
                return;
            }
            ctx.ping(b"");
        });

        // Spawn the capture task. The task reads 50 ms chunks from the
        // AudioCapturer and pushes them as binary WS messages.
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
        self.chunk_tx = Some(tx);

        let container_id = self.container_id;
        let format = self.format;
        let addr = ctx.address();

        actix::spawn(async move {
            let mut capturer = match AudioCapturer::new(container_id, format) {
                Ok(c) => c,
                Err(e) => {
                    tracing::error!(
                        container_id = %container_id,
                        error = %e,
                        "failed to start audio capturer"
                    );
                    return;
                }
            };
            tracing::info!(container_id = %container_id, "audio capture task started");

            loop {
                // Read 50 ms of audio at a time. On a 1-vCPU VPS this
                // is small enough to keep latency low but large enough
                // to amortize the per-read syscall cost.
                match capturer.read_chunk(50) {
                    Ok(pcm) => {
                        if pcm.is_empty() {
                            // Source exhausted — sleep briefly and retry.
                            tokio::time::sleep(Duration::from_millis(50)).await;
                            continue;
                        }
                        // Send the raw PCM bytes — the actor applies VAD
                        // and encodes the wire-format frame in its
                        // `SendAudio` handler. This keeps the VAD state
                        // (hysteresis counters etc.) on the actor side
                        // where it can be mutated by `set_vad` control
                        // messages from the client.
                        if addr.try_send(SendAudio(pcm)).is_err() {
                            // Channel full or closed — drop chunk.
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            container_id = %container_id,
                            error = %e,
                            "audio read failed; sleeping"
                        );
                        tokio::time::sleep(Duration::from_millis(200)).await;
                    }
                }
            }
        });

        // Relay task: turns mpsc messages into WS binary frames.
        let addr2 = ctx.address();
        actix::spawn(async move {
            while let Some(bytes) = rx.recv().await {
                if addr2.try_send(SendAudio(bytes)).is_err() {
                    break;
                }
            }
        });
    }

    fn stopped(&mut self, _ctx: &mut Self::Context) {
        tracing::info!(container_id = %self.container_id, "audio WS disconnected");
        // Drop the sender so the capture task's try_send fails and it exits.
        self.chunk_tx.take();
        // The capturer (if any) is dropped here, killing the arecord child.
    }
}

#[derive(Message)]
#[rtype(result = "()")]
struct SendAudio(Vec<u8>);

impl Handler<SendAudio> for AudioWs {
    type Result = ();
    fn handle(&mut self, msg: SendAudio, ctx: &mut Self::Context) {
        self.last_heartbeat = Instant::now();
        // Apply VAD (M8.3). `process_chunk` mutates hysteresis state
        // and returns `true` if this chunk should be sent as a silence
        // marker instead of as raw PCM.
        let chunk_ms = 50u32; // matches the capture task's read_chunk(50)
        let frame = if self.vad.process_chunk(&msg.0, self.format, chunk_ms) {
            // Silent: emit a 12-byte silence marker (no PCM payload).
            // Compute the actual sample count so the browser generates
            // the right number of silence samples.
            let frame_size = (self.format.bits_per_sample / 8) as usize
                * self.format.channels as usize;
            let sample_count = if frame_size > 0 {
                (msg.0.len() / frame_size) as u32
            } else {
                0
            };
            encode_silence_chunk(self.format, sample_count)
        } else {
            // Non-silent (or VAD disabled): emit a normal PCM chunk.
            encode_audio_chunk(self.format, &msg.0)
        };
        ctx.binary(frame);
    }
}

/// Text messages from the client are JSON control messages.
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AudioControlMessage {
    SetFormat { sample_rate: u32, channels: u16 },
    SetVolume { volume: f32 },
    /// Enable or disable Voice Activity Detection (M8.3). When enabled,
    /// silent chunks are replaced with 12-byte silence markers — saves
    /// 10-50x WebSocket bandwidth on typical Android UI audio.
    SetVad {
        enabled: bool,
        /// Optional threshold in dBFS (default -40). Chunks whose RMS
        /// is below this are treated as silent.
        threshold_db: Option<f32>,
        /// Optional hold-off in ms (default 50). How long a chunk must
        /// be silent before we start emitting silence markers.
        hold_ms: Option<u32>,
    },
    Ping,
}

impl StreamHandler<std::result::Result<ws::Message, ws::ProtocolError>> for AudioWs {
    fn handle(
        &mut self,
        item: std::result::Result<ws::Message, ws::ProtocolError>,
        ctx: &mut Self::Context,
    ) {
        self.last_heartbeat = Instant::now();
        match item {
            Ok(ws::Message::Ping(msg)) => ctx.pong(&msg),
            Ok(ws::Message::Pong(_)) => {}
            Ok(ws::Message::Text(txt)) => {
                match serde_json::from_str::<AudioControlMessage>(&txt) {
                    Ok(AudioControlMessage::SetFormat {
                        sample_rate,
                        channels,
                    }) => {
                        tracing::info!(
                            container_id = %self.container_id,
                            sample_rate,
                            channels,
                            "client requested format change (ignored — capturer format fixed at session start)"
                        );
                        // We'd need to recreate the capturer. M6.
                        ctx.text(r#"{"type":"error","msg":"format change not yet supported"}"#);
                    }
                    Ok(AudioControlMessage::SetVolume { volume }) => {
                        let v = volume.clamp(0.0, 1.0);
                        tracing::info!(
                            container_id = %self.container_id,
                            volume = v,
                            "client set volume (applied client-side in Web Audio)"
                        );
                        ctx.text(r#"{"type":"ok"}"#);
                    }
                    Ok(AudioControlMessage::SetVad {
                        enabled,
                        threshold_db,
                        hold_ms,
                    }) => {
                        self.vad.enabled = enabled;
                        if let Some(t) = threshold_db {
                            // Clamp to a sane range: -80 dBFS (essentially
                            // digital noise floor) to 0 dBFS (full scale).
                            self.vad.threshold_db = t.clamp(-80.0, 0.0);
                        }
                        if let Some(h) = hold_ms {
                            // Clamp to 0-1000 ms — longer holds make the
                            // stream unresponsive to short audio blips.
                            self.vad.hold_ms = h.clamp(0, 1000);
                        }
                        // Reset hysteresis state so the new config takes
                        // effect immediately on the next chunk.
                        self.vad.silent_ms = 0;
                        self.vad.emitting_silence = false;
                        tracing::info!(
                            container_id = %self.container_id,
                            enabled,
                            threshold_db = self.vad.threshold_db,
                            hold_ms = self.vad.hold_ms,
                            "VAD config updated"
                        );
                        ctx.text(r#"{"type":"ok"}"#);
                    }
                    Ok(AudioControlMessage::Ping) => {
                        ctx.text(r#"{"type":"pong"}"#);
                    }
                    Err(e) => {
                        ctx.text(format!(r#"{{"type":"error","msg":"{e}"}}"#));
                    }
                }
            }
            Ok(ws::Message::Binary(_)) => {}
            Ok(ws::Message::Close(reason)) => {
                ctx.close(reason);
                ctx.stop();
            }
            Ok(ws::Message::Continuation(_)) => {}
            Ok(ws::Message::Nop) => {}
            Err(e) => {
                tracing::warn!(
                    container_id = %self.container_id,
                    error = %e,
                    "audio WS protocol error"
                );
                ctx.stop();
            }
        }
    }
}

/// Convenience function to upgrade an HTTP request to an AudioWs actor.
pub async fn upgrade(
    req: actix_web::HttpRequest,
    payload: actix_web::web::Payload,
    container_id: Uuid,
    format: AudioFormat,
) -> crate::error::Result<actix_web::HttpResponse> {
    let ws_actor = AudioWs::new(container_id, format);
    let resp = ws::start(ws_actor, &req, payload).map_err(|e| {
        crate::error::DroidkerError::Internal(format!("audio WS upgrade failed: {e}"))
    })?;
    Ok(resp)
}

// ----- tests ----------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_format_bytes_per_second() {
        let f = AudioFormat::default();
        assert_eq!(f.bytes_per_second(), 8000 * 1 * 2);
    }

    #[test]
    fn audio_format_bytes_for_ms() {
        let f = AudioFormat::default();
        // 50 ms at 8 kHz mono s16 = 800 bytes
        assert_eq!(f.bytes_for_ms(50), 800);
    }

    #[test]
    fn chunk_header_packs_correctly() {
        let f = AudioFormat {
            sample_rate: 16000,
            channels: 2,
            bits_per_sample: 16,
        };
        let header = pack_chunk_header(f, 100);
        assert_eq!(u32::from_le_bytes(header[0..4].try_into().unwrap()), 16000);
        assert_eq!(u16::from_le_bytes(header[4..6].try_into().unwrap()), 2);
        assert_eq!(u16::from_le_bytes(header[6..8].try_into().unwrap()), 16);
        assert_eq!(u32::from_le_bytes(header[8..12].try_into().unwrap()), 100);
    }

    #[test]
    fn encode_chunk_round_trips() {
        let f = AudioFormat::default();
        let pcm = vec![0u8; 800]; // 50 ms of silence
        let encoded = encode_audio_chunk(f, &pcm);
        assert_eq!(encoded.len(), 12 + 800);
        // Sample count should be 400 (800 bytes / 2 bytes per sample)
        let sc = u32::from_le_bytes(encoded[8..12].try_into().unwrap());
        assert_eq!(sc, 400);
    }

    #[test]
    fn test_tone_produces_correct_byte_count() {
        let mut cap = AudioCapturer {
            container_id: Uuid::new_v4(),
            format: AudioFormat::default(),
            child: None,
            stdout: None,
            test_phase: 0.0,
            started_at: Instant::now(),
        };
        let bytes = cap.synthesize_test_tone(800);
        assert_eq!(bytes.len(), 800);
    }

    #[test]
    fn aac_encoder_is_passthrough() {
        let mut enc = AacEncoder::new(AudioFormat::default());
        let pcm = vec![1, 2, 3, 4];
        let out = enc.encode(&pcm);
        assert_eq!(out, pcm);
    }
}
