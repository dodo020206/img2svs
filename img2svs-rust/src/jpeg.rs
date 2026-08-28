use anyhow::{Context, Result};
use image::{ImageFormat, RgbImage};
use jpeg_encoder::{ColorType, Encoder, SamplingFactor};

pub fn decode_rgb(data: &[u8]) -> Result<RgbImage> {
    Ok(image::load_from_memory_with_format(data, ImageFormat::Jpeg)
        .context("decode JPEG tile")?
        .to_rgb8())
}

pub fn decode_image(data: &[u8]) -> Result<RgbImage> {
    Ok(image::load_from_memory(data)
        .context("decode embedded image")?
        .to_rgb8())
}

pub fn encode_jpeg(image: &RgbImage, quality: u8) -> Result<Vec<u8>> {
    let width = u16::try_from(image.width()).context("JPEG width exceeds 65535 pixels")?;
    let height = u16::try_from(image.height()).context("JPEG height exceeds 65535 pixels")?;
    let mut output = Vec::new();
    let mut encoder = Encoder::new(&mut output, quality);
    encoder.set_sampling_factor(SamplingFactor::F_2_2);
    encoder
        .encode(image.as_raw(), width, height, ColorType::Rgb)
        .context("encode JPEG")?;
    Ok(output)
}

pub fn white_image(width: u32, height: u32) -> RgbImage {
    RgbImage::from_pixel(width, height, image::Rgb([255, 255, 255]))
}

pub fn thumbnail(image: &RgbImage, max_size: u32) -> RgbImage {
    let (width, height) = image.dimensions();
    let scale = (max_size as f32 / width as f32)
        .min(max_size as f32 / height as f32)
        .min(1.0);
    image::imageops::resize(
        image,
        (width as f32 * scale).max(1.0) as u32,
        (height as f32 * scale).max(1.0) as u32,
        image::imageops::FilterType::Lanczos3,
    )
}
