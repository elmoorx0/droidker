// src/streaming/capture.rs
//
// Frame capturer for DroidKer containers.
//
// The capturer runs inside a tokio task, periodically grabbing the current
// framebuffer of a running container and returning it as an RGB888 buffer
// plus the dimensions. Three capture sources are tried in priority order:
//
//   1. `screencap -p` via nsenter — emits a PNG that we decode to RGB888.
//      Works on a real Android runtime with SurfaceFlinger up.
//   2. `/dev/graphics/fb0` mmap via nsenter — raw kernel framebuffer.
//      Used when the device doesn't have SurfaceFlinger (e.g. headless
//      Android Things).
//   3. Test pattern — a synthetic 540x960 RGB image with the container ID
//      and a clock overlaid. Used in dev mode and on the skeleton rootfs.
//
// The capturer tries source #1 first; on failure it falls back to #2, then
// #3. Once a source succeeds it's cached so we don't fork `nsenter` every
// frame only to discover the failure mode repeatedly.

use crate::error::{DroidkerError, Result};
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;
use uuid::Uuid;

/// One captured frame: raw RGB888 pixels plus dimensions.
#[derive(Debug, Clone)]
pub struct CapturedFrame {
    pub width: u32,
    pub height: u32,
    pub rgb: Vec<u8>,
    /// Where this frame came from. Useful for diagnostics.
    pub source: CaptureSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureSource {
    /// `screencap -p` PNG output.
    Screencap,
    /// `/dev/graphics/fb0` raw framebuffer.
    Framebuffer,
    /// Synthetic test pattern.
    TestPattern,
}

impl CaptureSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            CaptureSource::Screencap => "screencap",
            CaptureSource::Framebuffer => "fb0",
            CaptureSource::TestPattern => "test-pattern",
        }
    }
}

pub struct FrameCapturer {
    container_id: Uuid,
    target_pid: u32,
    /// Cached successful source. None means we haven't picked one yet.
    cached_source: Option<CaptureSource>,
    /// Maximum width. Frames wider than this are downscaled by skipping
    /// every other pixel column (cheap, looks fine for screen viewing).
    max_width: u32,
}

impl FrameCapturer {
    pub fn new(container_id: Uuid, target_pid: u32, max_width: u32) -> Self {
        Self {
            container_id,
            target_pid,
            cached_source: None,
            max_width,
        }
    }

    /// Grab one frame. Tries the cached source first; on failure walks the
    /// priority list and updates the cache.
    pub async fn capture(&mut self) -> Result<CapturedFrame> {
        // If we have a cached source, try it first.
        if let Some(src) = self.cached_source {
            match self.try_source(src).await {
                Ok(f) => return Ok(f),
                Err(e) => {
                    tracing::debug!(source = src.as_str(), error = %e, "cached capture source failed; falling back");
                    self.cached_source = None;
                }
            }
        }

        // Walk the priority list.
        for src in [CaptureSource::Screencap, CaptureSource::Framebuffer, CaptureSource::TestPattern] {
            match self.try_source(src).await {
                Ok(f) => {
                    self.cached_source = Some(src);
                    return Ok(f);
                }
                Err(e) => {
                    tracing::debug!(source = src.as_str(), error = %e, "capture source unavailable");
                }
            }
        }
        Err(DroidkerError::Internal(
            "all capture sources failed".into(),
        ))
    }

    async fn try_source(&self, src: CaptureSource) -> Result<CapturedFrame> {
        match src {
            CaptureSource::Screencap => self.capture_screencap().await,
            CaptureSource::Framebuffer => self.capture_fb0().await,
            CaptureSource::TestPattern => Ok(self.capture_test_pattern()),
        }
    }

    // ---- Source 1: screencap -p via nsenter -------------------------------

