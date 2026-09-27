//! Native HEVC decoder for HEVC-compressed SDPC/DYQX tiles.
//!
//! SDPC stores every tile as a self-contained Annex-B stream - VPS, SPS, PPS,
//! a prefix SEI and one IDR picture - so no state is carried between tiles and
//! no container parsing is needed.  `rust_h265` reconstructs the YUV420 planes
//! in pure Rust, which removes the FFmpeg runtime DLLs the previous
//! implementation loaded through `libloading`.
//!
//! The YUV to RGB conversion uses integer BT.601 coefficients with a plain
//! arithmetic shift, which is the pair libswscale uses for its default colour
//! space.  Measured against the FFmpeg decoder on the in-tree samples this
//! keeps every reconstructed sample identical and every RGB sample within two
//! code values.

use anyhow::{bail, Context, Result};
use image::{Rgb, RgbImage};
use rust_h265::{parse_annex_b, Decoder as HevcStream, PixelData};

/// Decodes HEVC tiles into RGB images.
pub struct Decoder {
    stream: HevcStream,
}

impl Decoder {
    /// Creates a decoder.
    ///
    /// Kept fallible so the SVS writer's error handling does not need a special
    /// case now that no external runtime has to be located.
    pub fn new() -> Result<Self> {
        Ok(Self {
            stream: HevcStream::new(),
        })
    }

    /// Decodes one Annex-B HEVC tile into an RGB image.
    ///
    /// `width` and `height` are the geometry the SVS writer expects: a smaller
    /// decoded picture is padded with white and a larger one is cropped.
    pub fn decode(&mut self, data: &[u8], width: u32, height: u32) -> Result<RgbImage> {
        if data.is_empty() {
            bail!("empty HEVC tile");
        }
        // A tile carries its own VPS/SPS/PPS and a single IDR picture, and the
        // stream is flushed at the end of every call, so nothing is carried
        // over from the previous tile.
        let mut picture = None;
        for nal in &parse_annex_b(data) {
            match self.stream.decode_nal(nal) {
                Ok(Some(frame)) => picture = Some(frame),
                Ok(None) => {}
                Err(error) => {
                    self.stream = HevcStream::new();
                    bail!("HEVC tile decode failed: {error:?}");
                }
            }
        }
        while let Some(frame) = self.stream.flush() {
            picture = Some(frame);
        }
        let picture = picture.context("HEVC tile produced no picture")?;

        let (PixelData::U8(luma), PixelData::U8(blue_chroma), PixelData::U8(red_chroma)) =
            (&picture.y, &picture.u, &picture.v)
        else {
            bail!(
                "HEVC tile uses {}-bit samples, only 8-bit is supported",
                picture.bit_depth
            );
        };
        let source_width = picture.width;
        let source_height = picture.height;
        if source_width == 0 || source_height == 0 {
            bail!("HEVC tile decoded to an empty picture");
        }
        let chroma_width = source_width.div_ceil(2) as usize;
        let chroma_height = source_height.div_ceil(2) as usize;
        let luma_len = source_width as usize * source_height as usize;
        let chroma_len = chroma_width * chroma_height;
        if luma_len > luma.len() || chroma_len > blue_chroma.len() || chroma_len > red_chroma.len()
        {
            bail!("HEVC tile planes are shorter than the decoded geometry");
        }

        let rows = source_height.min(height) as usize;
        let cols = source_width.min(width) as usize;
        let mut image = RgbImage::from_pixel(width, height, Rgb([255, 255, 255]));
        let pixels: &mut [u8] = &mut image;
        for (row, row_pixels) in pixels
            .chunks_exact_mut(width as usize * 3)
            .take(rows)
            .enumerate()
        {
            let luma_row = row * source_width as usize;
            let chroma_row = (row / 2) * chroma_width;
            for (col, pixel) in row_pixels.chunks_exact_mut(3).take(cols).enumerate() {
                let luma = i32::from(luma[luma_row + col]) - 16;
                let chroma = chroma_row + col / 2;
                let cb = i32::from(blue_chroma[chroma]) - 128;
                let cr = i32::from(red_chroma[chroma]) - 128;
                pixel[0] = ((298 * luma + 409 * cr) >> 8).clamp(0, 255) as u8;
                pixel[1] = ((298 * luma - 100 * cb - 208 * cr) >> 8).clamp(0, 255) as u8;
                pixel[2] = ((298 * luma + 516 * cb) >> 8).clamp(0, 255) as u8;
            }
        }
        Ok(image)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_tile_is_rejected() {
        let mut decoder = Decoder::new().unwrap();
        assert!(decoder.decode(&[], 8, 8).is_err());
    }

    #[test]
    fn garbage_is_rejected() {
        let mut decoder = Decoder::new().unwrap();
        assert!(decoder.decode(&[0u8; 64], 8, 8).is_err());
    }
}
