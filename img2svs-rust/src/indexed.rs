//! Rust readers for the vendor containers that store JPEG tiles in an index.
//! The layouts mirror the validated Python readers, but all data stays on disk
//! and is consumed by the common SVS writer through byte ranges.
//!
//! The BKIO container is shared by Motic's `.mdsx`, `.msdx` and `.mdss` slides,
//! so [`parse`] routes all three extensions to the same reader.

use crate::binary::{le_at, Reader};
use crate::model::{
    assign_tile_groups, AssociatedImage, ByteRange, Compression, Level, Metadata, Slide, Thumbnail,
    TilePlacement,
};
use anyhow::{bail, Context, Result};
use base64::Engine;
use quick_xml::events::Event;
use quick_xml::Reader as XmlReader;
use std::collections::HashMap;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::str::FromStr;

/// Signature at the start of a CSP file.
const CSP_SIGNATURE: &[u8] = b"MEDIC";
/// Fixed header read from the start of a CSP file.
const CSP_HEADER_SIZE: usize = 4096;
/// Header field holding the offset of the tail metadata block.
const CSP_TAIL_OFFSET_FIELD: usize = 0x1e;
/// Marker that locates the first JPEG tile payload.
const CSP_JPEG_STREAM_MARKER: &[u8] = b"\xff\xd8\xff\xe0";
/// Fixed 22-byte marker repeated at the start of every tile record in the
/// tail. Its fields are constant across CSP slides, so it is matched verbatim
/// to locate records rather than decoded.
const CSP_TILE_RECORD_MARKER: &[u8] =
    b"\x02\x00\x25\x00\x0f\x00\x07\x00\x00\x00\x00\x00\x00\x00\x24\x00\x00\x00\x00\x00\x00\x00";
/// Length of [`CSP_TILE_RECORD_MARKER`], i.e. the offset from a marker to the
/// first payload field.
const CSP_RECORD_HEADER_SIZE: usize = 22;
/// Payload bytes of a tile record. Header plus payload is exactly
/// [`CSP_RECORD_STRIDE`], so records follow each other without padding.
const CSP_RECORD_PAYLOAD_SIZE: usize = 36;
/// Distance between two consecutive records.
const CSP_RECORD_STRIDE: usize = CSP_RECORD_HEADER_SIZE + CSP_RECORD_PAYLOAD_SIZE;
/// Field offsets inside a CSP tile record.
const CSP_FIELD_TILE_WIDTH: usize = 0;
const CSP_FIELD_TILE_HEIGHT: usize = 4;
const CSP_FIELD_DATA_OFFSET: usize = 8;
const CSP_FIELD_DATA_LENGTH: usize = 16;
const CSP_FIELD_TILE_X: usize = 24;
const CSP_FIELD_TILE_Y: usize = 28;
/// Tile edge length of the CSP level grid.
const CSP_TILE_SIZE: u32 = 256;
/// Tail metadata items that carry the micrometres-per-pixel and objective,
/// addressed as `(group, item)` in the tail's item table.
const CSP_ITEM_MPP: (u16, u16) = (4, 10);
const CSP_ITEM_APP_MAG: (u16, u16) = (4, 9);
/// Records scanned for associated images, i.e. the number of items in a record.
const CSP_ASSOCIATED_SCAN_SIZE: usize = 220;
/// Item type code of a `f32` valued CSP metadata entry.
const CSP_TAIL_ITEM_TYPE_FLOAT: u16 = 9;
/// Version prefix of the KFB block.
const KFB_VERSION_PREFIX: &[u8] = b"KFB";
/// Compression prefix of the KFB block.
const KFB_COMPRESSION_PREFIX: &[u8] = b"JP";
/// Distance between the level identifiers of two adjacent KFB levels; a tile
/// identifier encodes `level * KFB_LEVEL_STEP` on top of the base identifier.
const KFB_LEVEL_STEP: i32 = 8_388_608;
/// Fixed sizes inside a KFB embedded image entry, in file order.
const KFB_EMBEDDED_HEADER_SIZE: usize = 8;
const KFB_EMBEDDED_DIMENSION_SIZE: usize = 4;
const KFB_EMBEDDED_RESERVED_SIZE: usize = 4;
const KFB_EMBEDDED_LENGTH_SIZE: usize = 4;
const KFB_EMBEDDED_TAIL_SIZE: usize = 28;
/// Offset of the payload inside a KFB embedded image entry.
const KFB_EMBEDDED_DATA_OFFSET: usize = KFB_EMBEDDED_HEADER_SIZE
    + KFB_EMBEDDED_DIMENSION_SIZE * 2
    + KFB_EMBEDDED_RESERVED_SIZE
    + KFB_EMBEDDED_LENGTH_SIZE
    + KFB_EMBEDDED_TAIL_SIZE;
