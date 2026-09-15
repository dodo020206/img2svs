//! Reader for the `DmetrixN` container.
//!
//! The header holds scan metadata, a fixed table of pyramid descriptors and one
//! tile index per level. Level 0 is the largest, but the descriptors are stored
//! smallest-first and each level records the grid extent of the level below it,
//! so `parse` walks them in reverse to recover the real image size.

use crate::binary::Reader;
use crate::jpeg::{decode_image, SOI_MARKER};
use crate::model::{AssociatedImage, ByteRange, Compression, Level, Metadata, Slide};
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

const MAGIC: &[u8; 8] = b"DmetrixN";
const TILE_RECORD_SIZE: u64 = 22;
const OFFSET_MPP_X: u64 = 0x30;
const OFFSET_MPP_Y: u64 = 0x38;
const OFFSET_APP_MAG: u64 = 0x40;
const OFFSET_DESCRIPTORS: u64 = 0xc2;
/// Upper bound on the descriptor table, which is terminated by a zero offset.
const MAX_LEVELS: usize = 64;
/// Sanity limit for the per-level tile grid.
const MAX_GRID_EXTENT: u32 = 100_000;
const MAX_MPP: f64 = 100.0;
const MAX_APP_MAG: u32 = 200;
/// Records stored immediately before the first level index hold the label and
/// macro images.
const ASSOCIATED_IMAGE_RECORDS: u64 = 2;
const LABEL_ID: u16 = 0xffff;
const MACRO_ID: u16 = 0xfffe;
const DEFAULT_JPEG_QUALITY: u8 = 75;
const MIN_TILE_SIZE: u32 = 16;
const MAX_TILE_SIZE: u32 = 4096;
/// Width of the reserved fields inside an associated-image record.
const ASSOCIATED_RESERVED_SIZE: usize = 4;

/// One pyramid level as described by the descriptor table.
#[derive(Clone, Copy, Debug)]
struct Descriptor {
    source_id: u16,
    max_x: u32,
    max_y: u32,
    index_offset: u64,
}

impl Descriptor {
    /// Number of tile columns, i.e. the largest x index plus one.
    fn cols(&self) -> u32 {
        self.max_x + 1
    }

    /// Number of tile rows, i.e. the largest y index plus one.
    fn rows(&self) -> u32 {
        self.max_y + 1
    }

    /// Total number of tiles in the level's grid.
    fn tile_count(&self) -> Option<u32> {
        self.cols().checked_mul(self.rows())
    }

    /// Whether `(x, y)` addresses a tile inside the level's grid.
    fn contains(&self, x: u32, y: u32) -> bool {
        x <= self.max_x && y <= self.max_y
    }

    /// Row-major position of the tile at `(x, y)` inside the level's index.
    fn slot(&self, x: u32, y: u32) -> usize {
        (y * self.cols() + x) as usize
    }
}

/// The scan geometry read from the fixed header.
#[derive(Clone, Copy, Debug)]
struct ScanMetadata {
    mpp_x: f64,
    mpp_y: f64,
    app_mag: u32,
}

impl ScanMetadata {
    /// Average of the two axis resolutions, which is what the SVS header wants.
    fn mpp(&self) -> f64 {
        (self.mpp_x + self.mpp_y) / 2.0
    }
}

/// Parses `path` into a slide description without decoding tile pixels.
pub fn parse(path: &Path) -> Result<Slide> {
    let mut reader = Reader::open(path)?;
    let file_size = reader.len();
    reader.seek(0)?;
    if reader.bytes(8, "DMetrix magic")? != MAGIC {
        bail!("unsupported DMetrix container: {}", path.display());
    }

    let scan = read_scan_metadata(&mut reader)?;
    let descriptors = read_descriptors(&mut reader)?;
    let associated = read_associated(&mut reader, descriptors[0].index_offset, file_size)?;
    let raw_levels = read_tile_indexes(&mut reader, &descriptors, file_size)?;
    let tile_size = discover_tile_size(&mut reader, first_tile(&raw_levels)?)?;
    let levels = build_levels(&mut reader, &descriptors, &raw_levels, tile_size)?;

    let top = levels.first().context("missing DMetrix levels")?;
    let jpeg_quality =
        estimate_quality(&mut reader, first_tile(&raw_levels)?).unwrap_or(DEFAULT_JPEG_QUALITY);
    Ok(Slide {
        path: PathBuf::from(path),
        metadata: Metadata {
            width: top.width,
            height: top.height,
            mpp: scan.mpp(),
            app_mag: f64::from(scan.app_mag),
            jpeg_quality,
        },
        tile_width: tile_size,
        tile_height: tile_size,
        compression: Compression::Jpeg,
        levels,
        associated_images: associated,
        thumbnail: None,
    })
}

