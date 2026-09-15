//! Reader for the SDPC/DYQX family of containers.
//!
//! The file is a chain of vendor SDK blocks: a fixed picture header, a
//! person-info block, zero or more macrograph (label/macro) blocks, a thumbnail
//! block and finally one picture-info block per pyramid level. Every block
//! starts with a flag and its own size, and ends with the offset of the next
//! block, so parsing is a walk along those offsets rather than a fixed layout.
//!
//! Tiles are addressed through an `i32` length table that immediately follows
//! each level's picture-info block; tile payloads follow the table back to back
//! and are therefore reconstructed by accumulating lengths.

use crate::binary::Reader;
use crate::model::{AssociatedImage, ByteRange, Compression, Level, Metadata, Slide, Thumbnail};
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

const PIC_HEAD_FLAG: u16 = 0x5153;
const PERSON_INFO_FLAG: u16 = 0x4950;
const MACROGRAPH_INFO_FLAG: u16 = 0x494d;
const PIC_INFO_FLAG: u16 = 0x4649;

/// Reserved bytes between the picture-header flag and its first used field.
const PIC_HEAD_VERSION_SIZE: usize = 16;
/// Colour-model fields that follow the JPEG quality byte.
const PIC_HEAD_COLOR_FIELDS_SIZE: usize = 3;
/// Reserved bytes after the person-info flag and size.
const PERSON_INFO_HEADER_SIZE: usize =
    64 + 64 + 1 + 1 + 64 + 64 + 1024 + 2048 + 2048 + 64 + 64 + 1024;
/// Reserved bytes between the person-info next-offset and the next block.
const PERSON_INFO_TAIL_SIZE: usize = 4 + 4 + 256;
/// Reserved bytes between a picture-info next-offset and the next block.
const PIC_INFO_TAIL_SIZE: usize = 8 + 4 + 4 + 1 + 63;

/// Width of the size field that follows a block's flag.
const BLOCK_SIZE_FIELD_SIZE: usize = 4;
/// Reserved sub-fields of the picture header, in file order.
const PIC_HEAD_FILE_SIZE_SIZE: usize = 8;
const PIC_HEAD_RESERVED_SIZE: usize = 4;
const PIC_HEAD_BYTE_RESERVED_SIZE: usize = 1;
const PIC_HEAD_OFFSET_SIZE: usize = 8;
/// Reserved sub-field of a picture-info block ahead of the tile counts.
const PIC_INFO_LAYER_FIELD_SIZE: usize = 4;
/// Tolerance when checking that a level scale is an exact reciprocal.
const SCALE_EPSILON: f32 = 1e-5;

/// Fixed fields of a macrograph block, in file order.
const MACROGRAPH_FLAG_SIZE: usize = 2;
const MACROGRAPH_RGB_SIZE: usize = 8;
const MACROGRAPH_DIMENSIONS_SIZE: usize = 8;
const MACROGRAPH_METADATA_SIZE: usize = 16;
const MACROGRAPH_ENCODED_SIZE_FIELD: usize = 8;
const MACROGRAPH_STREAM_MARKER_SIZE: usize = 1;
const MACROGRAPH_NEXT_OFFSET_FIELD: usize = 8;
const MACROGRAPH_TAIL_SIZE: usize = 4 + 4 + 64;
/// Offset of the payload inside a macrograph block, i.e. the sum of every fixed
/// field that precedes it.
const MACROGRAPH_DATA_OFFSET: usize = MACROGRAPH_FLAG_SIZE
    + MACROGRAPH_RGB_SIZE
    + MACROGRAPH_DIMENSIONS_SIZE
    + MACROGRAPH_METADATA_SIZE
    + MACROGRAPH_ENCODED_SIZE_FIELD
    + MACROGRAPH_STREAM_MARKER_SIZE
    + MACROGRAPH_NEXT_OFFSET_FIELD
    + MACROGRAPH_TAIL_SIZE;