/// Fixed gaps inside a KFB tile entry.
const KFB_TILE_LEADING_RESERVED_SIZE: usize = 4;
const KFB_TILE_MID_RESERVED_SIZE: usize = 8;
const KFB_TILE_TAIL_SIZE: usize = 20;
/// Magic of the BKIO container used by `.mdsx`, `.msdx` and `.mdss`.
const MDSX_MAGIC: &[u8] = b"BKIO";
/// Offset and entry count of the block offset table.
const MDSX_BLOCK_TABLE_OFFSET: u64 = 84;
const MDSX_BLOCK_COUNT: usize = 5;
/// Fixed sizes of a block offset table entry.
const MDSX_BLOCK_HEADER_SIZE: usize = 8;
const MDSX_BLOCK_TAIL_SIZE: usize = 4;
/// Bytes between the first block offset and its XML range table.
const MDSX_FIRST_BLOCK_HEADER_SIZE: u64 = 20;
/// Offset of the level index table and the size of one entry.
const MDSX_LEVEL_TABLE_OFFSET: u64 = 164;
const MDSX_LEVEL_ENTRY_SIZE: u64 = 16;
const MDSX_LEVEL_ENTRY_HEADER_SIZE: usize = 8;
/// Header preceding a level's tile index and the width of one tile record.
const MDSX_TILE_COUNT_HEADER_SIZE: u64 = 4;
const MDSX_TILE_RECORD_SIZE: u64 = 10;
const MDSX_TILE_RESERVED_SIZE: usize = 2;
/// Bytes skipped by a tagged XML range before its offset and length.
const MDSX_TAG_SIZE: usize = 6;
/// Bytes inspected to detect UTF-16 encoded XML text.
const UTF16_BOM_LENGTH: usize = 2;
/// Maximum number of associated images kept from a CSP file.
const CSP_ASSOCIATED_LIMIT: usize = 3;

pub fn parse(path: &Path) -> Result<Slide> {
    match path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "csp" => parse_csp(path),
        "kfb" => parse_kfb(path),
        "mdsx" | "msdx" | "mdss" => parse_mdsx(path),
        extension => bail!("unsupported indexed slide extension: .{extension}"),
    }
}

/// The three regions of a CSP file that parsing needs.
struct CspSections {
    header: Vec<u8>,
    tail: Vec<u8>,
    stream_start: u64,
}

/// One level described by the tail metadata.
struct CspLevel {
    width: u32,
    height: u32,
    cols: u32,
    rows: u32,
    tiles: Vec<ByteRange>,
}

fn parse_csp(path: &Path) -> Result<Slide> {
    let file_size = fs::metadata(path)?.len();
    let sections = read_csp_sections(path, file_size)?;
    let levels = build_csp_levels(&sections.tail, sections.stream_start, file_size)?;
    let top = levels
        .first()
        .context("CSP file contains no pyramid levels")?;
    let associated = parse_csp_associated(&sections.header, sections.stream_start, file_size)?;
    Ok(Slide {
        path: PathBuf::from(path),
        metadata: Metadata {
            width: top.width,
            height: top.height,
            mpp: csp_float(&sections.tail, CSP_ITEM_MPP.0, CSP_ITEM_MPP.1).unwrap_or(0.25),
            app_mag: csp_float(&sections.tail, CSP_ITEM_APP_MAG.0, CSP_ITEM_APP_MAG.1)
                .unwrap_or(40.0),
            jpeg_quality: 75,
        },
        tile_width: CSP_TILE_SIZE,
        tile_height: CSP_TILE_SIZE,
        compression: Compression::Jpeg,
        levels,
        associated_images: associated,
        thumbnail: None,
        sources: Vec::new(),
    })
}

/// Splits a CSP file into its header, its tail metadata and the offset of the
/// JPEG tile stream that the tail points into.
fn read_csp_sections(path: &Path, file_size: u64) -> Result<CspSections> {
    let mut file = std::fs::File::open(path)?;
    let mut header = vec![0; CSP_HEADER_SIZE];
    file.read_exact(&mut header).context("read CSP header")?;
    if !header.starts_with(CSP_SIGNATURE) {
        bail!("unsupported CSP signature");
    }
    let stream_start = header
        .windows(CSP_JPEG_STREAM_MARKER.len())
        .position(|window| window == CSP_JPEG_STREAM_MARKER)
        .context("could not locate CSP JPEG stream start")? as u64;
    let tail_start = le_at::<u64>(&header, CSP_TAIL_OFFSET_FIELD, "CSP header")
        .context("CSP header is too small")?;
    if tail_start == 0 || tail_start >= file_size {
        bail!("invalid CSP tail offset");
    }
    file.seek(SeekFrom::Start(tail_start))?;
    let mut tail = Vec::new();
    file.read_to_end(&mut tail)?;
    Ok(CspSections {
        header,
        tail,
        stream_start,
    })
}

