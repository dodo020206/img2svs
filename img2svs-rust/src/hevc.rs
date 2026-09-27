//! Native HEVC decoder for HEVC-compressed SDPC/DYQX tiles.
//!
//! SDPC stores every tile as a self-contained Annex-B stream - VPS, SPS, PPS,
//! a prefix SEI and one IDR picture - so no container parsing is needed and no
//! state has to be carried across tiles.  `rusty_h265` reconstructs the YUV420
//! planes in pure Rust, with runtime-dispatched SIMD kernels (SSE2/SSE4.1/AVX2
//! on x86-64) behind its default `simd` feature; that is what removed the
//! FFmpeg runtime DLLs the original implementation loaded through `libloading`.
//!
//! Two details matter for throughput and both are measured rather than assumed:
//!
//! * The decoder instance is *reused* across tiles.  Constructing one costs
//!   about 12% of a tile's decode time single-threaded, and the per-tile
//!   allocations it stops making cost more than that once twelve workers
//!   contend for the allocator.
//! * `flush()` is still required.  Pushing an access unit only queues it, so
//!   `next_frame()` returns nothing until the stream is flushed; a shared
//!   decoder without the flush hands out the *previous* tile's picture.
//!
//! The YUV to RGB conversion is a table-driven integer BT.601 with a plain
//! arithmetic shift, which is the pair libswscale uses for its default colour
//! space.  Against the FFmpeg decoder on the in-tree samples this keeps every
//! reconstructed sample identical and every RGB sample within two code values.
//!
//! Together the two changes took `t-sdpc` from 27,283 ms to 18,378 ms and
//! `t-sdpc1` from 4,215 ms to 3,089 ms, against 15,543 ms and 2,316 ms for the
//! FFmpeg build this replaced.  Both SVS outputs stayed byte-identical.

use anyhow::{bail, Context, Result};
use image::{Rgb, RgbImage};
use rusty_h265::{Decoder as HevcStream, Frame};

/// Pre-computed BT.601 terms, indexed by the raw 8-bit sample value.
///
/// The per-pixel maths collapses to three adds and a shift, and both luma
/// columns that share one chroma sample reuse the same three chroma terms.
/// This is what brings the conversion in line with libswscale's cost: the
/// straightforward scalar loop measured 1.90 s over the `t-sdpc1` tile set
/// against swscale's 1.33 s, the tables bring it to 1.35 s.
struct RgbTables {
    luma: [i32; 256],
    red: [i32; 256],
    green_cr: [i32; 256],
    green_cb: [i32; 256],
    blue: [i32; 256],
}

impl RgbTables {
    fn new() -> Box<Self> {
        let mut tables = Box::new(RgbTables {
            luma: [0; 256],
            red: [0; 256],
            green_cr: [0; 256],
            green_cb: [0; 256],
            blue: [0; 256],
        });
        for value in 0..256i32 {
            tables.luma[value as usize] = 298 * (value - 16);
            tables.red[value as usize] = 409 * (value - 128);
            tables.green_cr[value as usize] = -208 * (value - 128);
            tables.green_cb[value as usize] = -100 * (value - 128);
            tables.blue[value as usize] = 516 * (value - 128);
        }
        tables
    }
}

#[inline]
fn narrow(value: i32) -> u8 {
    (value >> 8).clamp(0, 255) as u8
}

/// Converts one row of YUV420 into `output`, which holds `cols` RGB triples.
///
/// `luma` is the luma row and `blue_chroma`/`red_chroma` are the two chroma
/// rows; every pixel pair shares one chroma sample, so the three chroma terms
/// are computed once per pair instead of once per pixel.
fn convert_row(
    tables: &RgbTables,
    luma: &[u8],
    blue_chroma: &[u8],
    red_chroma: &[u8],
    cols: usize,
    output: &mut [u8],
) {
    let pairs = cols / 2;
    let luma_pairs = &luma[..pairs * 2];
    let out_pairs = &mut output[..pairs * 6];
    for (pair, (pixel, luma_pair)) in out_pairs
        .chunks_exact_mut(6)
        .zip(luma_pairs.chunks_exact(2))
        .enumerate()
    {
        let cb = usize::from(blue_chroma[pair]);
        let cr = usize::from(red_chroma[pair]);
        let red = tables.red[cr];
        let green = tables.green_cb[cb] + tables.green_cr[cr];
        let blue = tables.blue[cb];
        let first = tables.luma[usize::from(luma_pair[0])];
        let second = tables.luma[usize::from(luma_pair[1])];
        pixel[0] = narrow(first + red);
        pixel[1] = narrow(first + green);
        pixel[2] = narrow(first + blue);
        pixel[3] = narrow(second + red);
        pixel[4] = narrow(second + green);
        pixel[5] = narrow(second + blue);
    }
    if cols % 2 == 1 {
        let cb = usize::from(blue_chroma[pairs]);
        let cr = usize::from(red_chroma[pairs]);
        let value = tables.luma[usize::from(luma[pairs * 2])];
        output[pairs * 6] = narrow(value + tables.red[cr]);
        output[pairs * 6 + 1] = narrow(value + tables.green_cb[cb] + tables.green_cr[cr]);
        output[pairs * 6 + 2] = narrow(value + tables.blue[cb]);
    }
}

