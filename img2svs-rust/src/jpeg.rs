//! JPEG decode/encode helpers for tile and associated-image handling.
//!
//! The SVS writer requires every tile to be baseline 4:2:0 JPEG, so this module
//! deliberately fixes the sampling factor and offers a YCbCr-to-YCbCr
//! transcoder that re-encodes without a colour conversion round trip.

use anyhow::{bail, Context, Result};
use image::{ImageFormat, RgbImage};
use jpeg_encoder::{ColorType, Encoder, SamplingFactor};
use zune_core::{bytestream::ZCursor, colorspace::ColorSpace, options::DecoderOptions};
use zune_jpeg::JpegDecoder;

/// Divisor used to guess the encoded size of a tile from its raw RGB length.
/// Only seeds the output buffer, so an inaccurate guess costs nothing worse
/// than a reallocation.
const RAW_TO_JPEG_SIZE_DIVISOR: usize = 8;

/// Lower bound for pre-allocated JPEG output buffers.
const MIN_JPEG_BUFFER: usize = 1024;

/// Start-of-image marker that opens every JPEG stream.
pub const SOI_MARKER: [u8; 2] = [0xff, 0xd8];

/// End-of-image marker that closes every JPEG stream.
pub const EOI_MARKER: [u8; 2] = [0xff, 0xd9];

/// Joins a TIFF `JPEGTables` stream with one strip's abbreviated stream.
///
/// TIFF Compression=7 lets a stripped page keep quantization and Huffman
/// tables once in tag 347 and let every strip omit them.  The tables hold
/// `SOI + DQT + DHT + EOI`, a strip holds `SOI + SOF + SOS + entropy + EOI`,
/// so the two pieces splice into a self-contained JPEG once the duplicated
/// markers are dropped.  Vendors that store full streams instead leave the
/// tables empty, in which case the strip is returned untouched.
pub fn merge_jpeg_tables(tables: &[u8], strip: &[u8]) -> Result<Vec<u8>> {
    if tables.is_empty() {
        return Ok(strip.to_vec());
    }
    if !tables.starts_with(&SOI_MARKER) {
        bail!("TIFF JPEGTables stream does not start with SOI");
    }
    let tables = tables.strip_suffix(&EOI_MARKER).unwrap_or(tables);
    let body = strip.strip_prefix(&SOI_MARKER).unwrap_or(strip);
    let mut merged = Vec::with_capacity(tables.len() + body.len() + 2);
    merged.extend_from_slice(&tables[..]);
    merged.extend_from_slice(body);
    if !merged.ends_with(&EOI_MARKER) {
        merged.extend_from_slice(&EOI_MARKER);
    }
    Ok(merged)
}

/// Splits a full interchange JPEG into a tables-only stream (for the TIFF
/// `JPEGTables` tag) and an abbreviated stream (for the tile payload).
///
/// TIFF Compression=7 allows tiles to omit quantization and Huffman tables
/// when the IFD carries them once in `JPEGTables`; at ~590 bytes of tables per
/// tile this is the difference between a 3.8 GB and a 3.2 GB MRXS conversion.
/// The encoder's tables depend only on the quality setting, so every tile we
/// re-encode in one run shares the same tables. Tiles passed through from the
/// source container keep their own embedded tables and stay full streams.
pub fn split_jpeg_tables(data: &[u8]) -> Result<(Vec<u8>, Vec<u8>)> {
    if !data.starts_with(&SOI_MARKER) {
        bail!("JPEG stream does not start with SOI");
    }
    let mut tables = Vec::with_capacity(640);
    let mut abbreviated = Vec::with_capacity(data.len());
    tables.extend_from_slice(&SOI_MARKER);
    abbreviated.extend_from_slice(&SOI_MARKER);

    let mut cursor = 2;
    while cursor + 4 <= data.len() {
        if data[cursor] != 0xff {
            bail!("malformed JPEG segment");
        }
        let marker = data[cursor + 1];
        if marker == 0xda {
            // SOS: everything from here to the end is entropy data plus EOI.
            abbreviated.extend_from_slice(&data[cursor..]);
            break;
        }
        if marker == 0xd8 || marker == 0xd9 || (0xd0..=0xd7).contains(&marker) {
            abbreviated.extend_from_slice(&data[cursor..cursor + 2]);
            cursor += 2;
            continue;
        }
        let length = usize::from(u16::from_be_bytes([data[cursor + 2], data[cursor + 3]]));
        let end = cursor + 2 + length;
        if end > data.len() {
            bail!("truncated JPEG segment");
        }
        if marker == 0xdb || marker == 0xc4 {
            tables.extend_from_slice(&data[cursor..end]);
        } else {
            abbreviated.extend_from_slice(&data[cursor..end]);
        }
        cursor = end;
    }
    if !abbreviated.ends_with(&[0xff, 0xd9]) {
        bail!("JPEG stream is missing its entropy data");
    }
    tables.extend_from_slice(&[0xff, 0xd9]);
    Ok((tables, abbreviated))
}

/// Decodes a JPEG buffer into a freshly allocated RGB image.
pub fn decode_rgb(data: &[u8]) -> Result<RgbImage> {
    Ok(image::load_from_memory_with_format(data, ImageFormat::Jpeg)
        .context("decode JPEG tile")?
        .to_rgb8())
}