/// Builds every pyramid level from the tile records in the tail metadata.
fn build_csp_levels(tail: &[u8], stream_start: u64, file_size: u64) -> Result<Vec<Level>> {
    let positions = find_all(tail, CSP_TILE_RECORD_MARKER);
    if positions.is_empty() {
        bail!("no CSP tile records were found in tail metadata");
    }

    let boundaries = csp_level_boundaries(&positions);
    let mut levels = Vec::with_capacity(boundaries.len() - 1);
    let mut full_width = 0u32;
    for (block, window) in boundaries.windows(2).enumerate() {
        let level = read_csp_level(
            tail,
            &positions[window[0]..window[1]],
            block,
            stream_start,
            file_size,
        )?;
        if block == 0 {
            full_width = level.width;
        }
        levels.push(Level {
            index: block,
            width: level.width,
            height: level.height,
            downsample: f64::from(full_width.max(1)) / f64::from(level.width),
            tile_cols: level.cols,
            tile_rows: level.rows,
            tiles: level.tiles,
            tile_positions: Vec::new(),
            tile_groups: Vec::new(),
            tiling: Default::default(),
        });
    }
    Ok(levels)
}

/// Splits the tile record positions into one run per level.
///
/// Records belonging to the same level sit exactly [`CSP_RECORD_STRIDE`] bytes
/// apart; the first record that breaks the stride starts the next level.
fn csp_level_boundaries(positions: &[usize]) -> Vec<usize> {
    let mut boundaries = vec![0usize];
    for (index, pair) in positions.windows(2).enumerate() {
        if pair[1] - pair[0] != CSP_RECORD_STRIDE {
            boundaries.push(index + 1);
        }
    }
    boundaries.push(positions.len());
    boundaries
}

/// Reads one level's records into a dense [`CSP_TILE_SIZE`] square tile grid.
///
/// Records are sparse and may be stored in any order, so each one is placed by
/// its own grid coordinate and missing cells are left as [`ByteRange::EMPTY`].
fn read_csp_level(
    tail: &[u8],
    records: &[usize],
    block: usize,
    stream_start: u64,
    file_size: u64,
) -> Result<CspLevel> {
    let mut entries = HashMap::new();
    let mut width = 0u32;
    let mut height = 0u32;
    for position in records {
        let record = position + CSP_RECORD_HEADER_SIZE;
        if record + CSP_RECORD_PAYLOAD_SIZE > tail.len() {
            bail!("truncated CSP tile record");
        }
        let tile_width = le_at::<u32>(tail, record + CSP_FIELD_TILE_WIDTH, "CSP tile record")?;
        let tile_height = le_at::<u32>(tail, record + CSP_FIELD_TILE_HEIGHT, "CSP tile record")?;
        let data = ByteRange {
            offset: stream_start
                + u64::from(le_at::<u32>(
                    tail,
                    record + CSP_FIELD_DATA_OFFSET,
                    "CSP tile record",
                )?),
            length: u64::from(le_at::<u32>(
                tail,
                record + CSP_FIELD_DATA_LENGTH,
                "CSP tile record",
            )?),
        };
        let x = le_at::<u32>(tail, record + CSP_FIELD_TILE_X, "CSP tile record")?;
        let y = le_at::<u32>(tail, record + CSP_FIELD_TILE_Y, "CSP tile record")?;
        data.validate(file_size, "CSP tile")?;
        entries.entry((x, y)).or_insert(data);
        width = width.max(x.checked_add(tile_width).context("CSP width overflow")?);
        height = height.max(y.checked_add(tile_height).context("CSP height overflow")?);
    }
    if width == 0 || height == 0 {
        bail!("invalid CSP level dimensions at block {block}");
    }

    let cols = width.div_ceil(CSP_TILE_SIZE);
    let rows = height.div_ceil(CSP_TILE_SIZE);
    let mut tiles = Vec::with_capacity((cols * rows) as usize);
    for row in 0..rows {
        for col in 0..cols {
            let cell = (col * CSP_TILE_SIZE, row * CSP_TILE_SIZE);
            tiles.push(entries.get(&cell).copied().unwrap_or(ByteRange::EMPTY));
        }
    }
    Ok(CspLevel {
        width,
        height,
        cols,
        rows,
        tiles,
    })
}

/// Reads the label and macro images referenced by the header records.
///
/// NOTE: [`csp_scalar`] never resolves a field today, so this yields no
/// associated images. That matches the validated Python reader and is kept
/// deliberately; see [`csp_scalar`] for the reason.
fn parse_csp_associated(
    header: &[u8],
    stream_start: u64,
    file_size: u64,
) -> Result<Vec<AssociatedImage>> {
    const RECORD_MARKER: &[u8] = b"\x02\x00\x01\x00\x0e\x00";
    let mut images = Vec::new();
    for start in find_all(header, RECORD_MARKER) {
        let end = (start + CSP_ASSOCIATED_SCAN_SIZE).min(header.len());
        let width = csp_scalar(header, &csp_pattern(2, 3, 5, 4), start, end).unwrap_or(0) as u32;
        let height = csp_scalar(header, &csp_pattern(2, 4, 5, 4), start, end).unwrap_or(0) as u32;
        let offset = csp_scalar(header, &csp_pattern(2, 5, 7, 8), start, end).unwrap_or(0);
        let length = csp_scalar(header, &csp_pattern(2, 6, 7, 8), start, end).unwrap_or(0);
        if width == 0 || height == 0 || length == 0 {
            continue;
        }
        let data = ByteRange {
            offset: stream_start + offset,
            length,
        };
        data.validate(file_size, "CSP associated image")?;
        images.push((width, height, data));
        if images.len() == CSP_ASSOCIATED_LIMIT {
            break;
        }
    }
    // Smallest first, so the label sorts ahead of the macro image.
    images.sort_by_key(|(width, height, _)| u64::from(*width) * u64::from(*height));
    Ok(images
        .into_iter()
        .enumerate()
        .map(|(index, (_, _, data))| AssociatedImage {
            kind: if index == 0 { "label" } else { "macro" }.to_owned(),
            data,
            ..Default::default()
        })
        .collect())
}