    async fn capture_screencap(&self) -> Result<CapturedFrame> {
        let mut cmd = Command::new("nsenter");
        cmd.arg(format!("--target={}", self.target_pid))
            .args(["--pid", "--mount", "--"])
            .args(["/system/bin/screencap", "-p"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);

        // screencap can hang if SurfaceFlinger isn't up. Cap it at 2s.
        let outcome = tokio::time::timeout(Duration::from_secs(2), cmd.output()).await;
        let output = match outcome {
            Ok(Ok(o)) if o.status.success() => o.stdout,
            Ok(Ok(o)) => {
                return Err(DroidkerError::Internal(format!(
                    "screencap exited with status {}",
                    o.status
                )));
            }
            Ok(Err(e)) => {
                return Err(DroidkerError::Internal(format!(
                    "screencap spawn failed: {e}"
                )));
            }
            Err(_) => {
                return Err(DroidkerError::Internal("screencap timed out".into()));
            }
        };

        if output.is_empty() {
            return Err(DroidkerError::Internal("screencap produced no output".into()));
        }

        // Decode PNG → RGB888.
        let decoder = png::Decoder::new(output.as_slice());
        let mut reader = decoder
            .read_info()
            .map_err(|e| DroidkerError::Internal(format!("png decode: {e}")))?;
        // Snapshot the info before calling next_frame (which needs &mut self).
        let (width, height, color_type) = {
            let info = reader.info();
            (info.width, info.height, info.color_type)
        };
        let mut buf = vec![0u8; reader.output_buffer_size()];
        let frame = reader
            .next_frame(&mut buf)
            .map_err(|e| DroidkerError::Internal(format!("png read: {e}")))?;
        let data = &buf[..frame.buffer_size()];

        let rgb = match color_type {
            png::ColorType::Rgb => data.to_vec(),
            png::ColorType::Rgba => rgba_to_rgb(data),
            png::ColorType::Grayscale => gray_to_rgb(data),
            png::ColorType::GrayscaleAlpha => {
                gray_alpha_to_rgb(data)
            }
            png::ColorType::Indexed => {
                return Err(DroidkerError::Internal(
                    "indexed PNG not supported from screencap".into(),
                ));
            }
        };

        Ok(self.maybe_downscale(CapturedFrame {
            width,
            height,
            rgb,
            source: CaptureSource::Screencap,
        }))
    }

    // ---- Source 2: /dev/graphics/fb0 mmap ----------------------------------
    //
    // We don't actually mmap fb0 from the host because the host's fb0 is
    // a different device than the container's. Instead we cat it via
    // nsenter into the container's mount namespace.
    //
    // The framebuffer's pixel format varies (BGRA8888 on most Android,
    // RGB565 on older devices). We assume BGRA8888 because that's what
    // every Android device since 4.x uses. If we get the format wrong the
    // image will look color-swapped, which is good enough for diagnostics.

    async fn capture_fb0(&self) -> Result<CapturedFrame> {
        let mut cmd = Command::new("nsenter");
        cmd.arg(format!("--target={}", self.target_pid))
            .args(["--pid", "--mount", "--"])
            .args(["/system/bin/cat", "/dev/graphics/fb0"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);

        let outcome = tokio::time::timeout(Duration::from_secs(2), cmd.output()).await;
        let output = match outcome {
            Ok(Ok(o)) if o.status.success() => o.stdout,
            _ => {
                return Err(DroidkerError::Internal(
                    "fb0 read failed".into(),
                ));
            }
        };

        // Hardcode 540x960 BGRA8888 for the skeleton rootfs. In production
        // we'd ioctl(FBIOGET_VSCREENINFO) to get the real dimensions.
        let width = 540u32;
        let height = 960u32;
        let expected = (width * height * 4) as usize;
        if output.len() < expected {
            return Err(DroidkerError::Internal(format!(
                "fb0 too small: {} bytes < {} expected",
                output.len(),
                expected
            )));
        }
        let rgb = bgra_to_rgb(&output[..expected]);
        Ok(self.maybe_downscale(CapturedFrame {
            width,
            height,
            rgb,
            source: CaptureSource::Framebuffer,
        }))
    }

    // ---- Source 3: synthetic test pattern ---------------------------------

    fn capture_test_pattern(&self) -> CapturedFrame {
        let width = 540u32;
        let height = 960u32;
        let mut rgb = Vec::with_capacity((width * height * 3) as usize);

        // Background: diagonal gradient from dark blue to dark purple, so
        // it's obvious this is a synthetic frame and not a real screen.
        let now = chrono::Utc::now().timestamp_millis() as u32;
        for y in 0..height {
            for x in 0..width {
                let r = ((x * 40) / width) as u8;
                let g = ((y * 30) / height) as u8;
                let b = 80u8.saturating_add(((x + y) % 80) as u8);
                rgb.extend_from_slice(&[r, g, b]);
            }
        }

        // Overlay the container ID (just the first 8 chars) as a white
        // block in the top-left corner. We draw 8x8 monospace chars by
        // hand — keeping it dependency-free.
        let id_str = format!("{:08x}", self.container_id.as_u128() & 0xFFFF_FFFF_FFFF_FFFF);
        let label = format!("DroidKer  {}  t={}", &id_str[..8], now % 1000);
        draw_text(&mut rgb, width, height, 16, 16, &label);

        // Moving dot — proves the stream is live.
        let dot_x = ((now / 30) % width) as usize;
        let dot_y = (height as usize) / 2;
        for dy in 0..16 {
            for dx in 0..16 {
                let px = dot_x + dx;
                let py = dot_y + dy;
                if px >= width as usize || py >= height as usize {
                    continue;
                }
                let i = (py * width as usize + px) * 3;
                if i + 2 < rgb.len() {
                    rgb[i] = 255;
                    rgb[i + 1] = 255;
                    rgb[i + 2] = 255;
                }
            }
        }

        CapturedFrame {
            width,
            height,
            rgb,
            source: CaptureSource::TestPattern,
        }
    }

    /// If `frame.width > max_width`, downscale by nearest-neighbor sampling.
    /// This keeps the JPEG encoding cost predictable on a 1-vCPU VPS.
    fn maybe_downscale(&self, frame: CapturedFrame) -> CapturedFrame {
        if frame.width <= self.max_width {
            return frame;
        }
        let scale = (frame.width + self.max_width - 1) / self.max_width;
        let new_w = frame.width / scale;
        let new_h = frame.height / scale;
        let mut out = Vec::with_capacity((new_w * new_h * 3) as usize);
        for y in 0..new_h {
            for x in 0..new_w {
                let src_x = x * scale;
                let src_y = y * scale;
                let i = ((src_y * frame.width + src_x) * 3) as usize;
                out.extend_from_slice(&frame.rgb[i..i + 3]);
            }
        }
        CapturedFrame {
            width: new_w,
            height: new_h,
            rgb: out,
            source: frame.source,
        }
    }
}

// ----- Pixel format helpers -------------------------------------------------

fn rgba_to_rgb(rgba: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rgba.len() * 3 / 4);
    for chunk in rgba.chunks_exact(4) {
        out.extend_from_slice(&chunk[..3]);
    }
    out
}

fn bgra_to_rgb(bgra: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bgra.len() * 3 / 4);
    for chunk in bgra.chunks_exact(4) {
        // BGRA → RGB
        out.push(chunk[2]);
        out.push(chunk[1]);
        out.push(chunk[0]);
    }
    out
}

