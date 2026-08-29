use crate::hevc::Decoder as HevcDecoder;
use crate::jpeg::{
    decode_image, decode_rgb, encode_jpeg, encode_jpeg_with_capacity, thumbnail as make_thumbnail,
    transcode_jpeg_to_420, white_image,
};
use crate::model::{ByteRange, Compression, Level, Slide};
use anyhow::{anyhow, bail, Context, Result};
use image::{Rgb, RgbImage};
use memmap2::MmapOptions;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::{self, JoinHandle};

const APERIO_VERSION: &str = "Aperio Image Library v12.4.3";

pub struct WriteOptions {
    pub jpeg_quality: u8,
    pub skip_associated: bool,
    pub overwrite: bool,
}

pub fn write_slide(slide: &Slide, output: &Path, options: &WriteOptions) -> Result<()> {
    if output.exists() && !options.overwrite {
        println!(
            "Skip  : {} -> {} (already exists)",
            slide.path.display(),
            output.display()
        );
        return Ok(());
    }
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent)?;
    }
    let temporary = temporary_path(output);
    let result = write_slide_inner(slide, &temporary, options);
    match result {
        Ok(()) => {
            if output.exists() {
                fs::remove_file(output)?;
            }
            fs::rename(&temporary, output)
                .with_context(|| format!("replace output {}", output.display()))?;
            Ok(())
        }
        Err(error) => {
            let _ = fs::remove_file(&temporary);
            Err(error)
        }
    }
}

/// Append label/macro pages produced by an external decoder to a classic TIFF.
/// This is used by the OpenSlide/libvips formats after their pyramid is written.
pub fn append_associated_images(
    output: &Path,
    images: &[(String, RgbImage)],
    mpp: f64,
    quality: u8,
) -> Result<()> {
    if images.is_empty() {
        return Ok(());
    }
    let mut header = [0u8; 4];
    File::open(output)?.read_exact(&mut header)?;
    if &header[0..2] != b"II" {
        bail!("can only append associated images to little-endian TIFF");
    }
    if u16::from_le_bytes([header[2], header[3]]) == 43 {
        let mut writer = BigTiffWriter::open_append(output)?;
        for (kind, image) in images {
            writer.write_strip_page(image, quality, 10000.0 / mpp, Some(&format!("{kind}\r")))?;
        }
        return writer.finish();
    }
    let mut writer = TiffWriter::open_append(output)?;
    for (kind, image) in images {
        writer.write_strip_page(image, quality, 10000.0 / mpp, Some(&format!("{kind}\r")))?;
    }
    writer.finish()
}

/// Add the pages that SVS readers expect immediately after the full-resolution page.
///
/// libvips writes a valid pyramidal TIFF, but its CLI does not provide a way to set
/// the Aperio comment and it writes the thumbnail only when the caller supplies one.
/// Appending a new first-page IFD lets us keep libvips' streaming tile writer while
/// presenting the conventional SVS order: level 0, thumbnail, level 1, ... .
pub fn prepend_compatible_pages(
    output: &Path,
    thumbnail: &RgbImage,
    mpp: f64,
    app_mag: f64,
    quality: u8,
) -> Result<()> {
    let mut file = OpenOptions::new().read(true).write(true).open(output)?;
    let mut header = [0u8; 16];
    file.read_exact(&mut header[..8])?;
    if &header[0..2] != b"II" {
        bail!("can only repair little-endian TIFF");
    }
    let magic = u16::from_le_bytes([header[2], header[3]]);
    let source = if magic == 42 {
        SourcePage::read_classic(&mut file)?
    } else if magic == 43 {
        file.seek(SeekFrom::Start(8))?;
        file.read_exact(&mut header[8..16])?;
        SourcePage::read_bigtiff(&mut file)?
    } else {
        bail!("unsupported TIFF header while preparing SVS");
    };
    if source.tile_offsets.is_empty()
        || source.tile_offsets.len() != source.tile_byte_counts.len()
        || source.tile_width == 0
        || source.tile_height == 0
    {
        bail!("source TIFF has no valid tiled full-resolution page");
    }

    let description = format!(
        "{}\n{}x{} [0,0 {}x{}] ({}x{}) JPEG/RGB Q={}|AppMag = {}|MPP = {:.6}",
        APERIO_VERSION,
        source.width,
        source.height,
        source.width,
        source.height,
        source.tile_width,
        source.tile_height,
        quality,
        if app_mag > 0.0 {
            format!("{app_mag:.6}")
        } else {
            "0".to_owned()
        },
        mpp
    );
    let resolution = 10000.0 / mpp.max(0.000001);

    file.seek(SeekFrom::End(0))?;
    let thumb_offset = file.stream_position()?;
    let thumb_bytes = encode_jpeg(thumbnail, quality)?;
    file.write_all(&thumb_bytes)?;

    let (main_ifd, main_next_pointer) = if magic == 42 {
        write_compatible_classic_main(&mut file, &source, &description, resolution)?
    } else {
        write_compatible_big_main(&mut file, &source, &description, resolution)?
    };
    let (thumbnail_ifd, thumbnail_next_pointer) = if magic == 42 {
        write_compatible_classic_thumbnail(
            &mut file,
            thumbnail,
            thumb_offset as u32,
            u32::try_from(thumb_bytes.len()).context("thumbnail is too large")?,
            resolution,
        )?
    } else {
        write_compatible_big_thumbnail(
            &mut file,
            thumbnail,
            thumb_offset,
            thumb_bytes.len() as u64,
            resolution,
        )?
    };

    if magic == 42 {
        patch_u32_file(&mut file, main_next_pointer, u32::try_from(thumbnail_ifd)?)?;
        patch_u32_file(
            &mut file,
            thumbnail_next_pointer,
            u32::try_from(source.next_ifd)?,
        )?;
        patch_u32_file(&mut file, 4, u32::try_from(main_ifd)?)?;
    } else {
        patch_u64_file(&mut file, main_next_pointer, thumbnail_ifd)?;
        patch_u64_file(&mut file, thumbnail_next_pointer, source.next_ifd)?;
        patch_u64_file(&mut file, 8, main_ifd)?;
    }
    file.flush()?;
    Ok(())
}

struct SourcePage {
    width: u32,
    height: u32,
    tile_width: u32,
    tile_height: u32,
    bits: [u16; 3],
    compression: u16,
    photometric: u16,
    samples: u16,
    planar: u16,
    next_ifd: u64,
    tile_offsets: Vec<u64>,
    tile_byte_counts: Vec<u64>,
}

impl SourcePage {
    fn read_classic(file: &mut File) -> Result<Self> {
        file.seek(SeekFrom::Start(4))?;
        let first = read_u32(file)? as u64;
        read_source_page(file, first, false)
    }

    fn read_bigtiff(file: &mut File) -> Result<Self> {
        file.seek(SeekFrom::Start(8))?;
        let first = read_u64(file)?;
        read_source_page(file, first, true)
    }
}

#[derive(Clone, Copy)]
struct RawTag {
    kind: u16,
    count: u64,
    value: u64,
}

fn read_source_page(file: &mut File, ifd: u64, big: bool) -> Result<SourcePage> {
    file.seek(SeekFrom::Start(ifd))?;
    let count = if big {
        read_u64(file)?
    } else {
        read_u16(file)? as u64
    };
    let entry_size = if big { 20 } else { 12 };
    let mut tags = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let tag = read_u16(file)?;
        let kind = read_u16(file)?;
        let (tag_count, value) = if big {
            (read_u64(file)?, read_u64(file)?)
        } else {
            (read_u32(file)? as u64, read_u32(file)? as u64)
        };
        tags.push((
            tag,
            RawTag {
                kind,
                count: tag_count,
                value,
            },
        ));
    }
    let count_size = if big { 8 } else { 2 };
    let next_position = ifd + count_size + count * entry_size;
    file.seek(SeekFrom::Start(next_position))?;
    let next_ifd = if big {
        read_u64(file)?
    } else {
        read_u32(file)? as u64
    };
    let tag = |code| {
        tags.iter()
            .find(|(tag, _)| *tag == code)
            .map(|(_, value)| *value)
            .with_context(|| format!("source TIFF is missing tag {code}"))
    };
    let width = scalar_tag(file, tag(256)?, big)? as u32;
    let height = scalar_tag(file, tag(257)?, big)? as u32;
    let bits_values = array_tag(file, tag(258)?, big)?;
    let tile_offsets = array_tag(file, tag(324)?, big)?;
    let tile_byte_counts = array_tag(file, tag(325)?, big)?;
    Ok(SourcePage {
        width,
        height,
        tile_width: scalar_tag(file, tag(322)?, big)? as u32,
        tile_height: scalar_tag(file, tag(323)?, big)? as u32,
        bits: [
            bits_values.first().copied().unwrap_or(8) as u16,
            bits_values.get(1).copied().unwrap_or(8) as u16,
            bits_values.get(2).copied().unwrap_or(8) as u16,
        ],
        compression: scalar_tag(file, tag(259)?, big)? as u16,
        photometric: scalar_tag(file, tag(262)?, big)? as u16,
        samples: scalar_tag(file, tag(277)?, big)? as u16,
        planar: scalar_tag(file, tag(284)?, big)? as u16,
        next_ifd,
        tile_offsets,
        tile_byte_counts,
    })
}