fn csp_pattern(group: u16, item: u16, type_code: u16, payload_size: u32) -> Vec<u8> {
    let mut result = Vec::with_capacity(22);
    result.extend_from_slice(&group.to_le_bytes());
    result.extend_from_slice(&item.to_le_bytes());
    result.extend_from_slice(&type_code.to_le_bytes());
    result.extend_from_slice(&1u16.to_le_bytes());
    result.extend_from_slice(&[0; 6]);
    result.extend_from_slice(&payload_size.to_le_bytes());
    result.extend_from_slice(&[0; 4]);
    result
}

/// Reads a `f32` field of the CSP tail metadata, such as scale or objective.
fn csp_float(data: &[u8], group: u16, item: u16) -> Option<f64> {
    let pattern = csp_pattern(group, item, CSP_TAIL_ITEM_TYPE_FLOAT, 4);
    let position = find_all(data, &pattern).first().copied()? + CSP_RECORD_HEADER_SIZE;
    Some(f64::from(f32::from_le_bytes(
        data.get(position..position + 4)?.try_into().ok()?,
    )))
}

/// Reads a scalar field out of a CSP header record.
///
/// NOTE: this returns `None` for every input at the moment. [`csp_pattern`]
/// always builds a 22-byte pattern, while the arms below test for 26 and 30, so
/// the `_` arm always wins. The mismatch predates this refactor and is left
/// untouched on purpose: fixing it would start reporting associated images that
/// the tool has never produced, which is a behaviour change rather than a
/// cleanup.
fn csp_scalar(data: &[u8], pattern: &[u8], start: usize, end: usize) -> Option<u64> {
    let position = find_subslice(&data[start..end], pattern)? + start + CSP_RECORD_HEADER_SIZE;
    match pattern.len() {
        26 => Some(u64::from(u32::from_le_bytes(
            data.get(position..position + 4)?.try_into().ok()?,
        ))),
        30 => Some(u64::from_le_bytes(
            data.get(position..position + 8)?.try_into().ok()?,
        )),
        _ => None,
    }
}

/// Header fields of a KFB file that parsing needs.
struct KfbHeader {
    tile_count: i32,
    base_width: i32,
    base_height: i32,
    scan_scale: f64,
    mpp: f64,
    tile_size: i32,
    macro_offset: u64,
    label_offset: u64,
    preview_offset: u64,
    tiles_offset: u64,
}

/// An image embedded in a KFB file, located by an absolute offset.
struct EmbeddedImage {
    width: u32,
    height: u32,
    data: ByteRange,
}

fn parse_kfb(path: &Path) -> Result<Slide> {
    let mut reader = Reader::open(path)?;
    let file_size = reader.len();
    let header = read_kfb_header(&mut reader)?;
    let mut levels = build_kfb_levels(&header);
    read_kfb_tiles(&mut reader, &header, &mut levels, file_size)?;
    assign_tile_groups(
        &mut levels,
        header.tile_size as u32,
        header.tile_size as u32,
    );
    let associated = read_kfb_associated(&mut reader, &header, file_size)?;
    let thumbnail = if header.preview_offset == 0 {
        None
    } else {
        Some(read_kfb_thumbnail(
            &mut reader,
            header.preview_offset,
            file_size,
        )?)
    };
    Ok(Slide {
        path: PathBuf::from(path),
        metadata: Metadata {
            width: header.base_width as u32,
            height: header.base_height as u32,
            mpp: header.mpp,
            app_mag: header.scan_scale,
            jpeg_quality: 75,
        },
        tile_width: header.tile_size as u32,
        tile_height: header.tile_size as u32,
        compression: Compression::Jpeg,
        levels,
        associated_images: associated,
        thumbnail,
        sources: Vec::new(),
    })
}