/// Decodes HEVC tiles into RGB images.
pub struct Decoder {
    stream: HevcStream,
    tables: Box<RgbTables>,
}

impl Decoder {
    /// Creates a decoder.
    ///
    /// Kept fallible so the SVS writer's error handling does not need a special
    /// case now that no external runtime has to be located.
    pub fn new() -> Result<Self> {
        Ok(Self {
            stream: HevcStream::new(),
            tables: RgbTables::new(),
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
        self.stream
            .push_annexb(data, None)
            .context("HEVC tile was rejected")?;
        self.stream.flush();
        // `next_frame` reports `Error::Again` when the output queue drains, so
        // this loop terminates on every tile and keeps the last picture.
        let mut picture: Option<Frame> = None;
        while let Ok(frame) = self.stream.next_frame() {
            picture = Some(frame);
        }
        let picture = picture.context("HEVC tile produced no picture")?;

        if picture.bit_depth() != 8 {
            bail!(
                "HEVC tile uses {}-bit samples, only 8-bit is supported",
                picture.bit_depth()
            );
        }
        let source_width = u32::try_from(picture.width).context("HEVC picture is too wide")?;
        let source_height = u32::try_from(picture.height).context("HEVC picture is too tall")?;
        if source_width == 0 || source_height == 0 {
            bail!("HEVC tile decoded to an empty picture");
        }

        let mut planes = Vec::new();
        picture.write_yuv(&mut planes);
        let luma_len = picture.width * picture.height;
        if planes.len() < luma_len {
            bail!("HEVC tile YUV buffer is shorter than the decoded geometry");
        }
        let chroma_total = planes.len() - luma_len;
        if chroma_total % 2 != 0 {
            bail!("HEVC tile YUV buffer has an odd chroma size");
        }
        let chroma_width = source_width.div_ceil(2) as usize;
        let chroma_height = source_height.div_ceil(2) as usize;
        let chroma_len = chroma_total / 2;
        if chroma_len < chroma_width * chroma_height {
            bail!("HEVC tile chroma planes are shorter than the decoded geometry");
        }
        let (luma, chroma) = planes.split_at(luma_len);
        let (blue_chroma, red_chroma) = chroma.split_at(chroma_len);

        let rows = source_height.min(height) as usize;
        let cols = source_width.min(width) as usize;
        let mut image = RgbImage::from_pixel(width, height, Rgb([255, 255, 255]));
        let pixels: &mut [u8] = &mut image;
        let tables = &*self.tables;
        for (row, row_pixels) in pixels
            .chunks_exact_mut(width as usize * 3)
            .take(rows)
            .enumerate()
        {
            let luma_row = row * source_width as usize;
            let chroma_row = (row / 2) * chroma_width;
            convert_row(
                tables,
                &luma[luma_row..luma_row + cols],
                &blue_chroma[chroma_row..chroma_row + cols.div_ceil(2)],
                &red_chroma[chroma_row..chroma_row + cols.div_ceil(2)],
                cols,
                row_pixels,
            );
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

    /// The table-driven conversion must reproduce the plain scalar loop.
    #[test]
    fn tables_match_the_scalar_conversion() {
        let tables = RgbTables::new();
        let (width, height) = (7usize, 5usize);
        let chroma_width = width.div_ceil(2);
        let chroma_height = height.div_ceil(2);
        let mut state = 1u32;
        let mut next = move || {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (state >> 16) as u8
        };
        let luma: Vec<u8> = (0..width * height).map(|_| next()).collect();
        let blue: Vec<u8> = (0..chroma_width * chroma_height).map(|_| next()).collect();
        let red: Vec<u8> = (0..chroma_width * chroma_height).map(|_| next()).collect();

        let mut expected = vec![0u8; width * height * 3];
        for row in 0..height {
            for col in 0..width {
                let luma = i32::from(luma[row * width + col]) - 16;
                let chroma = (row / 2) * chroma_width + col / 2;
                let cb = i32::from(blue[chroma]) - 128;
                let cr = i32::from(red[chroma]) - 128;
                let index = (row * width + col) * 3;
                expected[index] = ((298 * luma + 409 * cr) >> 8).clamp(0, 255) as u8;
                expected[index + 1] = ((298 * luma - 100 * cb - 208 * cr) >> 8).clamp(0, 255) as u8;
                expected[index + 2] = ((298 * luma + 516 * cb) >> 8).clamp(0, 255) as u8;
            }
        }

        let mut actual = vec![0u8; width * height * 3];
        for row in 0..height {
            let luma_row = row * width;
            let chroma_row = (row / 2) * chroma_width;
            convert_row(
                &tables,
                &luma[luma_row..luma_row + width],
                &blue[chroma_row..chroma_row + chroma_width],
                &red[chroma_row..chroma_row + chroma_width],
                width,
                &mut actual[luma_row * 3..luma_row * 3 + width * 3],
            );
        }
        assert_eq!(expected, actual);
    }
}