fn scalar_tag(file: &mut File, tag: RawTag, big: bool) -> Result<u64> {
    Ok(array_tag(file, tag, big)?.first().copied().unwrap_or(0))
}

fn array_tag(file: &mut File, tag: RawTag, big: bool) -> Result<Vec<u64>> {
    let unit = match tag.kind {
        1 | 2 | 6 | 7 => 1,
        3 | 8 => 2,
        4 | 9 | 11 => 4,
        5 | 10 | 12 | 16 => 8,
        _ => bail!("unsupported TIFF tag type {}", tag.kind),
    };
    let total = tag
        .count
        .checked_mul(unit)
        .context("TIFF tag size overflow")?;
    let inline = if big { 8 } else { 4 };
    let mut bytes = vec![0u8; usize::try_from(total)?];
    if total <= inline {
        let raw = if big {
            tag.value.to_le_bytes().to_vec()
        } else {
            (tag.value as u32).to_le_bytes().to_vec()
        };
        let length = bytes.len();
        bytes.copy_from_slice(&raw[..length]);
    } else {
        file.seek(SeekFrom::Start(tag.value))?;
        file.read_exact(&mut bytes)?;
    }
    let mut values = Vec::with_capacity(tag.count as usize);
    for index in 0..tag.count as usize {
        let start = index * unit as usize;
        let value = match tag.kind {
            1 | 2 | 6 | 7 => bytes[start] as u64,
            3 => u16::from_le_bytes(bytes[start..start + 2].try_into().unwrap()) as u64,
            4 => u32::from_le_bytes(bytes[start..start + 4].try_into().unwrap()) as u64,
            16 => u64::from_le_bytes(bytes[start..start + 8].try_into().unwrap()),
            _ => 0,
        };
        values.push(value);
    }
    Ok(values)
}

fn write_compatible_classic_main(
    file: &mut File,
    source: &SourcePage,
    description: &str,
    resolution: f64,
) -> Result<(u64, u64)> {
    let ifd = align(file.stream_position()?, 2);
    pad_to(file, ifd)?;
    let count = 19u16;
    let start = ifd + 2 + count as u64 * 12 + 4;
    let bits = align(start, 2);
    let offsets = align(bits + 6, 4);
    let counts = offsets + source.tile_offsets.len() as u64 * 4;
    let desc = counts + source.tile_byte_counts.len() as u64 * 4;
    let software = desc + description.len() as u64 + 1;
    let xres = align(software + 13, 2);
    let sample_format = xres + 16;
    let mut entries = vec![
        long(254, 0),
        long(256, source.width),
        long(257, source.height),
        short_array_at(258, bits),
        short(259, source.compression),
        short(262, source.photometric),
        ascii_at(270, desc, description.len() + 1)?,
        short(274, 1),
        short(277, source.samples),
        rational_at(282, xres),
        rational_at(283, xres + 8),
        short(284, source.planar),
        short(296, 3),
        long_at(322, source.tile_width),
        long_at(323, source.tile_height),
        u64_long_array_at(324, &source.tile_offsets, offsets)?,
        u64_long_array_at(325, &source.tile_byte_counts, counts)?,
        ascii_at(305, software, 13)?,
        short_array_at(339, sample_format),
    ];
    entries.sort_by_key(|entry| entry.tag);
    file.write_all(&count.to_le_bytes())?;
    for entry in &entries {
        file.write_all(&entry.tag.to_le_bytes())?;
        file.write_all(&entry.kind.to_le_bytes())?;
        file.write_all(&entry.count.to_le_bytes())?;
        file.write_all(&entry.value.to_le_bytes())?;
    }
    let next_pointer = ifd + 2 + count as u64 * 12;
    file.write_all(&0u32.to_le_bytes())?;
    pad_to(file, bits)?;
    for value in source.bits {
        file.write_all(&value.to_le_bytes())?;
    }
    pad_to(file, offsets)?;
    for value in &source.tile_offsets {
        file.write_all(&u32::try_from(*value)?.to_le_bytes())?;
    }
    for value in &source.tile_byte_counts {
        file.write_all(&u32::try_from(*value)?.to_le_bytes())?;
    }
    file.write_all(description.as_bytes())?;
    file.write_all(&[0])?;
    file.write_all(b"img2svs-rust\0")?;
    pad_to(file, xres)?;
    write_rational(file, resolution)?;
    write_rational(file, resolution)?;
    file.write_all(&1u16.to_le_bytes())?;
    file.write_all(&1u16.to_le_bytes())?;
    file.write_all(&1u16.to_le_bytes())?;
    Ok((ifd, next_pointer))
}

fn write_compatible_classic_thumbnail(
    file: &mut File,
    image: &RgbImage,
    offset: u32,
    count: u32,
    resolution: f64,
) -> Result<(u64, u64)> {
    let ifd = align(file.stream_position()?, 2);
    pad_to(file, ifd)?;
    let entry_count = 16u16;
    let start = ifd + 2 + entry_count as u64 * 12 + 4;
    let bits = align(start, 2);
    let xres = align(bits + 6, 2);
    let reference_bw = align(xres + 16, 4);
    let entries = vec![
        long(256, image.width()),
        long(257, image.height()),
        short_array_at(258, bits),
        short(259, 7),
        short(262, 6),
        long_at(273, offset),
        long_at(278, image.height()),
        long_at(279, count),
        short(274, 1),
        short(277, 3),
        rational_at(282, xres),
        rational_at(283, xres + 8),
        short(284, 1),
        short(296, 3),
        short_pair(530, 2, 2),
        rational_array_at(532, reference_bw, 6)?,
    ];
    let mut entries = entries;
    entries.sort_by_key(|entry| entry.tag);
    file.write_all(&entry_count.to_le_bytes())?;
    for entry in &entries {
        file.write_all(&entry.tag.to_le_bytes())?;
        file.write_all(&entry.kind.to_le_bytes())?;
        file.write_all(&entry.count.to_le_bytes())?;
        file.write_all(&entry.value.to_le_bytes())?;
    }
    file.write_all(&0u32.to_le_bytes())?;
    pad_to(file, bits)?;
    file.write_all(&[8, 0, 8, 0, 8, 0])?;
    pad_to(file, xres)?;
    write_rational(file, resolution)?;
    write_rational(file, resolution)?;
    pad_to(file, reference_bw)?;
    write_reference_black_white(file)?;
    Ok((ifd, ifd + 2 + entry_count as u64 * 12))
}

fn write_compatible_big_main(
    file: &mut File,
    source: &SourcePage,
    description: &str,
    resolution: f64,
) -> Result<(u64, u64)> {
    let ifd = align(file.stream_position()?, 8);
    pad_to(file, ifd)?;
    let count = 19u64;
    let start = ifd + 8 + count * 20 + 8;
    let bits = align(start, 8);
    let offsets = align(bits + 6, 8);
    let counts = offsets + source.tile_offsets.len() as u64 * 8;
    let desc = counts + source.tile_byte_counts.len() as u64 * 8;
    let software = desc + description.len() as u64 + 1;
    let xres = align(software + 13, 8);
    let sample_format = xres + 16;
    let mut entries = vec![
        big_long(254, 0),
        big_long(256, source.width),
        big_long(257, source.height),
        big_short_array(258, bits),
        big_short(259, source.compression),
        big_short(262, source.photometric),
        big_ascii_at(270, desc, description.len() + 1)?,
        big_short(274, 1),
        big_short(277, source.samples),
        big_rational(282, xres),
        big_rational(283, xres + 8),
        big_short(284, source.planar),
        big_short(296, 3),
        big_long(322, source.tile_width),
        big_long(323, source.tile_height),
        big_long8_values_at(324, &source.tile_offsets, offsets)?,
        big_long8_values_at(325, &source.tile_byte_counts, counts)?,
        big_ascii_at(305, software, 13)?,
        big_short_array(339, sample_format),
    ];
    entries.sort_by_key(|entry| entry.tag);
    file.write_all(&count.to_le_bytes())?;
    for entry in &entries {
        file.write_all(&entry.tag.to_le_bytes())?;
        file.write_all(&entry.kind.to_le_bytes())?;
        file.write_all(&entry.count.to_le_bytes())?;
        file.write_all(&entry.value.to_le_bytes())?;
    }
    let next_pointer = ifd + 8 + count * 20;
    file.write_all(&0u64.to_le_bytes())?;
    pad_to(file, bits)?;
    for value in source.bits {
        file.write_all(&value.to_le_bytes())?;
    }
    pad_to(file, offsets)?;
    for value in &source.tile_offsets {
        file.write_all(&value.to_le_bytes())?;
    }
    for value in &source.tile_byte_counts {
        file.write_all(&value.to_le_bytes())?;
    }
    file.write_all(description.as_bytes())?;
    file.write_all(&[0])?;
    file.write_all(b"img2svs-rust\0")?;
    pad_to(file, xres)?;
    write_rational(file, resolution)?;
    write_rational(file, resolution)?;
    file.write_all(&1u16.to_le_bytes())?;
    file.write_all(&1u16.to_le_bytes())?;
    file.write_all(&1u16.to_le_bytes())?;
    Ok((ifd, next_pointer))
}

