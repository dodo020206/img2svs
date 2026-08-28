use crate::binary::Reader;
use crate::model::{AssociatedImage, ByteRange, Compression, Level, Metadata, Slide, Thumbnail};
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

const PIC_HEAD_FLAG: u16 = 0x5153;
const PERSON_INFO_FLAG: u16 = 0x4950;
const MACROGRAPH_INFO_FLAG: u16 = 0x494d;
const PIC_INFO_FLAG: u16 = 0x4649;

pub fn parse(path: &Path) -> Result<Slide> {
    let mut reader = Reader::open(path)?;
    let head = read_pic_head(&mut reader)?;
    reader.seek(head.head_size)?;
    if reader.u16()? != PERSON_INFO_FLAG {
        bail!("unsupported SDPC person-info block");
    }
    let _info_size = reader.u32()?;
    reader.bytes(
        64 + 64 + 1 + 1 + 64 + 64 + 1024 + 2048 + 2048 + 64 + 64 + 1024,
        "SDPC person-info",
    )?;
    let person_next = reader.u64()?;
    reader.bytes(4 + 4 + 256, "SDPC person-info tail")?;

    let mut associated = Vec::new();
    let mut current = person_next;
    for index in 0..head.macrograph_count {
        reader.seek(current)?;
        if reader.u16()? != MACROGRAPH_INFO_FLAG {
            bail!("unsupported SDPC macrograph block");
        }
        reader.bytes(8, "SDPC macrograph rgb")?;
        let width = reader.u32()?;
        let height = reader.u32()?;
        reader.bytes(4 + 4 + 8, "SDPC macrograph metadata")?;
        let encoded_size = reader.u64()?;
        reader.u8()?;
        let next = reader.u64()?;
        reader.bytes(4 + 4 + 64, "SDPC macrograph tail")?;
        let data = ByteRange {
            offset: current + 123,
            length: encoded_size,
        };
        validate_range(data, reader.len(), "macrograph")?;
        associated.push(AssociatedImage {
            kind: if index == 0 {
                "label"
            } else if index == 1 {
                "macro"
            } else {
                "macro_other"
            }
            .to_owned(),
            data,
        });
        let _ = (width, height);
        current = next;
    }

    let thumbnail_info = read_pic_info(&mut reader, current)?;
    if thumbnail_info.slice_num != 1
        || thumbnail_info.slice_num_x != 1
        || thumbnail_info.slice_num_y != 1
    {
        bail!("unsupported SDPC thumbnail layout");
    }
    let thumbnail = Thumbnail {
        width: head.thumbnail_width,
        height: head.thumbnail_height,
        data: ByteRange {
            offset: current + thumbnail_info.info_size as u64,
            length: thumbnail_info.layer_size,
        },
    };

    let mut levels = Vec::new();
    let mut current_level = thumbnail_info.next_layer_offset;
    for index in 0..head.hierarchy {
        let info = read_pic_info(&mut reader, current_level)?;
        if info.slice_num
            != info
                .slice_num_x
                .checked_mul(info.slice_num_y)
                .context("SDPC tile count overflow")?
        {
            bail!("SDPC tile count mismatch at level {index}");
        }
        let downsample = downsample_from_scale(info.cur_scale)?;
        let count = info.slice_num as usize;
        reader.seek(current_level + info.info_size as u64)?;
        let mut lengths = Vec::with_capacity(count);
        for _ in 0..count {
            let length = reader.i32()?;
            if length < 0 {
                bail!("negative SDPC tile length");
            }
            lengths.push(length as u64);
        }
        let mut offset = current_level + info.info_size as u64 + count as u64 * 4;
        let mut tiles = Vec::with_capacity(count);
        for length in lengths {
            let data = ByteRange { offset, length };
            validate_range(data, reader.len(), "level tile")?;
            tiles.push(data);
            offset += length;
        }
        levels.push(Level {
            index: index as usize,
            width: (head.src_width as f64 / downsample as f64).floor().max(1.0) as u32,
            height: (head.src_height as f64 / downsample as f64)
                .floor()
                .max(1.0) as u32,
            downsample: downsample as f64,
            tile_cols: info.slice_num_x,
            tile_rows: info.slice_num_y,
            tiles,
            tile_positions: Vec::new(),
            tile_groups: Vec::new(),
        });
        current_level = info.next_layer_offset;
    }
    if levels.is_empty() {
        bail!("SDPC file contains no pyramid levels");
    }
    let compression = match head.slice_fmt {
        0 => Compression::Jpeg,
        4 => Compression::Hevc,
        value => bail!("unsupported SDPC tile compression mode: {value}"),
    };
    Ok(Slide {
        path: PathBuf::from(path),
        metadata: Metadata {
            width: head.src_width,
            height: head.src_height,
            mpp: head.ruler,
            app_mag: head.rate as f64,
            jpeg_quality: head.jpeg_quality.clamp(1, 100),
        },
        tile_width: head.tile_width,
        tile_height: head.tile_height,
        compression,
        levels,
        associated_images: associated,
        thumbnail: Some(thumbnail),
    })
}