/// Width of one entry in a level's tile length table.
const TILE_LENGTH_SIZE: u64 = 4;

/// Slice-format codes of [`PicHead::slice_fmt`].
const SLICE_FORMAT_JPEG: u8 = 0;
const SLICE_FORMAT_HEVC: u8 = 4;

/// Parses `path` into a slide description without decoding tile pixels.
pub fn parse(path: &Path) -> Result<Slide> {
    let mut reader = Reader::open(path)?;
    let head = read_pic_head(&mut reader)?;
    let after_person_info = read_person_info(&mut reader, head.head_size)?;
    let macrographs = read_macrographs(&mut reader, head.macrograph_count, after_person_info)?;

    let thumbnail_offset = macrographs.next_block_offset;
    let thumbnail_info = read_pic_info(&mut reader, thumbnail_offset)?;
    let thumbnail = thumbnail_from(&head, &thumbnail_info, thumbnail_offset)?;
    let levels = read_levels(&mut reader, &head, thumbnail_info.next_layer_offset)?;

    Ok(Slide {
        path: PathBuf::from(path),
        metadata: Metadata {
            width: head.src_width,
            height: head.src_height,
            mpp: head.ruler,
            app_mag: f64::from(head.rate),
            jpeg_quality: head.jpeg_quality.clamp(1, 100),
        },
        tile_width: head.tile_width,
        tile_height: head.tile_height,
        compression: compression_from(head.slice_fmt)?,
        levels,
        associated_images: macrographs.images,
        thumbnail: Some(thumbnail),
    })
}

/// Walks the person-info block and returns the offset that follows it.
fn read_person_info(reader: &mut Reader, head_size: u64) -> Result<u64> {
    reader.seek(head_size)?;
    if reader.u16()? != PERSON_INFO_FLAG {
        bail!("unsupported SDPC person-info block");
    }
    reader.skip(BLOCK_SIZE_FIELD_SIZE, "SDPC person-info size")?;
    reader.skip(PERSON_INFO_HEADER_SIZE, "SDPC person-info")?;
    let next = reader.u64()?;
    reader.skip(PERSON_INFO_TAIL_SIZE, "SDPC person-info tail")?;
    Ok(next)
}

/// The macrograph chain: the images it holds and the block that follows it.
struct Macrographs {
    images: Vec<AssociatedImage>,
    next_block_offset: u64,
}

/// Reads every macrograph block into an associated image.
///
/// The vendor SDK allows several; the first two are the label and the macro
/// image, and any extras are preserved under a numbered kind. The chain is only
/// navigable by following each block's next offset, so the trailing offset is
/// returned from the same walk.
fn read_macrographs(reader: &mut Reader, count: u32, start: u64) -> Result<Macrographs> {
    let mut images = Vec::with_capacity(count as usize);
    let mut current = start;
    for index in 0..count {
        reader.seek(current)?;
        if reader.u16()? != MACROGRAPH_INFO_FLAG {
            bail!("unsupported SDPC macrograph block");
        }
        reader.skip(MACROGRAPH_RGB_SIZE, "SDPC macrograph rgb")?;
        reader.skip(MACROGRAPH_DIMENSIONS_SIZE, "SDPC macrograph dimensions")?;
        reader.skip(MACROGRAPH_METADATA_SIZE, "SDPC macrograph metadata")?;
        let encoded_size = reader.u64()?;
        reader.skip(
            MACROGRAPH_STREAM_MARKER_SIZE,
            "SDPC macrograph stream marker",
        )?;
        let next = reader.u64()?;
        reader.skip(MACROGRAPH_TAIL_SIZE, "SDPC macrograph tail")?;

        let data = ByteRange {
            offset: current + MACROGRAPH_DATA_OFFSET as u64,
            length: encoded_size,
        };
        data.validate(reader.len(), "SDPC macrograph")?;
        images.push(AssociatedImage {
            kind: match index {
                0 => "label",
                1 => "macro",
                _ => "macro_other",
            }
            .to_owned(),
            data,
        });
        current = next;
    }
    Ok(Macrographs {
        images,
        next_block_offset: current,
    })
}