fn write_compatible_big_thumbnail(
    file: &mut File,
    image: &RgbImage,
    offset: u64,
    count: u64,
    resolution: f64,
) -> Result<(u64, u64)> {
    let ifd = align(file.stream_position()?, 8);
    pad_to(file, ifd)?;
    let entry_count = 16u64;
    let start = ifd + 8 + entry_count * 20 + 8;
    let bits = align(start, 8);
    let xres = align(bits + 6, 8);
    let reference_bw = align(xres + 16, 8);
    let mut entries = vec![
        big_long(256, image.width()),
        big_long(257, image.height()),
        big_short_array(258, bits),
        big_short(259, 7),
        big_short(262, 6),
        BigEntry {
            tag: 273,
            kind: 16,
            count: 1,
            value: offset,
        },
        big_long(278, image.height()),
        BigEntry {
            tag: 279,
            kind: 16,
            count: 1,
            value: count,
        },
        big_short(274, 1),
        big_short(277, 3),
        big_rational(282, xres),
        big_rational(283, xres + 8),
        big_short(284, 1),
        big_short(296, 3),
        big_short_pair(530, 2, 2),
        big_rational_array(532, reference_bw, 6),
    ];
    entries.sort_by_key(|entry| entry.tag);
    file.write_all(&entry_count.to_le_bytes())?;
    for entry in &entries {
        file.write_all(&entry.tag.to_le_bytes())?;
        file.write_all(&entry.kind.to_le_bytes())?;
        file.write_all(&entry.count.to_le_bytes())?;
        file.write_all(&entry.value.to_le_bytes())?;
    }
    file.write_all(&0u64.to_le_bytes())?;
    pad_to(file, bits)?;
    file.write_all(&[8, 0, 8, 0, 8, 0])?;
    pad_to(file, xres)?;
    write_rational(file, resolution)?;
    write_rational(file, resolution)?;
    pad_to(file, reference_bw)?;
    write_reference_black_white(file)?;
    Ok((ifd, ifd + 8 + entry_count * 20))
}

fn big_ascii_at(tag: u16, offset: u64, count: usize) -> Result<BigEntry> {
    Ok(BigEntry {
        tag,
        kind: 2,
        count: u64::try_from(count)?,
        value: offset,
    })
}

fn big_long8_array(tag: u16, count: usize, offset: u64) -> Result<BigEntry> {
    Ok(BigEntry {
        tag,
        kind: 16,
        count: u64::try_from(count)?,
        value: offset,
    })
}

fn big_long8_values_at(tag: u16, values: &[u64], offset: u64) -> Result<BigEntry> {
    if values.len() == 1 {
        Ok(BigEntry {
            tag,
            kind: 16,
            count: 1,
            value: values[0],
        })
    } else {
        big_long8_array(tag, values.len(), offset)
    }
}

fn read_u16(file: &mut File) -> Result<u16> {
    let mut bytes = [0u8; 2];
    file.read_exact(&mut bytes)?;
    Ok(u16::from_le_bytes(bytes))
}