/// Reads the micrometres-per-pixel and objective power from the header.
fn read_scan_metadata(reader: &mut Reader) -> Result<ScanMetadata> {
    let scan = ScanMetadata {
        mpp_x: read_f64_at(reader, OFFSET_MPP_X)?,
        mpp_y: read_f64_at(reader, OFFSET_MPP_Y)?,
        app_mag: read_u32_at(reader, OFFSET_APP_MAG)?,
    };
    if !(0.0 < scan.mpp_x
        && scan.mpp_x < MAX_MPP
        && 0.0 < scan.mpp_y
        && scan.mpp_y < MAX_MPP
        && scan.app_mag > 0
        && scan.app_mag <= MAX_APP_MAG)
    {
        bail!("invalid DMetrix scan metadata");
    }
    Ok(scan)
}

/// Converts the raw tile indexes into model levels, smallest level first.
///
/// The descriptor order is reversed so that `levels[0]` is the full-resolution
/// image, and each level's size is recovered from the tile grid of the level
/// above it plus the size of its own bottom-right edge tile.
fn build_levels(
    reader: &mut Reader,
    descriptors: &[Descriptor],
    raw_levels: &[Vec<ByteRange>],
    tile_size: u32,
) -> Result<Vec<Level>> {
    if descriptors.len() != raw_levels.len() {
        bail!("DMetrix level descriptor/index count mismatch");
    }
    let mut levels = Vec::with_capacity(descriptors.len());
    for (index, (descriptor, tiles)) in descriptors.iter().zip(raw_levels.iter()).rev().enumerate()
    {
        let edge = tiles[descriptor.slot(descriptor.max_x, descriptor.max_y)];
        let image = decode_image(&reader.range(edge.offset, edge.length, "DMetrix edge tile")?)?;
        if image.width() == 0
            || image.width() > tile_size
            || image.height() == 0
            || image.height() > tile_size
        {
            bail!(
                "invalid edge tile size at DMetrix level {}",
                descriptor.source_id
            );
        }
        levels.push(Level {
            index,
            width: descriptor.max_x * tile_size + image.width(),
            height: descriptor.max_y * tile_size + image.height(),
            downsample: 2f64.powi(index as i32),
            tile_cols: descriptor.cols(),
            tile_rows: descriptor.rows(),
            tiles: tiles.clone(),
            tile_positions: Vec::new(),
            tile_groups: Vec::new(),
        });
    }
    Ok(levels)
}

/// Reads the descriptor table, which ends at the first zero index offset.
fn read_descriptors(reader: &mut Reader) -> Result<Vec<Descriptor>> {
    reader.seek(OFFSET_DESCRIPTORS)?;
    let mut result = Vec::new();
    for _ in 0..MAX_LEVELS {
        let source_id = reader.u16()?;
        let max_x = reader.u32()?;
        let max_y = reader.u32()?;
        let index_offset = reader.u32()? as u64;
        if index_offset == 0 {
            break;
        }
        if max_x > MAX_GRID_EXTENT || max_y > MAX_GRID_EXTENT {
            bail!("invalid DMetrix level grid");
        }
        result.push(Descriptor {
            source_id,
            max_x,
            max_y,
            index_offset,
        });
    }
    if result.is_empty() {
        bail!("DMetrix file contains no pyramid levels");
    }
    for pair in result.windows(2) {
        if pair[1].source_id <= pair[0].source_id {
            bail!("DMetrix pyramid level identifiers are not increasing");
        }
    }
    Ok(result)
}

/// Reads the label and macro images stored ahead of the first level index.
fn read_associated(
    reader: &mut Reader,
    first_index: u64,
    file_size: u64,
) -> Result<Vec<AssociatedImage>> {
    let start = first_index
        .checked_sub(ASSOCIATED_IMAGE_RECORDS * TILE_RECORD_SIZE)
        .context("invalid DMetrix associated-image index")?;
    reader.seek(start)?;
    let mut label = None;
    let mut macro_image = None;
    for _ in 0..ASSOCIATED_IMAGE_RECORDS {
        let id = reader.u16()?;
        reader.skip(ASSOCIATED_RESERVED_SIZE, "DMetrix associated reserved")?;
        reader.skip(ASSOCIATED_RESERVED_SIZE, "DMetrix associated reserved")?;
        let offset = reader.u64()?;
        let length = reader.u32()? as u64;
        let data = ByteRange { offset, length };
        data.validate(file_size, "DMetrix associated image")?;
        if id == LABEL_ID {
            label = Some(data);
        } else if id == MACRO_ID {
            macro_image = Some(data);
        }
    }
    Ok([("label", label), ("macro", macro_image)]
        .into_iter()
        .filter_map(|(kind, data)| {
            data.map(|data| AssociatedImage {
                kind: kind.to_owned(),
                data,
            })
        })
        .collect())
}