/// Reads and validates the KFB header.
fn read_kfb_header(reader: &mut Reader) -> Result<KfbHeader> {
    reader.seek(4)?;
    if !reader
        .bytes(4, "KFB version")?
        .starts_with(KFB_VERSION_PREFIX)
    {
        bail!("unsupported KFB signature");
    }
    reader.skip(8, "KFB header reserved")?;
    let tile_count = reader.i32()?;
    let base_height = reader.i32()?;
    let base_width = reader.i32()?;
    let scan_scale = f64::from(reader.i32()?);
    if !reader
        .bytes(4, "KFB compression")?
        .starts_with(KFB_COMPRESSION_PREFIX)
    {
        bail!("unsupported KFB compression");
    }
    reader.skip(4, "KFB compression reserved")?;
    reader.skip(4, "KFB scan duration")?;
    reader.skip(8, "KFB scan time")?;
    let macro_offset = u64::from(reader.u32()?);
    let label_offset = u64::from(reader.u32()?);
    let preview_offset = reader.u64()?;
    let tiles_offset = reader.u64()?;
    let mpp = f64::from(reader.f32()?);
    reader.skip(8, "KFB resolution reserved")?;
    let tile_size = reader.i32()?;
    if tile_count < 0 || base_width <= 0 || base_height <= 0 || tile_size <= 0 || mpp <= 0.0 {
        bail!("invalid KFB header");
    }
    Ok(KfbHeader {
        tile_count,
        base_width,
        base_height,
        scan_scale,
        mpp,
        tile_size,
        macro_offset,
        label_offset,
        preview_offset,
        tiles_offset,
    })
}

/// Builds the empty pyramid skeleton; tiles are attached afterwards.
///
/// The level count is derived from the base image size rather than stored.
fn build_kfb_levels(header: &KfbHeader) -> Vec<Level> {
    let longest_edge = f64::from(header.base_width.max(header.base_height));
    let zoom_levels = longest_edge.log2().ceil() as usize + 1;
    let tile_size = header.tile_size as u32;
    (0..zoom_levels)
        .map(|index| {
            let downsample = 1u32 << index.min(31);
            let width = (header.base_width as u32 / downsample).max(1);
            let height = (header.base_height as u32 / downsample).max(1);
            Level {
                index,
                width,
                height,
                downsample: f64::from(downsample),
                tile_cols: width.div_ceil(tile_size),
                tile_rows: height.div_ceil(tile_size),
                tiles: Vec::new(),
                tile_positions: Vec::new(),
                tile_groups: Vec::new(),
                tiling: Default::default(),
            }
        })
        .collect()
}

/// Attaches every tile entry to the level its identifier points at.
fn read_kfb_tiles(
    reader: &mut Reader,
    header: &KfbHeader,
    levels: &mut [Level],
    file_size: u64,
) -> Result<()> {
    reader.seek(header.tiles_offset)?;
    let mut base_level_id = None;
    for _ in 0..header.tile_count {
        reader.skip(KFB_TILE_LEADING_RESERVED_SIZE, "KFB tile reserved")?;
        let x = reader.i32()?;
        let y = reader.i32()?;
        let width = reader.i32()?;
        let height = reader.i32()?;
        let tile_id = reader.i32()?;
        let base = *base_level_id.get_or_insert(tile_id);
        let delta = base - tile_id;
        if delta < 0 || delta % KFB_LEVEL_STEP != 0 {
            bail!("invalid KFB level id mapping");
        }
        let index = (delta / KFB_LEVEL_STEP) as usize;
        if index >= levels.len() || x < 0 || y < 0 || width <= 0 || height <= 0 {
            bail!("invalid KFB tile entry");
        }
        reader.skip(KFB_TILE_MID_RESERVED_SIZE, "KFB tile reserved")?;
        let length = reader.i32()?;
        // Offsets in the tile table are relative to the start of the tile
        // block, so a negative one points before the file even begins.
        let relative = reader.i64()?;
        reader.skip(KFB_TILE_TAIL_SIZE, "KFB tile tail")?;
        if length < 0 {
            bail!("invalid KFB tile range");
        }
        let absolute = i128::from(header.tiles_offset) + i128::from(relative);
        if absolute < 0 || absolute > i128::from(u64::MAX) {
            bail!("invalid KFB tile offset");
        }
        let data = ByteRange {
            offset: u64::try_from(absolute).context("KFB tile offset out of range")?,
            length: length as u64,
        };
        data.validate(file_size, "KFB tile")?;
        levels[index].tiles.push(data);
        levels[index].tile_positions.push(TilePlacement {
            x: x as u32,
            y: y as u32,
            width: width as u32,
            height: height as u32,
            src_x: 0,
            src_y: 0,
        });
    }
    Ok(())
}

/// Reads the macro and label images referenced by the header offsets.
fn read_kfb_associated(
    reader: &mut Reader,
    header: &KfbHeader,
    file_size: u64,
) -> Result<Vec<AssociatedImage>> {
    [
        ("macro", header.macro_offset),
        ("label", header.label_offset),
    ]
    .into_iter()
    .filter(|(_, offset)| *offset != 0)
    .map(|(kind, offset)| read_kfb_image(reader, offset, file_size, kind))
    .collect()
}