fn read_u32(file: &mut File) -> Result<u32> {
    let mut bytes = [0u8; 4];
    file.read_exact(&mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}

fn read_u64(file: &mut File) -> Result<u64> {
    let mut bytes = [0u8; 8];
    file.read_exact(&mut bytes)?;
    Ok(u64::from_le_bytes(bytes))
}

fn patch_u32_file(file: &mut File, offset: u64, value: u32) -> Result<()> {
    let current = file.stream_position()?;
    file.seek(SeekFrom::Start(offset))?;
    file.write_all(&value.to_le_bytes())?;
    file.seek(SeekFrom::Start(current))?;
    Ok(())
}

fn patch_u64_file(file: &mut File, offset: u64, value: u64) -> Result<()> {
    let current = file.stream_position()?;
    file.seek(SeekFrom::Start(offset))?;
    file.write_all(&value.to_le_bytes())?;
    file.seek(SeekFrom::Start(current))?;
    Ok(())
}

fn temporary_path(output: &Path) -> PathBuf {
    let mut path = output.to_path_buf();
    let suffix = format!(".{}.tmp", std::process::id());
    path.set_file_name(format!(
        "{}{}",
        output.file_name().unwrap().to_string_lossy(),
        suffix
    ));
    path
}

fn write_slide_inner(slide: &Slide, output: &Path, options: &WriteOptions) -> Result<()> {
    let mut input =
        File::open(&slide.path).with_context(|| format!("open input {}", slide.path.display()))?;
    let mut writer = TiffWriter::create(output)?;
    let mut hevc = if slide.compression == Compression::Hevc {
        Some(HevcDecoder::new()?)
    } else {
        None
    };

    let thumbnail = render_thumbnail(slide, &mut input, options.jpeg_quality, hevc.as_mut())?;
    let tile_pool = TilePool::new(slide)?;
    writer.write_tiled_page(
        slide,
        &slide.levels[0],
        options.jpeg_quality,
        false,
        &tile_pool,
    )?;
    writer.write_strip_page(
        &thumbnail,
        options.jpeg_quality,
        10000.0 / slide.metadata.mpp,
        None,
    )?;
    for level in slide.levels.iter().skip(1) {
        writer.write_tiled_page(slide, level, options.jpeg_quality, true, &tile_pool)?;
    }

    if !options.skip_associated {
        for associated in &slide.associated_images {
            let bytes = read_range(&mut input, associated.data.offset, associated.data.length)?;
            if bytes.is_empty() {
                continue;
            }
            let image = decode_image(&bytes)
                .with_context(|| format!("decode associated image {}", associated.kind))?;
            writer.write_strip_page(
                &image,
                options.jpeg_quality,
                10000.0 / slide.metadata.mpp,
                Some(&format!("{}\r", associated.kind)),
            )?;
        }
    }
    writer.finish()
}

fn render_thumbnail(
    slide: &Slide,
    input: &mut File,
    quality: u8,
    hevc: Option<&mut HevcDecoder>,
) -> Result<RgbImage> {
    if let Some(thumbnail) = &slide.thumbnail {
        let _declared_thumbnail_size = (thumbnail.width, thumbnail.height);
        let bytes = read_range(input, thumbnail.data.offset, thumbnail.data.length)?;
        if !bytes.is_empty() {
            if let Ok(image) = decode_image(&bytes) {
                return Ok(make_thumbnail(&image, 1024));
            }
        }
    }
    let level = slide.levels.last().context("slide has no pyramid levels")?;
    let image = render_level(slide, input, level, quality, hevc)?;
    Ok(make_thumbnail(&image, 1024))
}

fn render_level(
    slide: &Slide,
    input: &mut File,
    level: &Level,
    _quality: u8,
    mut hevc: Option<&mut HevcDecoder>,
) -> Result<RgbImage> {
    let mut canvas = white_image(level.width, level.height);
    for (index, range) in level.tiles.iter().enumerate() {
        if !range.present() {
            continue;
        }
        let bytes = read_range(input, range.offset, range.length)?;
        let tile = decode_tile(slide, &bytes, hevc.as_deref_mut())?;
        if let Some(position) = level.tile_positions.get(index) {
            copy_region(
                &mut canvas,
                &tile,
                position.x,
                position.y,
                position.width,
                position.height,
            );
        } else {
            let row = index as u32 / level.tile_cols;
            let col = index as u32 % level.tile_cols;
            copy_clipped(
                &mut canvas,
                &tile,
                col * slide.tile_width,
                row * slide.tile_height,
            );
        }
    }
    Ok(canvas)
}

fn decode_tile(slide: &Slide, bytes: &[u8], hevc: Option<&mut HevcDecoder>) -> Result<RgbImage> {
    match slide.compression {
        Compression::Jpeg => decode_rgb(bytes),
        Compression::Hevc => hevc.context("HEVC decoder was not initialized")?.decode(
            bytes,
            slide.tile_width,
            slide.tile_height,
        ),
    }
}

fn copy_clipped(dst: &mut RgbImage, src: &RgbImage, left: u32, top: u32) {
    if left >= dst.width() || top >= dst.height() {
        return;
    }
    let width = src.width().min(dst.width() - left);
    let height = src.height().min(dst.height() - top);
    for y in 0..height {
        for x in 0..width {
            dst.put_pixel(left + x, top + y, *src.get_pixel(x, y));
        }
    }
}

fn copy_region(dst: &mut RgbImage, src: &RgbImage, left: u32, top: u32, width: u32, height: u32) {
    if left >= dst.width() || top >= dst.height() {
        return;
    }
    let width = width.min(src.width()).min(dst.width() - left);
    let height = height.min(src.height()).min(dst.height() - top);
    for y in 0..height {
        for x in 0..width {
            dst.put_pixel(left + x, top + y, *src.get_pixel(x, y));
        }
    }
}

fn read_range(input: &mut File, offset: u64, length: u64) -> Result<Vec<u8>> {
    if length == 0 {
        return Ok(Vec::new());
    }
    let size = usize::try_from(length).context("tile is too large for this platform")?;
    input.seek(SeekFrom::Start(offset))?;
    let mut bytes = vec![0; size];
    std::io::Read::read_exact(input, &mut bytes)?;
    Ok(bytes)
}

fn mapped_range(source: &[u8], range: ByteRange) -> Result<&[u8]> {
    let start = usize::try_from(range.offset).context("tile offset exceeds this platform")?;
    let length = usize::try_from(range.length).context("tile length exceeds this platform")?;
    let end = start.checked_add(length).context("tile range overflow")?;
    source
        .get(start..end)
        .context("tile range exceeds input file")
}

struct TiffWriter {
    file: File,
    first_ifd_pointer: Option<u64>,
    previous_next_pointer: Option<u64>,
}

impl TiffWriter {
    fn create(path: &Path) -> Result<Self> {
        let mut file = OpenOptions::new().create_new(true).write(true).open(path)?;
        file.write_all(b"II")?;
        file.write_all(&42u16.to_le_bytes())?;
        file.write_all(&0u32.to_le_bytes())?;
        Ok(Self {
            file,
            first_ifd_pointer: None,
            previous_next_pointer: None,
        })
    }

    fn open_append(path: &Path) -> Result<Self> {
        let mut file = OpenOptions::new().read(true).write(true).open(path)?;
        let mut header = [0u8; 8];
        file.read_exact(&mut header)?;
        if &header[0..2] != b"II" || u16::from_le_bytes([header[2], header[3]]) != 42 {
            bail!("can only append associated images to classic little-endian TIFF");
        }
        let first_ifd = u32::from_le_bytes(header[4..8].try_into().unwrap()) as u64;
        let mut ifd = first_ifd;
        if ifd == 0 {
            bail!("TIFF has no image directory");
        }
        let next_pointer = loop {
            file.seek(SeekFrom::Start(ifd))?;
            let mut count = [0u8; 2];
            file.read_exact(&mut count)?;
            let count = u16::from_le_bytes(count) as u64;
            let pointer = ifd + 2 + count * 12;
            file.seek(SeekFrom::Start(pointer))?;
            let mut next = [0u8; 4];
            file.read_exact(&mut next)?;
            let next = u32::from_le_bytes(next) as u64;
            if next == 0 {
                break pointer;
            }
            if next <= ifd {
                bail!("invalid TIFF IFD chain");
            }
            ifd = next;
        };
        file.seek(SeekFrom::End(0))?;
        Ok(Self {
            file,
            first_ifd_pointer: Some(first_ifd),
            previous_next_pointer: Some(next_pointer),
        })
    }

    fn begin_ifd(&mut self) -> Result<u64> {
        let offset = self.file.stream_position()?;
        if let Some(pointer) = self.previous_next_pointer {
            self.patch_u32(
                pointer,
                u32::try_from(offset).context("TIFF exceeds classic 4 GiB offsets")?,
            )?;
        } else {
            self.patch_u32(
                4,
                u32::try_from(offset).context("TIFF exceeds classic 4 GiB offsets")?,
            )?;
            self.first_ifd_pointer = Some(offset);
        }
        Ok(offset)
    }

    fn finish(&mut self) -> Result<()> {
        self.file.flush()?;
        Ok(())
    }

    fn write_tiled_page(
        &mut self,
        slide: &Slide,
        level: &Level,
        quality: u8,
        reduced: bool,
        tile_pool: &TilePool,
    ) -> Result<()> {
        let merge_cols = 16 / gcd(slide.tile_width, 16);
        let merge_rows = 16 / gcd(slide.tile_height, 16);
        let tile_width = slide.tile_width * merge_cols;
        let tile_height = slide.tile_height * merge_rows;
        let output_cols = (level.tile_cols + merge_cols - 1) / merge_cols;
        let output_rows = (level.tile_rows + merge_rows - 1) / merge_rows;
        let mut offsets = Vec::with_capacity((output_cols * output_rows) as usize);
        let mut counts = Vec::with_capacity(offsets.capacity());

        let total = usize::try_from(output_cols as u64 * output_rows as u64)
            .context("output tile count exceeds this platform")?;
        for batch_start in (0..total).step_by(tile_pool.batch_size()) {
            let batch_end = (batch_start + tile_pool.batch_size()).min(total);
            let tasks: Vec<_> = (batch_start..batch_end)
                .map(|index| TileTask {
                    slot: index - batch_start,
                    level_index: level.index,
                    output_row: index as u32 / output_cols,
                    output_col: index as u32 % output_cols,
                    merge_rows,
                    merge_cols,
                    output_width: tile_width,
                    output_height: tile_height,
                    quality,
                })
                .collect();
            let encoded_tiles = tile_pool.encode_batch(&tasks)?;
            let batch_offset = self.file.stream_position()?;
            let batch_bytes = encoded_tiles.iter().map(Vec::len).sum();
            let mut batch = Vec::with_capacity(batch_bytes);
            for encoded in encoded_tiles {
                let offset = batch_offset + batch.len() as u64;
                offsets.push(u32::try_from(offset).context("TIFF exceeds classic 4 GiB offsets")?);
                counts.push(u32::try_from(encoded.len()).context("JPEG tile is too large")?);
                batch.extend_from_slice(&encoded);
            }
            self.file.write_all(&batch)?;
        }
        let description = aperio_description(slide, level, tile_width, tile_height, quality);
        let resolution =
            10000.0 / slide.metadata.mpp / if reduced { level.downsample } else { 1.0 };
        self.write_ifd(Page::Tiled {
            width: level.width,
            height: level.height,
            tile_width,
            tile_height,
            offsets,
            counts,
            description,
            reduced,
            resolution,
        })
    }

    fn write_strip_page(
        &mut self,
        image: &RgbImage,
        quality: u8,
        resolution: f64,
        description: Option<&str>,
    ) -> Result<()> {
        let encoded = encode_jpeg(image, quality)?;
        let offset = self.file.stream_position()?;
        self.file.write_all(&encoded)?;
        self.write_ifd(Page::Strip {
            width: image.width(),
            height: image.height(),
            offset: u32::try_from(offset).context("TIFF exceeds classic 4 GiB offsets")?,
            count: u32::try_from(encoded.len()).context("JPEG image is too large")?,
            resolution,
            description: description.map(str::to_owned),
        })
    }

    fn write_ifd(&mut self, page: Page) -> Result<()> {
        let ifd_offset = self.begin_ifd()?;
        let entry_count = page.entry_count();
        let extras_offset = ifd_offset + 2 + entry_count as u64 * 12 + 4;
        let entries = page.entries(extras_offset)?;
        self.file.write_all(&(entry_count as u16).to_le_bytes())?;
        for entry in &entries {
            self.file.write_all(&entry.tag.to_le_bytes())?;
            self.file.write_all(&entry.kind.to_le_bytes())?;
            self.file.write_all(&entry.count.to_le_bytes())?;
            self.file.write_all(&entry.value.to_le_bytes())?;
        }
        self.file.write_all(&0u32.to_le_bytes())?;
        page.write_extra(&mut self.file)?;
        self.previous_next_pointer = Some(ifd_offset + 2 + entry_count as u64 * 12);
        Ok(())
    }

    fn patch_u32(&mut self, offset: u64, value: u32) -> Result<()> {
        let current = self.file.stream_position()?;
        self.file.seek(SeekFrom::Start(offset))?;
        self.file.write_all(&value.to_le_bytes())?;
        self.file.seek(SeekFrom::Start(current))?;
        Ok(())
    }
}

#[derive(Clone, Copy)]
struct TileTask {
    slot: usize,
    level_index: usize,
    output_row: u32,
    output_col: u32,
    merge_rows: u32,
    merge_cols: u32,
    output_width: u32,
    output_height: u32,
    quality: u8,
}

struct TileResult {
    slot: usize,
    encoded: Result<Vec<u8>>,
}

struct TilePool {
    tasks: Option<mpsc::Sender<TileTask>>,
    results: mpsc::Receiver<TileResult>,
    workers: Vec<JoinHandle<()>>,
    batch_size: usize,
}

impl TilePool {
    fn new(slide: &Slide) -> Result<Self> {
        let available = thread::available_parallelism()
            .map(|count| count.get())
            .unwrap_or(1);
        let worker_limit = if slide.compression == Compression::Jpeg {
            64
        } else {
            32
        };
        let default_workers = if slide.compression == Compression::Jpeg {
            available.min(worker_limit)
        } else {
            available.saturating_sub(1).max(1).min(worker_limit)
        };
        let worker_count = std::env::var("IMG2SVS_THREADS")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .filter(|count| *count > 0)
            .unwrap_or(default_workers)
            .min(worker_limit);
        Self::with_worker_count(slide, worker_count)
    }

    fn with_worker_count(slide: &Slide, worker_count: usize) -> Result<Self> {
        let worker_count = worker_count.clamp(1, 64);
        let (result_sender, result_receiver) = mpsc::channel::<TileResult>();
        let (task_sender, task_receiver) = mpsc::channel::<TileTask>();
        let task_receiver = Arc::new(Mutex::new(task_receiver));
        let slide = Arc::new(slide.clone());
        let source_file = File::open(&slide.path)
            .with_context(|| format!("open input {}", slide.path.display()))?;
        // SAFETY: the converter opens the source read-only and never mutates it
        // while this mapping is alive.
        let source = Arc::new(unsafe { MmapOptions::new().map(&source_file)? });
        let mut workers = Vec::with_capacity(worker_count);

        for index in 0..worker_count {
            let worker_slide = Arc::clone(&slide);
            let worker_source = Arc::clone(&source);
            let worker_tasks = Arc::clone(&task_receiver);
            let worker_results = result_sender.clone();
            workers.push(
                thread::Builder::new()
                    .name(format!("svs-tile-{index}"))
                    .spawn(move || {
                        tile_worker(worker_slide, worker_source, worker_tasks, worker_results)
                    })
                    .context("start tile worker")?,
            );
        }
        drop(result_sender);
        Ok(Self {
            tasks: Some(task_sender),
            results: result_receiver,
            workers,
            batch_size: worker_count * 64,
        })
    }

    fn batch_size(&self) -> usize {
        self.batch_size
    }

    fn encode_batch(&self, tasks: &[TileTask]) -> Result<Vec<Vec<u8>>> {
        let sender = self.tasks.as_ref().context("tile workers are closed")?;
        for task in tasks {
            sender.send(*task).context("send tile task")?;
        }
        let mut ordered: Vec<Option<Vec<u8>>> = (0..tasks.len()).map(|_| None).collect();
        let mut first_error = None;
        for _ in tasks {
            let result = self.results.recv().context("receive encoded tile")?;
            match result.encoded {
                Ok(encoded) => ordered[result.slot] = Some(encoded),
                Err(error) => {
                    first_error.get_or_insert(error);
                }
            }
        }
        if let Some(error) = first_error {
            return Err(error);
        }
        ordered
            .into_iter()
            .map(|encoded| encoded.context("tile worker returned no data"))
            .collect()
    }
}

impl Drop for TilePool {
    fn drop(&mut self) {
        self.tasks.take();
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}

fn tile_worker(
    slide: Arc<Slide>,
    source: Arc<memmap2::Mmap>,
    tasks: Arc<Mutex<mpsc::Receiver<TileTask>>>,
    results: mpsc::Sender<TileResult>,
) {
    let (mut hevc, hevc_error) = if slide.compression == Compression::Hevc {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(HevcDecoder::new)) {
            Ok(Ok(decoder)) => (Some(decoder), None),
            Ok(Err(error)) => (None, Some(format!("initialize HEVC decoder: {error:#}"))),
            Err(_) => (
                None,
                Some("initialize HEVC decoder: worker panicked".to_owned()),
            ),
        }
    } else {
        (None, None)
    };
    loop {
        let task = match tasks.lock() {
            Ok(receiver) => match receiver.recv() {
                Ok(task) => task,
                Err(_) => return,
            },
            Err(_) => return,
        };
        let encoded = if let Some(message) = &hevc_error {
            Err(anyhow!(message.clone()))
        } else {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                encode_output_tile(&slide, &source, task, hevc.as_mut())
            }))
            .unwrap_or_else(|_| Err(anyhow!("tile worker panicked")))
        };
        if results
            .send(TileResult {
                slot: task.slot,
                encoded,
            })
            .is_err()
        {
            return;
        }
    }
}