/// Reads one tile index per level into a row-major `ByteRange` grid.
fn read_tile_indexes(
    reader: &mut Reader,
    descriptors: &[Descriptor],
    file_size: u64,
) -> Result<Vec<Vec<ByteRange>>> {
    let mut result = Vec::with_capacity(descriptors.len());
    for descriptor in descriptors {
        reader.seek(descriptor.index_offset)?;
        let count = descriptor
            .tile_count()
            .context("DMetrix tile count overflow")? as usize;
        let mut tiles = vec![ByteRange::EMPTY; count];
        let mut seen = vec![false; count];
        for _ in 0..count {
            let record = read_tile_record(reader)?;
            if record.source_id != descriptor.source_id || !descriptor.contains(record.x, record.y)
            {
                bail!(
                    "invalid DMetrix tile record in level {}",
                    descriptor.source_id
                );
            }
            record.data.validate(file_size, "DMetrix level tile")?;
            let slot = descriptor.slot(record.x, record.y);
            if seen[slot] {
                bail!(
                    "duplicate DMetrix tile coordinate ({}, {})",
                    record.x,
                    record.y
                );
            }
            seen[slot] = true;
            tiles[slot] = record.data;
        }
        if seen.iter().any(|value| !value) {
            bail!("DMetrix tile count mismatch");
        }
        result.push(tiles);
    }
    Ok(result)
}

/// One fixed-size tile index entry.
#[derive(Clone, Copy, Debug)]
struct TileRecord {
    source_id: u16,
    x: u32,
    y: u32,
    data: ByteRange,
}

/// Reads a single [`TILE_RECORD_SIZE`]-byte tile index entry.
fn read_tile_record(reader: &mut Reader) -> Result<TileRecord> {
    Ok(TileRecord {
        source_id: reader.u16()?,
        x: reader.u32()?,
        y: reader.u32()?,
        data: ByteRange {
            offset: reader.u64()?,
            length: reader.u32()? as u64,
        },
    })
}

/// Reads the first tile of the smallest level, whose grid is guaranteed to be
/// fully populated, to learn the tile edge length.
fn discover_tile_size(reader: &mut Reader, range: ByteRange) -> Result<u32> {
    let image = decode_image(&reader.range(range.offset, range.length, "DMetrix tile")?)?;
    if image.width() != image.height() || !(MIN_TILE_SIZE..=MAX_TILE_SIZE).contains(&image.width())
    {
        bail!(
            "unsupported DMetrix tile size: {}x{}",
            image.width(),
            image.height()
        );
    }
    Ok(image.width())
}

/// Source JPEG quality for the given tile, when it can be determined.
///
/// DMetrix stores plain JPEG tiles with no quality field, and the reference
/// Python implementation derives the value from the quantization tables. Until
/// that is ported, JPEG tiles report [`DEFAULT_JPEG_QUALITY`] and non-JPEG
/// payloads report `None` so the caller can apply its own default.
fn estimate_quality(reader: &mut Reader, range: ByteRange) -> Option<u8> {
    let data = reader
        .range(range.offset, range.length, "DMetrix JPEG")
        .ok()?;
    is_jpeg(&data).then_some(DEFAULT_JPEG_QUALITY)
}

/// Whether `data` starts with the JPEG start-of-image marker.
fn is_jpeg(data: &[u8]) -> bool {
    data.starts_with(&SOI_MARKER)
}

/// First tile of the smallest pyramid level.
fn first_tile(raw_levels: &[Vec<ByteRange>]) -> Result<ByteRange> {
    raw_levels
        .last()
        .and_then(|tiles| tiles.first())
        .copied()
        .context("missing DMetrix levels")
}

fn read_u32_at(reader: &mut Reader, offset: u64) -> Result<u32> {
    reader.seek(offset)?;
    reader.u32()
}

fn read_f64_at(reader: &mut Reader, offset: u64) -> Result<f64> {
    reader.seek(offset)?;
    reader.f64()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn tile_record_consumes_the_declared_record_size() -> Result<()> {
        let path =
            std::env::temp_dir().join(format!("img2svs-dmetrix-record-{}.bin", std::process::id()));
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&7u16.to_le_bytes());
        bytes.extend_from_slice(&3u32.to_le_bytes());
        bytes.extend_from_slice(&4u32.to_le_bytes());
        bytes.extend_from_slice(&0x1000u64.to_le_bytes());
        bytes.extend_from_slice(&0x200u32.to_le_bytes());
        assert_eq!(bytes.len() as u64, TILE_RECORD_SIZE);
        fs::write(&path, &bytes)?;

        let mut reader = Reader::open(&path)?;
        let record = read_tile_record(&mut reader)?;
        assert_eq!(record.source_id, 7);
        assert_eq!((record.x, record.y), (3, 4));
        assert_eq!(record.data.offset, 0x1000);
        assert_eq!(record.data.length, 0x200);
        assert_eq!(reader.len(), TILE_RECORD_SIZE);

        drop(reader);
        fs::remove_file(path)?;
        Ok(())
    }

    #[test]
    fn jpeg_marker_detection_requires_the_full_soi() {
        assert!(is_jpeg(&[0xff, 0xd8, 0xff, 0xe0]));
        assert!(!is_jpeg(&[0xff]));
        assert!(!is_jpeg(b"\x89PNG"));
    }
}
