// src/streaming/encoder.rs
//
// JPEG encoder wrapper around `jpeg-encoder` (pure Rust, no native deps).
//
// We expose a single function `encode(width, height, rgb, quality) -> Vec<u8>`
// that returns a complete JPEG file. The encoder is created per call —
// `jpeg-encoder` is stateless across encodes, so we don't pool instances.

use crate::error::{DroidkerError, Result};
use jpeg_encoder::{ColorType, Encoder as JpegEnc, EncodingError};

pub struct JpegEncoder {
    quality: u8,
}

impl JpegEncoder {
    pub fn new(quality: u8) -> Self {
        // Clamp to jpeg-encoder's valid range (1-100).
        let quality = quality.clamp(1, 100);
        Self { quality }
    }

    /// Encode an RGB888 buffer as a JPEG file.
    ///
    /// `rgb.len()` must equal `width * height * 3`.
    pub fn encode(&self, width: u32, height: u32, rgb: &[u8]) -> Result<Vec<u8>> {
        let expected = (width as usize) * (height as usize) * 3;
        if rgb.len() != expected {
            return Err(DroidkerError::Internal(format!(
                "jpeg encode: rgb buffer wrong size ({} != {}*{}*3 = {})",
                rgb.len(),
                width,
                height,
                expected
            )));
        }
        // jpeg-encoder writes the encoded JPEG into the `W: JfifWrite` we
        // pass to `Encoder::new`. `Vec<u8>` implements `JfifWrite`, so we
        // use it directly.
        let mut out: Vec<u8> = Vec::with_capacity(expected / 4);
        let enc = JpegEnc::new(&mut out, self.quality);
        enc.encode(rgb, width as u16, height as u16, ColorType::Rgb)
            .map_err(|e: EncodingError| DroidkerError::Internal(format!("jpeg encode: {e}")))?;
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_solid_color_frame() {
        let enc = JpegEncoder::new(60);
        let w = 8u32;
        let h = 8u32;
        // Solid red frame.
        let mut rgb = Vec::with_capacity((w * h * 3) as usize);
        for _ in 0..(w * h) {
            rgb.extend_from_slice(&[255, 0, 0]);
        }
        let jpeg = enc.encode(w, h, &rgb).unwrap();
        // JPEG files start with SOI marker 0xFFD8 and end with EOI 0xFFD9.
        assert_eq!(&jpeg[0..2], &[0xFF, 0xD8]);
        assert_eq!(&jpeg[jpeg.len() - 2..], &[0xFF, 0xD9]);
    }

    #[test]
    fn rejects_misized_buffer() {
        let enc = JpegEncoder::new(50);
        let bogus = vec![0u8; 10];
        assert!(enc.encode(8, 8, &bogus).is_err());
    }

    #[test]
    fn clamps_quality_to_valid_range() {
        let enc = JpegEncoder::new(0);
        assert!(enc.quality >= 1);
        let enc = JpegEncoder::new(200);
        assert!(enc.quality <= 100);
    }
}