fn encode_output_tile(
    slide: &Slide,
    source: &[u8],
    task: TileTask,
    hevc: Option<&mut HevcDecoder>,
) -> Result<Vec<u8>> {
    let level = slide
        .levels
        .get(task.level_index)
        .context("invalid pyramid level")?;
    let passthrough = task.merge_rows == 1
        && task.merge_cols == 1
        && slide.compression == Compression::Jpeg
        && level.tile_positions.is_empty()
        && task.quality == slide.metadata.jpeg_quality
        && task.output_row + 1 < level.tile_rows
        && task.output_col + 1 < level.tile_cols;
    if passthrough {
        let range = *level
            .tiles
            .get((task.output_row * level.tile_cols + task.output_col) as usize)
            .context("invalid source tile index")?;
        let bytes = mapped_range(source, range)?;
        if bytes.is_empty() {
            return encode_jpeg(
                &compose_tile(
                    slide,
                    source,
                    level,
                    task.output_row,
                    task.output_col,
                    task.merge_rows,
                    task.merge_cols,
                    task.output_width,
                    task.output_height,
                    hevc,
                )?,
                task.quality,
            );
        }
        if !jpeg_is_420(bytes) {
            return transcode_jpeg_to_420(bytes, task.quality).or_else(|_| {
                encode_jpeg_with_capacity(&decode_rgb(bytes)?, task.quality, bytes.len())
            });
        }
        return Ok(bytes.to_vec());
    }
    let image = compose_tile(
        slide,
        source,
        level,
        task.output_row,
        task.output_col,
        task.merge_rows,
        task.merge_cols,
        task.output_width,
        task.output_height,
        hevc,
    )?;
    encode_jpeg(&image, task.quality)
}

struct BigTiffWriter {
    file: File,
    previous_next_pointer: u64,
}

impl BigTiffWriter {
    fn open_append(path: &Path) -> Result<Self> {
        let mut file = OpenOptions::new().read(true).write(true).open(path)?;
        let mut header = [0u8; 16];
        file.read_exact(&mut header)?;
        if &header[0..2] != b"II"
            || u16::from_le_bytes([header[2], header[3]]) != 43
            || u16::from_le_bytes([header[4], header[5]]) != 8
            || u16::from_le_bytes([header[6], header[7]]) != 0
        {
            bail!("unsupported BigTIFF header");
        }
        let file_size = file.metadata()?.len();
        let first_ifd = u64::from_le_bytes(header[8..16].try_into().unwrap());
        if first_ifd == 0 || first_ifd >= file_size {
            bail!("BigTIFF has no valid image directory");
        }
        let mut ifd = first_ifd;
        let next_pointer = loop {
            if ifd > file_size.saturating_sub(16) {
                bail!("invalid BigTIFF IFD chain");
            }
            file.seek(SeekFrom::Start(ifd))?;
            let mut count = [0u8; 8];
            file.read_exact(&mut count)?;
            let count = u64::from_le_bytes(count);
            let pointer = ifd
                .checked_add(8)
                .and_then(|value| value.checked_add(count.checked_mul(20)?))
                .context("BigTIFF IFD is too large")?;
            if pointer > file_size.saturating_sub(8) {
                bail!("invalid BigTIFF IFD bounds");
            }
            file.seek(SeekFrom::Start(pointer))?;
            let mut next = [0u8; 8];
            file.read_exact(&mut next)?;
            let next = u64::from_le_bytes(next);
            if next == 0 {
                break pointer;
            }
            if next <= ifd || next >= file_size {
                bail!("invalid BigTIFF IFD chain");
            }
            ifd = next;
        };
        file.seek(SeekFrom::End(0))?;
        Ok(Self {
            file,
            previous_next_pointer: next_pointer,
        })
    }

    fn write_strip_page(
        &mut self,
        image: &RgbImage,
        quality: u8,
        resolution: f64,
        description: Option<&str>,
    ) -> Result<()> {
        let encoded = encode_jpeg(image, quality)?;
        let offset = self.file.stream_position()?;
        self.file.write_all(&encoded)?;
        self.write_ifd(BigPage::Strip {
            width: image.width(),
            height: image.height(),
            offset,
            count: encoded.len() as u64,
            resolution,
            description: description.map(str::to_owned),
        })
    }

    fn write_ifd(&mut self, page: BigPage) -> Result<()> {
        let ifd_offset = self.file.stream_position()?;
        self.patch_u64(self.previous_next_pointer, ifd_offset)?;
        let entry_count = page.entry_count();
        let extras_offset = ifd_offset + 8 + entry_count as u64 * 20 + 8;
        let extra = page.extra_data(extras_offset)?;
        let entries = page.entries(&extra)?;
        self.file.write_all(&(entries.len() as u64).to_le_bytes())?;
        for entry in &entries {
            self.file.write_all(&entry.tag.to_le_bytes())?;
            self.file.write_all(&entry.kind.to_le_bytes())?;
            self.file.write_all(&entry.count.to_le_bytes())?;
            self.file.write_all(&entry.value.to_le_bytes())?;
        }
        self.file.write_all(&0u64.to_le_bytes())?;
        page.write_extra(&mut self.file, &extra)?;
        self.previous_next_pointer = ifd_offset + 8 + entries.len() as u64 * 20;
        Ok(())
    }