fn gray_to_rgb(g: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(g.len() * 3);
    for &v in g {
        out.extend_from_slice(&[v, v, v]);
    }
    out
}

fn gray_alpha_to_rgb(ga: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(ga.len() * 3 / 2);
    for chunk in ga.chunks_exact(2) {
        let v = chunk[0];
        out.extend_from_slice(&[v, v, v]);
    }
    out
}

// ----- 8x8 monospace text renderer -----------------------------------------
//
// Each ASCII char is rendered from a 8x8 bitmap stored as 8 bytes (one per
// row, MSB first). We only need [A-Za-z0-9 _=:/] for the test pattern
// label. Unknown chars render as a blank.

fn draw_text(buf: &mut [u8], width: u32, height: u32, x0: u32, y0: u32, text: &str) {
    let mut x = x0;
    for ch in text.chars() {
        let glyph = glyph_for(ch);
        for (row, &bits) in glyph.iter().enumerate() {
            for col in 0..8u32 {
                if bits & (0x80 >> col) != 0 {
                    let px = (x + col) as usize;
                    let py = (y0 + row as u32) as usize;
                    if px < width as usize && py < height as usize {
                        let i = (py * width as usize + px) * 3;
                        if i + 2 < buf.len() {
                            buf[i] = 255;
                            buf[i + 1] = 255;
                            buf[i + 2] = 255;
                        }
                    }
                }
            }
        }
        x += 8;
        if x + 8 > width {
            break;
        }
    }
}