/// Reads an associated image, whose size the SVS writer does not need.
fn read_kfb_image(
    reader: &mut Reader,
    offset: u64,
    file_size: u64,
    kind: &str,
) -> Result<AssociatedImage> {
    let image = read_kfb_embedded(reader, offset, file_size)?;
    Ok(AssociatedImage {
        kind: kind.to_owned(),
        data: image.data,
        ..Default::default()
    })
}

/// Reads the preview image stored ahead of the tile stream.
fn read_kfb_thumbnail(reader: &mut Reader, offset: u64, file_size: u64) -> Result<Thumbnail> {
    let image = read_kfb_embedded(reader, offset, file_size)?;
    Ok(Thumbnail {
        width: image.width,
        height: image.height,
        data: image.data,
        ..Default::default()
    })
}

/// Reads the dimensions and payload range of an embedded KFB image.
fn read_kfb_embedded(reader: &mut Reader, offset: u64, file_size: u64) -> Result<EmbeddedImage> {
    reader.seek(offset)?;
    reader.skip(KFB_EMBEDDED_HEADER_SIZE, "KFB embedded header")?;
    let height = reader.i32()?;
    let width = reader.i32()?;
    reader.skip(KFB_EMBEDDED_RESERVED_SIZE, "KFB embedded reserved")?;
    let length = reader.i32()?;
    reader.skip(KFB_EMBEDDED_TAIL_SIZE, "KFB embedded tail")?;
    if width <= 0 || height <= 0 || length <= 0 {
        bail!("invalid KFB embedded image entry");
    }
    let data = ByteRange {
        offset: offset + KFB_EMBEDDED_DATA_OFFSET as u64,
        length: length as u64,
    };
    data.validate(file_size, "KFB embedded image")?;
    Ok(EmbeddedImage {
        width: width as u32,
        height: height as u32,
        data,
    })
}

/// Byte ranges of the XML sections referenced by the MDSX block table.
struct MdsxSections {
    property: ByteRange,
    macro_section: ByteRange,
    label: ByteRange,
    slide: ByteRange,
}

/// Reads a Motic BKIO slide (`.mdsx`, `.msdx` or `.mdss`).
///
/// The three extensions differ only in their name: the container, the UTF-16 /
/// Base64 XML sections, the pyramid tile index and the label/macro JPEGs are
/// byte-for-byte the same layout.
fn parse_mdsx(path: &Path) -> Result<Slide> {
    let mut reader = Reader::open(path)?;
    if reader.bytes(4, "MDSX magic")? != MDSX_MAGIC {
        bail!("unsupported MDSX container");
    }
    let blocks = read_mdsx_block_table(&mut reader)?;
    let sections = read_mdsx_sections(&mut reader, blocks[0])?;
    let property_values = xml_values(&decode_mdsx_xml(&reader.range(
        sections.property.offset,
        sections.property.length,
        "MDSX property XML",
    )?)?)?;
    let matrix = parse_matrix(&decode_mdsx_xml(&reader.range(
        sections.slide.offset,
        sections.slide.length,
        "MDSX slide XML",
    )?)?)?;
    if matrix.tile_width != matrix.tile_height {
        bail!("unsupported non-square MDSX tile size");
    }
    let levels = read_mdsx_levels(&mut reader, &matrix)?;
    let (mpp, app_mag, jpeg_quality) = read_mdsx_metadata(path, &property_values)?;
    let associated_images = [("label", sections.label), ("macro", sections.macro_section)]
        .into_iter()
        .filter(|(_, data)| data.present())
        .map(|(kind, data)| AssociatedImage {
            kind: kind.to_owned(),
            data,
            ..Default::default()
        })
        .collect();
    Ok(Slide {
        path: PathBuf::from(path),
        metadata: Metadata {
            width: matrix.width,
            height: matrix.height,
            mpp,
            app_mag,
            jpeg_quality,
        },
        tile_width: matrix.tile_width,
        tile_height: matrix.tile_width,
        compression: Compression::Jpeg,
        levels,
        associated_images,
        thumbnail: None,
        sources: Vec::new(),
    })
}

/// Reads the fixed block offset table, of which only the first entry is used.
fn read_mdsx_block_table(reader: &mut Reader) -> Result<[u64; MDSX_BLOCK_COUNT]> {
    reader.seek(MDSX_BLOCK_TABLE_OFFSET)?;
    let mut offsets = [0u64; MDSX_BLOCK_COUNT];
    for offset in &mut offsets {
        reader.skip(MDSX_BLOCK_HEADER_SIZE, "MDSX block header")?;
        *offset = u64::from(reader.u32()?);
        reader.skip(MDSX_BLOCK_TAIL_SIZE, "MDSX block tail")?;
    }
    Ok(offsets)
}

/// Reads the XML section ranges, which are stored back to back.
fn read_mdsx_sections(reader: &mut Reader, first_block: u64) -> Result<MdsxSections> {
    reader.seek(first_block + MDSX_FIRST_BLOCK_HEADER_SIZE)?;
    Ok(MdsxSections {
        property: read_mdsx_range(reader)?,
        macro_section: read_mdsx_tagged_range(reader)?,
        label: read_mdsx_tagged_range(reader)?,
        slide: read_mdsx_tagged_range(reader)?,
    })
}