/// Builds the thumbnail from the picture-info block that describes it.
fn thumbnail_from(head: &PicHead, info: &PicInfo, block_offset: u64) -> Result<Thumbnail> {
    if info.slice_num != 1 || info.slice_num_x != 1 || info.slice_num_y != 1 {
        bail!("unsupported SDPC thumbnail layout");
    }
    Ok(Thumbnail {
        width: head.thumbnail_width,
        height: head.thumbnail_height,
        data: ByteRange {
            offset: block_offset + u64::from(info.info_size),
            length: info.layer_size,
        },
    })
}

/// Reads every pyramid level starting from the thumbnail's successor.
fn read_levels(reader: &mut Reader, head: &PicHead, start: u64) -> Result<Vec<Level>> {
    let mut levels = Vec::with_capacity(head.hierarchy as usize);
    let mut current = start;
    for index in 0..head.hierarchy {
        let info = read_pic_info(reader, current)?;
        let expected = info
            .slice_num_x
            .checked_mul(info.slice_num_y)
            .context("SDPC tile count overflow")?;
        if info.slice_num != expected {
            bail!("SDPC tile count mismatch at level {index}");
        }
        let downsample = downsample_from_scale(info.cur_scale)?;
        let tiles = read_level_tiles(reader, current, &info)?;
        levels.push(Level {
            index: index as usize,
            width: level_dimension(head.src_width, downsample),
            height: level_dimension(head.src_height, downsample),
            downsample: f64::from(downsample),
            tile_cols: info.slice_num_x,
            tile_rows: info.slice_num_y,
            tiles,
            tile_positions: Vec::new(),
            tile_groups: Vec::new(),
        });
        current = info.next_layer_offset;
    }
    if levels.is_empty() {
        bail!("SDPC file contains no pyramid levels");
    }
    Ok(levels)
}

/// Reads a level's tile length table and turns it into byte ranges.
///
/// The table follows the picture-info block and holds one `i32` length per
/// tile; payloads are stored contiguously after the table.
fn read_level_tiles(
    reader: &mut Reader,
    level_offset: u64,
    info: &PicInfo,
) -> Result<Vec<ByteRange>> {
    let table_offset = level_offset + u64::from(info.info_size);
    let count = info.slice_num as usize;
    reader.seek(table_offset)?;
    let mut lengths = Vec::with_capacity(count);
    for _ in 0..count {
        let length = reader.i32()?;
        if length < 0 {
            bail!("negative SDPC tile length");
        }
        lengths.push(length as u64);
    }

    let file_size = reader.len();
    let mut offset = table_offset + count as u64 * TILE_LENGTH_SIZE;
    let mut tiles = Vec::with_capacity(count);
    for length in lengths {
        let data = ByteRange { offset, length };
        data.validate(file_size, "SDPC level tile")?;
        tiles.push(data);
        offset += length;
    }
    Ok(tiles)
}

/// Maps the container's slice format code onto a compression mode.
fn compression_from(slice_fmt: u8) -> Result<Compression> {
    match slice_fmt {
        SLICE_FORMAT_JPEG => Ok(Compression::Jpeg),
        SLICE_FORMAT_HEVC => Ok(Compression::Hevc),
        value => bail!("unsupported SDPC tile compression mode: {value}"),
    }
}

/// Level dimensions, i.e. the level-0 size divided by the downsample factor.
fn level_dimension(source: u32, downsample: u32) -> u32 {
    (f64::from(source) / f64::from(downsample)).floor().max(1.0) as u32
}

/// Fields of the fixed `SqPicHead` block at offset 0.
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