#[derive(Debug)]
struct PicHead {
    head_size: u64,
    macrograph_count: u32,
    hierarchy: u32,
    src_width: u32,
    src_height: u32,
    tile_width: u32,
    tile_height: u32,
    thumbnail_width: u32,
    thumbnail_height: u32,
    jpeg_quality: u8,
    ruler: f64,
    rate: u32,
    slice_fmt: u8,
}

fn read_pic_head(reader: &mut Reader) -> Result<PicHead> {
    reader.seek(0)?;
    if reader.u16()? != PIC_HEAD_FLAG {
        bail!("unsupported SqPicHead flag");
    }
    reader.bytes(16, "SDPC version")?;
    let head_size = reader.u32()? as u64;
    let _file_size = reader.u64()?;
    let macrograph_count = reader.u32()?;
    reader.u32()?;
    let hierarchy = reader.u32()?;
    let src_width = reader.u32()?;
    let src_height = reader.u32()?;
    let tile_width = reader.u32()?;
    let tile_height = reader.u32()?;
    let thumbnail_width = reader.u32()?;
    let thumbnail_height = reader.u32()?;
    reader.u8()?;
    let jpeg_quality = reader.u8()?;
    reader.u8()?;
    reader.bytes(3, "SDPC color fields")?;
    let scale = reader.f32()?;
    let ruler = reader.f64()?;
    let rate = reader.u32()?;
    let _extra_offset = reader.u64()?;
    let _tile_offset = reader.u64()?;
    let slice_fmt = reader.u8()?;
    if scale <= 0.0 || ruler <= 0.0 || rate == 0 || tile_width == 0 || tile_height == 0 {
        bail!("invalid SDPC dimensions or metadata");
    }
    Ok(PicHead {
        head_size,
        macrograph_count,
        hierarchy,
        src_width,
        src_height,
        tile_width,
        tile_height,
        thumbnail_width,
        thumbnail_height,
        jpeg_quality,
        ruler,
        rate,
        slice_fmt,
    })
}

#[derive(Clone, Copy)]
struct PicInfo {
    info_size: u32,
    slice_num: u32,
    slice_num_x: u32,
    slice_num_y: u32,
    layer_size: u64,
    next_layer_offset: u64,
    cur_scale: f32,
}

fn read_pic_info(reader: &mut Reader, offset: u64) -> Result<PicInfo> {
    reader.seek(offset)?;
    if reader.u16()? != PIC_INFO_FLAG {
        bail!("unsupported SqPicInfo flag at {offset}");
    }
    let info_size = reader.u32()?;
    let _layer = reader.u32()?;
    let slice_num = reader.u32()?;
    let slice_num_x = reader.u32()?;
    let slice_num_y = reader.u32()?;
    let layer_size = reader.u64()?;
    let next_layer_offset = reader.u64()?;
    let cur_scale = reader.f32()?;
    reader.bytes(8 + 4 + 4 + 1 + 63, "SDPC picture-info tail")?;
    Ok(PicInfo {
        info_size,
        slice_num,
        slice_num_x,
        slice_num_y,
        layer_size,
        next_layer_offset,
        cur_scale,
    })
}

fn downsample_from_scale(scale: f32) -> Result<u32> {
    if scale <= 0.0 {
        bail!("invalid SDPC level scale: {scale}");
    }
    let value = (1.0 / scale).round() as u32;
    if value == 0 || ((scale * value as f32) - 1.0).abs() > 1e-5 {
        bail!("unsupported non-integral SDPC level scale: {scale}");
    }
    Ok(value)
}

fn validate_range(data: ByteRange, file_size: u64, context: &str) -> Result<()> {
    if !data.present() || data.offset >= file_size || data.length > file_size - data.offset {
        bail!("invalid SDPC byte range for {context}");
    }
    Ok(())
}

pub fn print_info(slide: &Slide) {
    println!(
        "Image : {}x{}, tile={}x{}, levels={}, compression={:?}",
        slide.metadata.width,
        slide.metadata.height,
        slide.tile_width,
        slide.tile_height,
        slide.levels.len(),
        slide.compression
    );
    println!(
        "Meta  : mpp={:.6}, app_mag={}, jpeg_quality={}",
        slide.metadata.mpp, slide.metadata.app_mag, slide.metadata.jpeg_quality
    );
    println!(
        "Pyr   : {}",
        slide
            .levels
            .iter()
            .map(|l| format!(
                "L{}={}x{} ({}x{} tiles)",
                l.index, l.width, l.height, l.tile_cols, l.tile_rows
            ))
            .collect::<Vec<_>>()
            .join(", ")
    );
    println!(
        "Assoc : {}",
        if slide.associated_images.is_empty() {
            "none".to_owned()
        } else {
            slide
                .associated_images
                .iter()
                .map(|v| v.kind.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        }
    );
}