/// Reads every level's tile index from the level table.
fn read_mdsx_levels(reader: &mut Reader, matrix: &Matrix) -> Result<Vec<Level>> {
    let mut levels = Vec::with_capacity(matrix.layer_count);
    for index in 0..matrix.layer_count {
        let (rows, cols) = matrix
            .layers
            .get(index)
            .copied()
            .context("missing MDSX layer")?;
        let divisor = 1u32 << index.min(31);
        reader.seek(MDSX_LEVEL_TABLE_OFFSET + index as u64 * MDSX_LEVEL_ENTRY_SIZE)?;
        reader.skip(MDSX_LEVEL_ENTRY_HEADER_SIZE, "MDSX level index header")?;
        let tiles_offset = u64::from(reader.u32()?);
        let tiles_length = u64::from(reader.u32()?);
        if tiles_length < MDSX_TILE_COUNT_HEADER_SIZE {
            bail!("invalid MDSX tile index length");
        }
        let count = (tiles_length - MDSX_TILE_COUNT_HEADER_SIZE) / MDSX_TILE_RECORD_SIZE;
        if count != u64::from(rows) * u64::from(cols) {
            bail!("MDSX tile count mismatch at level {index}");
        }
        reader.seek(tiles_offset + MDSX_TILE_COUNT_HEADER_SIZE)?;
        let mut tiles = Vec::with_capacity(count as usize);
        for _ in 0..count {
            reader.skip(MDSX_TILE_RESERVED_SIZE, "MDSX tile reserved")?;
            let data = ByteRange {
                offset: u64::from(reader.u32()?),
                length: u64::from(reader.u32()?),
            };
            data.validate(reader.len(), "MDSX tile")?;
            tiles.push(data);
        }
        levels.push(Level {
            index,
            width: matrix.width.div_ceil(divisor).max(1),
            height: matrix.height.div_ceil(divisor).max(1),
            downsample: 2f64.powi(index as i32),
            tile_cols: cols,
            tile_rows: rows,
            tiles,
            tile_positions: Vec::new(),
            tile_groups: Vec::new(),
            tiling: Default::default(),
        });
    }
    Ok(levels)
}

/// Scan metadata: the sidecar ini files win over the embedded property XML.
fn read_mdsx_metadata(
    path: &Path,
    property_values: &HashMap<String, String>,
) -> Result<(f64, f64, u8)> {
    let base_dir = path.parent().unwrap_or_else(|| Path::new("."));
    let info = read_ini(&base_dir.join("info.ini"));
    let meta = read_ini(&base_dir.join("meta"));
    let mpp = first_parse(&[
        meta.get("property.scale"),
        info.get("info.scale"),
        property_values.get("Scale"),
    ])
    .context("missing MDSX scale")?;
    let app_mag = first_parse(&[
        meta.get("property.scanobjective"),
        info.get("info.scanlens"),
        property_values.get("ScanObjective"),
    ])
    .context("missing MDSX objective")?;
    let jpeg_quality = first_parse(&[
        meta.get("property.compressquality"),
        property_values.get("CompressQuality"),
    ])
    .unwrap_or(75)
    .clamp(1, 100) as u8;
    Ok((mpp, app_mag, jpeg_quality))
}

/// Reads an offset/length pair.
fn read_mdsx_range(reader: &mut Reader) -> Result<ByteRange> {
    Ok(ByteRange {
        offset: u64::from(reader.u32()?),
        length: u64::from(reader.u32()?),
    })
}

/// Reads an offset/length pair preceded by a six byte tag.
fn read_mdsx_tagged_range(reader: &mut Reader) -> Result<ByteRange> {
    reader.skip(MDSX_TAG_SIZE, "MDSX tag")?;
    read_mdsx_range(reader)
}

fn decode_mdsx_xml(data: &[u8]) -> Result<String> {
    if data.is_empty() {
        return Ok(String::new());
    }
    let decoded = if data.starts_with(b"<") {
        data.to_vec()
    } else {
        let compact: Vec<u8> = data
            .iter()
            .copied()
            .filter(|byte| !byte.is_ascii_whitespace() && *byte != 0)
            .collect();
        base64::engine::general_purpose::STANDARD
            .decode(compact)
            .context("decode MDSX XML base64")?
    };
    if decoded.len() >= UTF16_BOM_LENGTH && decoded[1] == 0 {
        let units: Vec<u16> = decoded
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u16::from_le_bytes(*pair))
            .collect();
        Ok(String::from_utf16_lossy(&units)
            .trim_matches('\0')
            .to_owned())
    } else {
        Ok(String::from_utf8_lossy(&decoded)
            .trim_matches('\0')
            .to_owned())
    }
}