/// Reads and validates the picture header at offset 0.
fn read_pic_head(reader: &mut Reader) -> Result<PicHead> {
    reader.seek(0)?;
    if reader.u16()? != PIC_HEAD_FLAG {
        bail!("unsupported SqPicHead flag");
    }
    reader.skip(PIC_HEAD_VERSION_SIZE, "SDPC version")?;
    let head_size = u64::from(reader.u32()?);
    reader.skip(PIC_HEAD_FILE_SIZE_SIZE, "SDPC file size")?;
    let macrograph_count = reader.u32()?;
    reader.skip(PIC_HEAD_RESERVED_SIZE, "SDPC head reserved")?;
    let hierarchy = reader.u32()?;
    let src_width = reader.u32()?;
    let src_height = reader.u32()?;
    let tile_width = reader.u32()?;
    let tile_height = reader.u32()?;
    let thumbnail_width = reader.u32()?;
    let thumbnail_height = reader.u32()?;
    reader.skip(PIC_HEAD_BYTE_RESERVED_SIZE, "SDPC head reserved")?;
    let jpeg_quality = reader.u8()?;
    reader.skip(PIC_HEAD_BYTE_RESERVED_SIZE, "SDPC head reserved")?;
    reader.skip(PIC_HEAD_COLOR_FIELDS_SIZE, "SDPC color fields")?;
    let scale = reader.f32()?;
    let ruler = reader.f64()?;
    let rate = reader.u32()?;
    reader.skip(PIC_HEAD_OFFSET_SIZE, "SDPC extra offset")?;
    reader.skip(PIC_HEAD_OFFSET_SIZE, "SDPC tile offset")?;
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

/// Fields of a `SqPicInfo` block, which describes either one level or the
/// thumbnail.
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

/// Reads a picture-info block located at absolute `offset`.
fn read_pic_info(reader: &mut Reader, offset: u64) -> Result<PicInfo> {
    reader.seek(offset)?;
    if reader.u16()? != PIC_INFO_FLAG {
        bail!("unsupported SqPicInfo flag at {offset}");
    }
    let info_size = reader.u32()?;
    reader.skip(PIC_INFO_LAYER_FIELD_SIZE, "SDPC picture-info layer")?;
    let slice_num = reader.u32()?;
    let slice_num_x = reader.u32()?;
    let slice_num_y = reader.u32()?;
    let layer_size = reader.u64()?;
    let next_layer_offset = reader.u64()?;
    let cur_scale = reader.f32()?;
    reader.skip(PIC_INFO_TAIL_SIZE, "SDPC picture-info tail")?;
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

/// Converts a level's scale factor into an integer downsample factor.
///
/// Scales are stored as `1 / 2^n`, so anything that is not an exact reciprocal
/// is rejected rather than silently rounded.
fn downsample_from_scale(scale: f32) -> Result<u32> {
    if scale <= 0.0 {
        bail!("invalid SDPC level scale: {scale}");
    }
    let value = (1.0 / scale).round() as u32;
    if value == 0 || ((scale * value as f32) - 1.0).abs() > SCALE_EPSILON {
        bail!("unsupported non-integral SDPC level scale: {scale}");
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn macrograph_payload_starts_after_every_fixed_field() {
        assert_eq!(MACROGRAPH_DATA_OFFSET, 123);
    }

    #[test]
    fn downsample_accepts_only_integral_reciprocal_scales() {
        assert_eq!(downsample_from_scale(1.0).unwrap(), 1);
        assert_eq!(downsample_from_scale(0.5).unwrap(), 2);
        assert_eq!(downsample_from_scale(0.25).unwrap(), 4);
        assert!(downsample_from_scale(0.3).is_err());
        assert!(downsample_from_scale(2.0).is_err());
        assert!(downsample_from_scale(0.0).is_err());
    }

    #[test]
    fn slice_format_maps_known_codes_and_rejects_the_rest() {
        assert_eq!(
            compression_from(SLICE_FORMAT_JPEG).unwrap(),
            Compression::Jpeg
        );
        assert_eq!(
            compression_from(SLICE_FORMAT_HEVC).unwrap(),
            Compression::Hevc
        );
        assert!(compression_from(9).is_err());
    }
}