    fn patch_u64(&mut self, offset: u64, value: u64) -> Result<()> {
        let current = self.file.stream_position()?;
        self.file.seek(SeekFrom::Start(offset))?;
        self.file.write_all(&value.to_le_bytes())?;
        self.file.seek(SeekFrom::Start(current))?;
        Ok(())
    }

    fn finish(&mut self) -> Result<()> {
        self.file.flush()?;
        Ok(())
    }
}

enum BigPage {
    Strip {
        width: u32,
        height: u32,
        offset: u64,
        count: u64,
        resolution: f64,
        description: Option<String>,
    },
}

#[derive(Clone, Copy)]
struct BigEntry {
    tag: u16,
    kind: u16,
    count: u64,
    value: u64,
}

struct BigExtra {
    bits: u64,
    desc: u64,
    xres: u64,
    yres: u64,
    sample: u64,
    reference_bw: u64,
    desc_text: String,
}

impl BigPage {
    fn entry_count(&self) -> usize {
        match self {
            Self::Strip { description, .. } => {
                if description.is_some() {
                    18
                } else {
                    17
                }
            }
        }
    }

    fn extra_data(&self, start: u64) -> Result<BigExtra> {
        match self {
            Self::Strip { description, .. } => {
                let bits = align(start, 2);
                let desc = if description.is_some() { bits + 6 } else { 0 };
                let after_desc = bits + 6 + description.as_ref().map_or(0, |v| v.len() + 1) as u64;
                let xres = align(after_desc, 2);
                Ok(BigExtra {
                    bits,
                    desc,
                    xres,
                    yres: xres + 8,
                    sample: xres + 16,
                    reference_bw: align(xres + 22, 8),
                    desc_text: description.clone().unwrap_or_default(),
                })
            }
        }
    }

    fn entries(&self, extra: &BigExtra) -> Result<Vec<BigEntry>> {
        match self {
            Self::Strip {
                width,
                height,
                offset,
                count,
                description,
                ..
            } => {
                let mut entries = vec![
                    big_long(256, *width),
                    big_long(257, *height),
                    big_short_array(258, extra.bits),
                    big_short(259, 7),
                    big_short(262, 6),
                    big_short(274, 1),
                    big_short(277, 3),
                    big_short(284, 1),
                    big_long(278, *height),
                    BigEntry {
                        tag: 273,
                        kind: 16,
                        count: 1,
                        value: *offset,
                    },
                    BigEntry {
                        tag: 279,
                        kind: 16,
                        count: 1,
                        value: *count,
                    },
                    big_short(296, 3),
                    big_rational(282, extra.xres),
                    big_rational(283, extra.yres),
                    big_short_array(339, extra.sample),
                    big_short_pair(530, 2, 2),
                    big_rational_array(532, extra.reference_bw, 6),
                ];
                if description.is_some() {
                    let value = if extra.desc_text.len() < 8 {
                        big_ascii_inline(&extra.desc_text)
                    } else {
                        extra.desc
                    };
                    entries.push(BigEntry {
                        tag: 270,
                        kind: 2,
                        count: extra.desc_text.len() as u64 + 1,
                        value,
                    });
                }
                entries.sort_by_key(|entry| entry.tag);
                Ok(entries)
            }
        }
    }

    fn write_extra(&self, file: &mut File, extra: &BigExtra) -> Result<()> {
        match self {
            Self::Strip {
                resolution,
                description,
                ..
            } => {
                pad_to(file, extra.bits)?;
                file.write_all(&[8, 0, 8, 0, 8, 0])?;
                if let Some(text) = description {
                    file.write_all(text.as_bytes())?;
                    file.write_all(&[0])?;
                }
                pad_to(file, extra.xres)?;
                write_rational(file, *resolution)?;
                write_rational(file, *resolution)?;
                file.write_all(&1u16.to_le_bytes())?;
                file.write_all(&1u16.to_le_bytes())?;
                file.write_all(&1u16.to_le_bytes())?;
                let reference_bw = align(file.stream_position()?, 4);
                pad_to(file, reference_bw)?;
                write_reference_black_white(file)?;
                Ok(())
            }
        }
    }
}

fn big_short(tag: u16, value: u16) -> BigEntry {
    BigEntry {
        tag,
        kind: 3,
        count: 1,
        value: value as u64,
    }
}

fn big_short_pair(tag: u16, first: u16, second: u16) -> BigEntry {
    BigEntry {
        tag,
        kind: 3,
        count: 2,
        value: first as u64 | ((second as u64) << 16),
    }
}

fn big_short_array(tag: u16, offset: u64) -> BigEntry {
    BigEntry {
        tag,
        kind: 3,
        count: 3,
        value: offset,
    }
}

fn big_long(tag: u16, value: u32) -> BigEntry {
    BigEntry {
        tag,
        kind: 4,
        count: 1,
        value: value as u64,
    }
}

fn big_rational(tag: u16, offset: u64) -> BigEntry {
    BigEntry {
        tag,
        kind: 5,
        count: 1,
        value: offset,
    }
}

fn big_rational_array(tag: u16, offset: u64, count: u64) -> BigEntry {
    BigEntry {
        tag,
        kind: 5,
        count,
        value: offset,
    }
}

fn big_ascii_inline(value: &str) -> u64 {
    let mut bytes = [0u8; 8];
    let source = value.as_bytes();
    let length = source.len().min(7);
    bytes[..length].copy_from_slice(&source[..length]);
    u64::from_le_bytes(bytes)
}

enum Page {
    Tiled {
        width: u32,
        height: u32,
        tile_width: u32,
        tile_height: u32,
        offsets: Vec<u32>,
        counts: Vec<u32>,
        description: String,
        reduced: bool,
        resolution: f64,
    },
    Strip {
        width: u32,
        height: u32,
        offset: u32,
        count: u32,
        resolution: f64,
        description: Option<String>,
    },
}

#[derive(Clone, Copy)]
struct Entry {
    tag: u16,
    kind: u16,
    count: u32,
    value: u32,
}

impl Page {
    fn entry_count(&self) -> usize {
        match self {
            Page::Tiled { .. } => 20,
            Page::Strip { description, .. } => {
                if description.is_some() {
                    18
                } else {
                    17
                }
            }
        }
    }

    fn extra_data(&self, ifd_offset: u64) -> Result<Extra> {
        let start = ifd_offset + 2 + self.entry_count() as u64 * 12 + 4;
        match self {
            Page::Tiled {
                offsets,
                counts,
                description,
                ..
            } => {
                let bits = align(start, 2);
                let tile_offsets = align(bits + 6, 4);
                let tile_counts = tile_offsets + offsets.len() as u64 * 4;
                let desc = tile_counts + counts.len() as u64 * 4;
                let xres = align(desc + description.len() as u64 + 1, 2);
                let yres = xres + 8;
                let sample = yres + 8;
                let reference_bw = align(sample + 6, 4);
                Ok(Extra {
                    bits,
                    tile_offsets,
                    tile_counts,
                    desc,
                    xres,
                    yres,
                    sample,
                    reference_bw,
                    desc_text: description.clone(),
                })
            }
            Page::Strip { description, .. } => {
                let bits = align(start, 2);
                let desc = if description.is_some() { bits + 6 } else { 0 };
                let xres = align(
                    bits + 6 + description.as_ref().map_or(0, |v| v.len() + 1) as u64,
                    2,
                );
                let yres = xres + 8;
                let sample = yres + 8;
                let reference_bw = align(sample + 6, 4);
                Ok(Extra {
                    bits,
                    tile_offsets: 0,
                    tile_counts: 0,
                    desc,
                    xres,
                    yres,
                    sample,
                    reference_bw,
                    desc_text: description.clone().unwrap_or_default(),
                })
            }
        }
    }