fn xml_values(xml: &str) -> Result<HashMap<String, String>> {
    let mut values = HashMap::new();
    let mut parser = XmlReader::from_str(xml);
    parser.config_mut().trim_text(true);
    loop {
        match parser.read_event() {
            Ok(Event::Start(event)) | Ok(Event::Empty(event)) => {
                let name = String::from_utf8_lossy(event.local_name().as_ref()).to_string();
                for attribute in event.attributes().flatten() {
                    if attribute.key.as_ref() == b"value" {
                        values.insert(name.clone(), attribute.unescape_value()?.into_owned());
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(error) => bail!("parse MDSX XML: {error}"),
            _ => {}
        }
    }
    Ok(values)
}

struct Matrix {
    width: u32,
    height: u32,
    tile_width: u32,
    tile_height: u32,
    layer_count: usize,
    layers: Vec<(u32, u32)>,
}

fn parse_matrix(xml: &str) -> Result<Matrix> {
    let mut parser = XmlReader::from_str(xml);
    parser.config_mut().trim_text(true);
    let mut matrix = Matrix {
        width: 0,
        height: 0,
        tile_width: 0,
        tile_height: 0,
        layer_count: 0,
        layers: Vec::new(),
    };
    let mut in_matrix = false;
    let mut current_layer = None;
    loop {
        match parser.read_event() {
            Ok(Event::Start(event)) | Ok(Event::Empty(event)) => {
                let name = String::from_utf8_lossy(event.local_name().as_ref()).to_string();
                if name == "ImageMatrix" {
                    in_matrix = true;
                }
                if let Some(index) = name
                    .strip_prefix("Layer")
                    .and_then(|value| value.parse::<usize>().ok())
                {
                    current_layer = Some(index);
                    while matrix.layers.len() <= index {
                        matrix.layers.push((0, 0));
                    }
                }
                let value = event
                    .attributes()
                    .flatten()
                    .find(|attribute| attribute.key.as_ref() == b"value")
                    .map(|attribute| attribute.unescape_value().map(|value| value.into_owned()))
                    .transpose()?;
                let Some(value) = value else { continue };
                if !in_matrix {
                    continue;
                }
                match name.as_str() {
                    "Width" | "Height" | "CellWidth" | "CellHeight" | "LayerCount" | "Rows"
                    | "Cols" => {
                        let number = value
                            .parse::<u32>()
                            .with_context(|| format!("invalid MDSX XML value for {name}"))?;
                        match name.as_str() {
                            "Width" => matrix.width = number,
                            "Height" => matrix.height = number,
                            "CellWidth" => matrix.tile_width = number,
                            "CellHeight" => matrix.tile_height = number,
                            "LayerCount" => matrix.layer_count = number as usize,
                            "Rows" => {
                                if let Some(index) = current_layer {
                                    matrix.layers[index].0 = number
                                }
                            }
                            "Cols" => {
                                if let Some(index) = current_layer {
                                    matrix.layers[index].1 = number
                                }
                            }
                            _ => {}
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::End(event)) => {
                if event.local_name().as_ref() == b"ImageMatrix" {
                    in_matrix = false;
                } else if event.local_name().as_ref().starts_with(b"Layer") {
                    current_layer = None;
                }
            }
            Ok(Event::Eof) => break,
            Err(error) => bail!("parse MDSX slide XML: {error}"),
            _ => {}
        }
    }
    if matrix.width == 0 || matrix.height == 0 || matrix.tile_width == 0 || matrix.layer_count == 0
    {
        bail!("invalid MDSX ImageMatrix");
    }
    Ok(matrix)
}

/// Parses an ini file into `section.key` entries, both lower-cased.
///
/// MDSX ships metadata as two optional sidecars plus embedded XML, so a
/// missing or unreadable file is not an error: the caller falls through to the
/// next source and only fails if no source has the value.
fn read_ini(path: &Path) -> HashMap<String, String> {
    let Ok(text) = fs::read_to_string(path) else {
        return HashMap::new();
    };
    let mut section = String::new();
    let mut values = HashMap::new();
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') && line.ends_with(']') {
            section = line[1..line.len() - 1].to_ascii_lowercase();
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        values.insert(
            format!("{}.{}", section, key.trim().to_ascii_lowercase()),
            value.trim().to_owned(),
        );
    }
    values
}

/// First of `values` that parses as `T`.
///
/// The same number appears under several keys across the MDSX sources, so the
/// caller lists them best-first and takes the first usable one.
fn first_parse<T: FromStr>(values: &[Option<&String>]) -> Option<T> {
    values.iter().flatten().find_map(|value| value.parse().ok())
}

/// Every position of `needle` inside `data`.
fn find_all(data: &[u8], needle: &[u8]) -> Vec<usize> {
    let mut matches = Vec::new();
    let mut start = 0;
    while let Some(found) = find_subslice(&data[start..], needle) {
        start += found;
        matches.push(start);
        start += 1;
    }
    matches
}

/// First position of `needle` inside `data`.
fn find_subslice(data: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return None;
    }
    data.windows(needle.len())
        .position(|window| window == needle)
}