/// Return an 8x8 bitmap for the given char. Unknown chars → all zeros.
fn glyph_for(c: char) -> [u8; 8] {
    // We use a tiny subset of the classic 8x8 font. Each row is 8 bits,
    // MSB = leftmost pixel.
    match c {
        ' ' => [0; 8],
        '0' => [0x3C, 0x42, 0x42, 0x42, 0x42, 0x42, 0x3C, 0],
        '1' => [0x08, 0x18, 0x28, 0x08, 0x08, 0x08, 0x3E, 0],
        '2' => [0x3C, 0x42, 0x02, 0x0C, 0x30, 0x40, 0x7E, 0],
        '3' => [0x3C, 0x42, 0x02, 0x0C, 0x02, 0x42, 0x3C, 0],
        '4' => [0x04, 0x0C, 0x14, 0x24, 0x7E, 0x04, 0x04, 0],
        '5' => [0x7E, 0x40, 0x7C, 0x02, 0x02, 0x42, 0x3C, 0],
        '6' => [0x0C, 0x10, 0x20, 0x7C, 0x42, 0x42, 0x3C, 0],
        '7' => [0x7E, 0x02, 0x04, 0x08, 0x10, 0x10, 0x10, 0],
        '8' => [0x3C, 0x42, 0x42, 0x3C, 0x42, 0x42, 0x3C, 0],
        '9' => [0x3C, 0x42, 0x42, 0x3E, 0x02, 0x04, 0x18, 0],
        'a' | 'A' => [0x3C, 0x42, 0x42, 0x7E, 0x42, 0x42, 0x42, 0],
        'b' | 'B' => [0x7C, 0x42, 0x42, 0x7C, 0x42, 0x42, 0x7C, 0],
        'c' | 'C' => [0x3C, 0x42, 0x40, 0x40, 0x40, 0x42, 0x3C, 0],
        'd' | 'D' => [0x7C, 0x42, 0x42, 0x42, 0x42, 0x42, 0x7C, 0],
        'e' | 'E' => [0x7E, 0x40, 0x40, 0x7C, 0x40, 0x40, 0x7E, 0],
        'f' | 'F' => [0x7E, 0x40, 0x40, 0x7C, 0x40, 0x40, 0x40, 0],
        'g' | 'G' => [0x3C, 0x42, 0x40, 0x4E, 0x42, 0x42, 0x3E, 0],
        'h' | 'H' => [0x42, 0x42, 0x42, 0x7E, 0x42, 0x42, 0x42, 0],
        'i' | 'I' => [0x3E, 0x08, 0x08, 0x08, 0x08, 0x08, 0x3E, 0],
        'j' | 'J' => [0x02, 0x02, 0x02, 0x02, 0x42, 0x42, 0x3C, 0],
        'k' | 'K' => [0x42, 0x44, 0x48, 0x70, 0x48, 0x44, 0x42, 0],
        'l' | 'L' => [0x40, 0x40, 0x40, 0x40, 0x40, 0x40, 0x7E, 0],
        'm' | 'M' => [0x42, 0x66, 0x5A, 0x42, 0x42, 0x42, 0x42, 0],
        'n' | 'N' => [0x42, 0x62, 0x52, 0x4A, 0x46, 0x42, 0x42, 0],
        'o' | 'O' => [0x3C, 0x42, 0x42, 0x42, 0x42, 0x42, 0x3C, 0],
        'p' | 'P' => [0x7C, 0x42, 0x42, 0x7C, 0x40, 0x40, 0x40, 0],
        'q' | 'Q' => [0x3C, 0x42, 0x42, 0x42, 0x4A, 0x44, 0x3A, 0],
        'r' | 'R' => [0x7C, 0x42, 0x42, 0x7C, 0x48, 0x44, 0x42, 0],
        's' | 'S' => [0x3C, 0x42, 0x40, 0x3C, 0x02, 0x42, 0x3C, 0],
        't' | 'T' => [0x7E, 0x08, 0x08, 0x08, 0x08, 0x08, 0x08, 0],
        'u' | 'U' => [0x42, 0x42, 0x42, 0x42, 0x42, 0x42, 0x3C, 0],
        'v' | 'V' => [0x42, 0x42, 0x42, 0x42, 0x42, 0x24, 0x18, 0],
        'w' | 'W' => [0x42, 0x42, 0x42, 0x42, 0x5A, 0x66, 0x42, 0],
        'x' | 'X' => [0x42, 0x42, 0x24, 0x18, 0x24, 0x42, 0x42, 0],
        'y' | 'Y' => [0x42, 0x42, 0x42, 0x24, 0x18, 0x08, 0x08, 0],
        'z' | 'Z' => [0x7E, 0x02, 0x04, 0x08, 0x10, 0x20, 0x7E, 0],
        '=' => [0, 0x18, 0x18, 0x18, 0x18, 0x18, 0, 0],
        ':' => [0, 0x18, 0x18, 0, 0, 0x18, 0x18, 0],
        '/' => [0x02, 0x04, 0x04, 0x08, 0x10, 0x10, 0x20, 0],
        '_' => [0, 0, 0, 0, 0, 0, 0, 0x3C],
        '-' => [0, 0, 0x18, 0x18, 0x18, 0x18, 0, 0],
        '+' => [0, 0x18, 0x18, 0x7E, 0x18, 0x18, 0, 0],
        '.' => [0, 0, 0, 0, 0, 0x18, 0x18, 0],
        ',' => [0, 0, 0, 0, 0, 0x18, 0x08, 0x10],
        _ => [0; 8],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pattern_has_correct_size() {
        let c = FrameCapturer::new(Uuid::new_v4(), 1, 540);
        let f = c.capture_test_pattern();
        assert_eq!(f.width, 540);
        assert_eq!(f.height, 960);
        assert_eq!(f.rgb.len(), 540 * 960 * 3);
        assert_eq!(f.source, CaptureSource::TestPattern);
    }

    #[test]
    fn bgra_to_rgb_swaps_channels() {
        let bgra = [10u8, 20, 30, 255];
        let rgb = bgra_to_rgb(&bgra);
        assert_eq!(rgb, [30, 20, 10]);
    }

    #[test]
    fn rgba_to_rgb_drops_alpha() {
        let rgba = [10u8, 20, 30, 255];
        let rgb = rgba_to_rgb(&rgba);
        assert_eq!(rgb, [10, 20, 30]);
    }

    #[test]
    fn downscale_halves_resolution() {
        let c = FrameCapturer::new(Uuid::new_v4(), 1, 100);
        let f = CapturedFrame {
            width: 200,
            height: 100,
            rgb: vec![0; 200 * 100 * 3],
            source: CaptureSource::TestPattern,
        };
        let out = c.maybe_downscale(f);
        assert_eq!(out.width, 100);
        assert_eq!(out.height, 50);
        assert_eq!(out.rgb.len(), 100 * 50 * 3);
    }

    #[test]
    fn downscale_skipped_when_within_budget() {
        let c = FrameCapturer::new(Uuid::new_v4(), 1, 540);
        let f = CapturedFrame {
            width: 540,
            height: 960,
            rgb: vec![0; 540 * 960 * 3],
            source: CaptureSource::TestPattern,
        };
        let out = c.maybe_downscale(f);
        assert_eq!(out.width, 540);
        assert_eq!(out.height, 960);
    }

    #[test]
    fn glyph_for_known_char_returns_nonzero() {
        let g = glyph_for('A');
        assert!(g.iter().any(|&b| b != 0));
        let g = glyph_for(' ');
        assert!(g.iter().all(|&b| b == 0));
    }

    #[test]
    fn draw_text_doesnt_panic_on_long_string() {
        let mut buf = vec![0u8; 540 * 960 * 3];
        draw_text(&mut buf, 540, 960, 16, 16, "DroidKer test pattern with a long label that exceeds width");
        // No assertion needed — just verifying no out-of-bounds.
    }
}