    fn entries(&self, extra_offset: u64) -> Result<Vec<Entry>> {
        let extra = self.extra_data(extra_offset - 2 - self.entry_count() as u64 * 12 - 4)?;
        let e = match self {
            Page::Tiled {
                width,
                height,
                tile_width,
                tile_height,
                offsets,
                counts,
                reduced,
                ..
            } => vec![
                long(254, if *reduced { 1 } else { 0 }),
                long(256, *width),
                long(257, *height),
                short_array_at(258, extra.bits),
                short(259, 7),
                short(262, 6),
                short(274, 1),
                short(277, 3),
                short(284, 1),
                short(296, 3),
                long_at(322, *tile_width),
                long_at(323, *tile_height),
                long_array_at(324, offsets, extra.tile_offsets)?,
                long_array_at(325, counts, extra.tile_counts)?,
                rational_at(282, extra.xres),
                rational_at(283, extra.yres),
                short_array_at(339, extra.sample),
                ascii_at(270, extra.desc, extra.desc_text.len() + 1)?,
                short_pair(530, 2, 2),
                rational_array_at(532, extra.reference_bw, 6)?,
            ],
            Page::Strip {
                width,
                height,
                offset,
                count,
                description,
                ..
            } => {
                let mut v = vec![
                    long(256, *width),
                    long(257, *height),
                    short_array_at(258, extra.bits),
                    short(259, 7),
                    short(262, 6),
                    short(274, 1),
                    short(277, 3),
                    short(284, 1),
                    long_at(278, *height),
                    long_at(273, *offset),
                    long_at(279, *count),
                    short(296, 3),
                    rational_at(282, extra.xres),
                    rational_at(283, extra.yres),
                    short_array_at(339, extra.sample),
                    short_pair(530, 2, 2),
                    rational_array_at(532, extra.reference_bw, 6)?,
                ];
                if description.is_some() {
                    v.push(ascii_at(270, extra.desc, extra.desc_text.len() + 1)?);
                }
                v
            }
        };
        let mut e = e;
        e.sort_by_key(|entry| entry.tag);
        Ok(e)
    }

    fn write_extra(&self, file: &mut File) -> Result<()> {
        let ifd = file.stream_position()?;
        // The actual offsets are recomputed from the current IFD layout.
        let start = ifd;
        let bits = align(start, 2);
        pad_to(file, bits)?;
        file.write_all(&[8, 0, 8, 0, 8, 0])?;
        match self {
            Page::Tiled {
                offsets,
                counts,
                description,
                ..
            } => {
                let tile_offsets = align(bits + 6, 4);
                pad_to(file, tile_offsets)?;
                for value in offsets {
                    file.write_all(&value.to_le_bytes())?;
                }
                for value in counts {
                    file.write_all(&value.to_le_bytes())?;
                }
                file.write_all(description.as_bytes())?;
                file.write_all(&[0])?;
                let after_desc = tile_offsets
                    + offsets.len() as u64 * 4
                    + counts.len() as u64 * 4
                    + description.len() as u64
                    + 1;
                pad_to(file, align(after_desc, 2))?;
                let resolution = match self {
                    Page::Tiled { resolution, .. } => *resolution,
                    _ => unreachable!(),
                };
                write_rational(file, resolution)?;
                write_rational(file, resolution)?;
                file.write_all(&1u16.to_le_bytes())?;
                file.write_all(&1u16.to_le_bytes())?;
                file.write_all(&1u16.to_le_bytes())?;
                let reference_bw = align(file.stream_position()?, 4);
                pad_to(file, reference_bw)?;
                write_reference_black_white(file)?;
            }
            Page::Strip { description, .. } => {
                if let Some(text) = description {
                    file.write_all(text.as_bytes())?;
                    file.write_all(&[0])?;
                }
                let after_desc = bits + 6 + description.as_ref().map_or(0, |v| v.len() + 1) as u64;
                pad_to(file, align(after_desc, 2))?;
                let resolution = match self {
                    Page::Strip { resolution, .. } => *resolution,
                    _ => unreachable!(),
                };
                write_rational(file, resolution)?;
                write_rational(file, resolution)?;
                file.write_all(&1u16.to_le_bytes())?;
                file.write_all(&1u16.to_le_bytes())?;
                file.write_all(&1u16.to_le_bytes())?;
                let reference_bw = align(file.stream_position()?, 4);
                pad_to(file, reference_bw)?;
                write_reference_black_white(file)?;
            }
        }
        Ok(())
    }
}

struct Extra {
    bits: u64,
    tile_offsets: u64,
    tile_counts: u64,
    desc: u64,
    xres: u64,
    yres: u64,
    sample: u64,
    reference_bw: u64,
    desc_text: String,
}

fn compose_tile(
    slide: &Slide,
    source: &[u8],
    level: &Level,
    output_row: u32,
    output_col: u32,
    merge_rows: u32,
    merge_cols: u32,
    output_width: u32,
    output_height: u32,
    mut hevc: Option<&mut HevcDecoder>,
) -> Result<RgbImage> {
    let mut output = RgbImage::from_pixel(output_width, output_height, Rgb([255, 255, 255]));
    if !level.tile_positions.is_empty() {
        let origin_x = output_col * output_width;
        let origin_y = output_row * output_height;
        let limit_x = origin_x + output_width;
        let limit_y = origin_y + output_height;
        let candidates = if level.tile_groups.is_empty() {
            (0..level.tiles.len()).collect()
        } else {
            level
                .tile_groups
                .get((output_row * level.tile_cols + output_col) as usize)
                .cloned()
                .unwrap_or_default()
        };
        for index in candidates {
            let range = level.tiles[index];
            let Some(position) = level.tile_positions.get(index) else {
                continue;
            };
            if !range.present() || position.x >= limit_x || position.y >= limit_y {
                continue;
            }
            let bytes = mapped_range(source, range)?;
            let image = decode_tile(slide, bytes, hevc.as_deref_mut())?;
            let source_width = position.width.min(image.width());
            let source_height = position.height.min(image.height());
            let right = (position.x + source_width).min(limit_x);
            let bottom = (position.y + source_height).min(limit_y);
            let left = origin_x.max(position.x);
            let top = origin_y.max(position.y);
            if left >= right || top >= bottom {
                continue;
            }
            let src_left = left - position.x;
            let src_top = top - position.y;
            for y in 0..(bottom - top) {
                for x in 0..(right - left) {
                    output.put_pixel(
                        left - origin_x + x,
                        top - origin_y + y,
                        *image.get_pixel(src_left + x, src_top + y),
                    );
                }
            }
        }
        return Ok(output);
    }
    for inner_row in 0..merge_rows {
        let row = output_row * merge_rows + inner_row;
        if row >= level.tile_rows {
            continue;
        }
        for inner_col in 0..merge_cols {
            let col = output_col * merge_cols + inner_col;
            if col >= level.tile_cols {
                continue;
            }
            let index = (row * level.tile_cols + col) as usize;
            let range = level.tiles[index];
            if !range.present() {
                continue;
            }
            let data = mapped_range(source, range)?;
            let image = decode_tile(slide, data, hevc.as_deref_mut())?;
            copy_clipped(
                &mut output,
                &image,
                inner_col * slide.tile_width,
                inner_row * slide.tile_height,
            );
        }
    }
    Ok(output)
}

fn aperio_description(
    slide: &Slide,
    level: &Level,
    tile_width: u32,
    tile_height: u32,
    quality: u8,
) -> String {
    format!(
        "{}\n{}x{} [0,0 {}x{}] ({}x{}) JPEG/RGB Q={}|AppMag = {}|MPP = {:.6}",
        APERIO_VERSION,
        level.width,
        level.height,
        level.width,
        level.height,
        tile_width,
        tile_height,
        quality,
        slide.metadata.app_mag,
        slide.metadata.mpp
    )
}

fn gcd(mut a: u32, mut b: u32) -> u32 {
    while b != 0 {
        let r = a % b;
        a = b;
        b = r;
    }
    a
}

fn jpeg_is_420(data: &[u8]) -> bool {
    if !data.starts_with(&[0xff, 0xd8]) {
        return false;
    }
    let mut offset = 2usize;
    while offset + 4 <= data.len() {
        if data[offset] != 0xff {
            offset += 1;
            continue;
        }
        while offset < data.len() && data[offset] == 0xff {
            offset += 1;
        }
        if offset >= data.len() {
            return false;
        }
        let marker = data[offset];
        offset += 1;
        if marker == 0xd8 || marker == 0xd9 || (0xd0..=0xd7).contains(&marker) {
            continue;
        }
        if offset + 2 > data.len() {
            return false;
        }
        let length = u16::from_be_bytes([data[offset], data[offset + 1]]) as usize;
        if length < 2 || offset + length > data.len() {
            return false;
        }
        if matches!(
            marker,
            0xc0 | 0xc1
                | 0xc2
                | 0xc3
                | 0xc5
                | 0xc6
                | 0xc7
                | 0xc9
                | 0xca
                | 0xcb
                | 0xcd
                | 0xce
                | 0xcf
        ) {
            if length < 8 {
                return false;
            }
            let components = data[offset + 7] as usize;
            if components != 3 || length < 8 + components * 3 {
                return false;
            }
            return data[offset + 9] == 0x22
                && data[offset + 12] == 0x11
                && data[offset + 15] == 0x11;
        }
        offset += length;
    }
    false
}