/// Decodes an embedded image whose format is sniffed from the bytes.
///
/// Used for label and macro pages, which vendors store as PNG, JPEG or BMP.
pub fn decode_image(data: &[u8]) -> Result<RgbImage> {
    Ok(image::load_from_memory(data)
        .context("decode embedded image")?
        .to_rgb8())
}

/// Encodes `image` as a 4:2:0 JPEG at `quality`.
pub fn encode_jpeg(image: &RgbImage, quality: u8) -> Result<Vec<u8>> {
    encode_jpeg_with_capacity(
        image,
        quality,
        image.as_raw().len() / RAW_TO_JPEG_SIZE_DIVISOR,
    )
}

/// Encodes `image` as a 4:2:0 JPEG, reserving `estimated_size` bytes up front.
pub fn encode_jpeg_with_capacity(
    image: &RgbImage,
    quality: u8,
    estimated_size: usize,
) -> Result<Vec<u8>> {
    let width = u16::try_from(image.width()).context("JPEG width exceeds 65535 pixels")?;
    let height = u16::try_from(image.height()).context("JPEG height exceeds 65535 pixels")?;
    let mut output = Vec::with_capacity(estimated_size.max(MIN_JPEG_BUFFER));
    let mut encoder = Encoder::new(&mut output, quality);
    encoder.set_sampling_factor(SamplingFactor::F_2_2);
    encoder
        .encode(image.as_raw(), width, height, ColorType::Rgb)
        .context("encode JPEG")?;
    Ok(output)
}

/// Re-encodes an existing JPEG as 4:2:0 without converting its colour space.
///
/// Keeps the source YCbCr samples, which both preserves quality and avoids the
/// cost of a full RGB round trip on every source tile.
pub fn transcode_jpeg_to_420(data: &[u8], quality: u8) -> Result<Vec<u8>> {
    let options = DecoderOptions::default().jpeg_set_out_colorspace(ColorSpace::YCbCr);
    let mut decoder = JpegDecoder::new_with_options(ZCursor::new(data), options);
    let pixels = decoder.decode().context("decode JPEG tile as YCbCr")?;
    let info = decoder
        .info()
        .context("JPEG tile has no image information")?;
    ensure_three_component_ycbcr(&pixels, info.width, info.height)?;

    let mut output = Vec::with_capacity(data.len().max(MIN_JPEG_BUFFER));
    let mut encoder = Encoder::new(&mut output, quality);
    encoder.set_sampling_factor(SamplingFactor::F_2_2);
    encoder
        .encode(&pixels, info.width, info.height, ColorType::Ycbcr)
        .context("encode JPEG from YCbCr")?;
    Ok(output)
}

/// Verifies the decoder produced planar-free three-component YCbCr samples.
///
/// Greyscale or CMYK sources would otherwise be written out as broken tiles.
fn ensure_three_component_ycbcr(pixels: &[u8], width: u16, height: u16) -> Result<()> {
    let expected = usize::from(width) * usize::from(height) * 3;
    if pixels.len() != expected {
        bail!("JPEG tile is not a three-component YCbCr image");
    }
    Ok(())
}

/// A pure white image, used to pad tiles that fall outside the slide bounds.
pub fn white_image(width: u32, height: u32) -> RgbImage {
    RgbImage::from_pixel(width, height, image::Rgb([255, 255, 255]))
}

/// Scales `image` down so its longer edge is at most `max_size` pixels.
///
/// Images already smaller than `max_size` are returned unchanged; the result is
/// never upscaled.
pub fn thumbnail(image: &RgbImage, max_size: u32) -> RgbImage {
    let (width, height) = image.dimensions();
    let scale = fit_scale(width, height, max_size);
    image::imageops::resize(
        image,
        scaled_dimension(width, scale),
        scaled_dimension(height, scale),
        image::imageops::FilterType::Lanczos3,
    )
}

/// Largest factor `<= 1.0` that brings the longer edge within `max_size`.
fn fit_scale(width: u32, height: u32, max_size: u32) -> f32 {
    (max_size as f32 / width as f32)
        .min(max_size as f32 / height as f32)
        .min(1.0)
}

/// Applies `scale` to one axis, clamping degenerate results to a single pixel.
fn scaled_dimension(length: u32, scale: f32) -> u32 {
    (length as f32 * scale).max(1.0) as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Rgb, RgbImage};

    #[test]
    fn split_tables_rejoin_decodes_identically() -> Result<()> {
        let mut image = RgbImage::new(32, 24);
        for (x, y, pixel) in image.enumerate_pixels_mut() {
            *pixel = Rgb([(x * 7) as u8, (y * 11) as u8, ((x + y) * 5) as u8]);
        }
        let full = encode_jpeg(&image, 80)?;
        let (tables, abbreviated) = split_jpeg_tables(&full)?;
        assert!(tables.len() < full.len());
        assert!(abbreviated.len() < full.len());

        // Rejoin: SOI + table segments + everything after the SOI.
        let mut rejoined = tables.clone();
        rejoined.truncate(rejoined.len() - 2); // drop EOI
        rejoined.extend_from_slice(&abbreviated[2..]);
        let expected = decode_rgb(&full)?;
        let actual = decode_rgb(&rejoined)?;
        assert_eq!(expected.as_raw(), actual.as_raw());
        Ok(())
    }
}