fn align(value: u64, alignment: u64) -> u64 {
    (value + alignment - 1) / alignment * alignment
}
fn pad_to(file: &mut File, offset: u64) -> Result<()> {
    let current = file.stream_position()?;
    if offset > current {
        file.write_all(&vec![0; usize::try_from(offset - current)?])?;
    }
    Ok(())
}
fn write_rational(file: &mut File, value: f64) -> Result<()> {
    let numerator = (value.max(0.0) * 1000.0).round() as u32;
    file.write_all(&numerator.to_le_bytes())?;
    file.write_all(&1000u32.to_le_bytes())?;
    Ok(())
}
fn write_reference_black_white(file: &mut File) -> Result<()> {
    for value in [0u32, 255, 128, 255, 128, 255] {
        file.write_all(&value.to_le_bytes())?;
        file.write_all(&1u32.to_le_bytes())?;
    }
    Ok(())
}
fn short(tag: u16, value: u16) -> Entry {
    Entry {
        tag,
        kind: 3,
        count: 1,
        value: value as u32,
    }
}
fn short_pair(tag: u16, first: u16, second: u16) -> Entry {
    Entry {
        tag,
        kind: 3,
        count: 2,
        value: first as u32 | ((second as u32) << 16),
    }
}
fn short_array_at(tag: u16, offset: u64) -> Entry {
    Entry {
        tag,
        kind: 3,
        count: 3,
        value: offset as u32,
    }
}
fn long(tag: u16, value: u32) -> Entry {
    Entry {
        tag,
        kind: 4,
        count: 1,
        value,
    }
}
fn long_at(tag: u16, value: u32) -> Entry {
    long(tag, value)
}
fn rational_at(tag: u16, offset: u64) -> Entry {
    Entry {
        tag,
        kind: 5,
        count: 1,
        value: offset as u32,
    }
}
fn rational_array_at(tag: u16, offset: u64, count: u32) -> Result<Entry> {
    Ok(Entry {
        tag,
        kind: 5,
        count,
        value: u32::try_from(offset).context("TIFF rational array offset overflow")?,
    })
}
fn array_at(tag: u16, count: usize, offset: u64) -> Result<Entry> {
    Ok(Entry {
        tag,
        kind: 4,
        count: u32::try_from(count)?,
        value: u32::try_from(offset).context("TIFF extra data offset overflow")?,
    })
}
fn u64_long_array_at(tag: u16, values: &[u64], offset: u64) -> Result<Entry> {
    if values.len() == 1 {
        Ok(long(tag, u32::try_from(values[0])?))
    } else {
        array_at(tag, values.len(), offset)
    }
}
fn long_array_at(tag: u16, values: &[u32], offset: u64) -> Result<Entry> {
    if values.len() == 1 {
        Ok(long(tag, values[0]))
    } else {
        array_at(tag, values.len(), offset)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ByteRange, Metadata};
    use std::collections::HashMap;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temporary_tiff(name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "img2svs-rust-{name}-{}-{nonce}.tif",
            std::process::id()
        ))
    }

    fn classic_entries(bytes: &[u8]) -> HashMap<u16, Entry> {
        let first = u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize;
        let count = u16::from_le_bytes(bytes[first..first + 2].try_into().unwrap()) as usize;
        (0..count)
            .map(|index| {
                let start = first + 2 + index * 12;
                let entry = Entry {
                    tag: u16::from_le_bytes(bytes[start..start + 2].try_into().unwrap()),
                    kind: u16::from_le_bytes(bytes[start + 2..start + 4].try_into().unwrap()),
                    count: u32::from_le_bytes(bytes[start + 4..start + 8].try_into().unwrap()),
                    value: u32::from_le_bytes(bytes[start + 8..start + 12].try_into().unwrap()),
                };
                (entry.tag, entry)
            })
            .collect()
    }

    #[test]
    fn jpeg_strip_declares_eight_bit_ycbcr_samples() -> Result<()> {
        let path = temporary_tiff("strip-tags");
        let mut writer = TiffWriter::create(&path)?;
        let image = RgbImage::from_pixel(8, 8, Rgb([12, 34, 56]));
        writer.write_strip_page(&image, 75, 1.0, None)?;
        writer.finish()?;
        drop(writer);

        let bytes = fs::read(&path)?;
        fs::remove_file(&path)?;
        let entries = classic_entries(&bytes);
        let bits = entries[&258].value as usize;
        assert_eq!(&bytes[bits..bits + 6], &[8, 0, 8, 0, 8, 0]);
        assert_eq!(entries[&262].value, 6);
        assert_eq!(entries[&530].count, 2);
        assert_eq!(entries[&530].value, 0x0002_0002);
        assert_eq!(entries[&532].count, 6);
        let reference_bw = entries[&532].value as usize;
        let expected_reference_bw = [0u32, 1, 255, 1, 128, 1, 255, 1, 128, 1, 255, 1];
        for (index, expected) in expected_reference_bw.into_iter().enumerate() {
            let start = reference_bw + index * 4;
            assert_eq!(
                u32::from_le_bytes(bytes[start..start + 4].try_into().unwrap()),
                expected
            );
        }
        let jpeg = entries[&273].value as usize;
        assert_eq!(&bytes[jpeg..jpeg + 2], &[0xff, 0xd8]);
        Ok(())
    }

    #[test]
    fn jpeg_sampling_check_accepts_only_420_data() -> Result<()> {
        let encoded = encode_jpeg(&RgbImage::from_pixel(8, 8, Rgb([1, 2, 3])), 75)?;
        assert!(jpeg_is_420(&encoded));

        let jpeg_422 = [
            0xff, 0xd8, 0xff, 0xc0, 0x00, 0x11, 0x08, 0x00, 0x01, 0x00, 0x01, 0x03, 0x01, 0x21,
            0x00, 0x02, 0x11, 0x01, 0x03, 0x11, 0x01, 0xff, 0xd9,
        ];
        assert!(!jpeg_is_420(&jpeg_422));
        Ok(())
    }

    #[test]
    fn single_tile_offset_and_count_are_stored_inline() -> Result<()> {
        let path = temporary_tiff("single-tile");
        let mut writer = TiffWriter::create(&path)?;
        writer.file.write_all(&[0xff, 0xd8, 0xff, 0xd9])?;
        writer.write_ifd(Page::Tiled {
            width: 8,
            height: 8,
            tile_width: 16,
            tile_height: 16,
            offsets: vec![8],
            counts: vec![4],
            description: "test".to_owned(),
            reduced: true,
            resolution: 1.0,
        })?;
        writer.finish()?;
        drop(writer);

        let bytes = fs::read(&path)?;
        fs::remove_file(&path)?;
        let entries = classic_entries(&bytes);
        assert_eq!(entries[&324].count, 1);
        assert_eq!(entries[&324].value, 8);
        assert_eq!(entries[&325].count, 1);
        assert_eq!(entries[&325].value, 4);
        Ok(())
    }

    #[test]
    fn parallel_tile_encoding_preserves_output_order() -> Result<()> {
        let path = temporary_tiff("parallel-source");
        let colors = [20u8, 80, 160, 230];
        let mut source = File::create(&path)?;
        source.write_all(&[0])?;
        let mut ranges = Vec::new();
        for value in colors {
            let encoded = encode_jpeg(
                &RgbImage::from_pixel(16, 16, Rgb([value, value, value])),
                75,
            )?;
            let offset = source.stream_position()?;
            source.write_all(&encoded)?;
            ranges.push(ByteRange {
                offset,
                length: encoded.len() as u64,
            });
        }
        drop(source);
        let slide = Slide {
            path: path.clone(),
            metadata: Metadata {
                width: 32,
                height: 32,
                mpp: 0.25,
                app_mag: 40.0,
                jpeg_quality: 75,
            },
            tile_width: 16,
            tile_height: 16,
            compression: Compression::Jpeg,
            levels: vec![Level {
                index: 0,
                width: 32,
                height: 32,
                downsample: 1.0,
                tile_cols: 2,
                tile_rows: 2,
                tiles: ranges,
                tile_positions: Vec::new(),
                tile_groups: Vec::new(),
            }],
            associated_images: Vec::new(),
            thumbnail: None,
        };
        let pool = TilePool::with_worker_count(&slide, 4)?;
        let tasks: Vec<_> = (0..4)
            .map(|index| TileTask {
                slot: index,
                level_index: 0,
                output_row: index as u32 / 2,
                output_col: index as u32 % 2,
                merge_rows: 1,
                merge_cols: 1,
                output_width: 16,
                output_height: 16,
                quality: 75,
            })
            .collect();
        let encoded = pool.encode_batch(&tasks)?;
        drop(pool);
        fs::remove_file(path)?;

        for (data, expected) in encoded.iter().zip(colors) {
            let image = decode_rgb(data)?;
            let pixel = image.get_pixel(8, 8);
            for channel in pixel.0 {
                assert!((channel as i16 - expected as i16).abs() <= 3);
            }
        }
        Ok(())
    }
}
fn ascii_at(tag: u16, offset: u64, count: usize) -> Result<Entry> {
    Ok(Entry {
        tag,
        kind: 2,
        count: u32::try_from(count)?,
        value: u32::try_from(offset).context("TIFF description offset overflow")?,
    })
}
