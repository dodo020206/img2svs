//! Native reader for Hamamatsu NDPI files.
//!
//! An NDPI is a TIFF container, but its pyramid levels are not tiled images:
//! each level is one baseline JPEG strip covering the whole level, cut into
//! restart intervals.  Hamamatsu sizes the interval so that one interval is
//! one tile, which is why OpenSlide reports `tile-width` 3840 / `tile-height`
//! 8 for the samples at hand - 240 MCUs of 16x8 pixels.
//!
//! Because every interval begins right after the previous one reset the DC
//! predictors, an interval decodes on its own as long as the strip's header
//! is placed in front of it and an EOI behind it.  A tile is therefore a byte
//! range of the strip wrapped in a shared header, so the level is never
//! decoded as a whole and memory stays flat regardless of slide size.
//!
//! The header does need one edit: it declares the full level, and a decoder
//! handed a single interval would allocate that whole level before failing.
//! The frame size is patched down to the tile geometry, which is a fixed
//! length change and therefore computed once per level and reused by every
//! tile.
//!
//! Directories are classified the way OpenSlide's Hamamatsu reader does it:
//! tag 65421 (`NDPI_SOURCELENS`) holds the objective magnification as a float,
//! positive for a pyramid level and `-1` for the macro image.  Anything else -
//! the label page in the sample - is ignored.

use crate::jpeg::{EOI_MARKER, SOI_MARKER};
use crate::model::{
    assign_tile_groups, AssociatedImage, ByteRange, Compression, Level, Metadata, Slide,
    TileLayout, TilePlacement,
};
use crate::tiff::{self, tag, RawDirectory};
use anyhow::{bail, Context, Result};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// TIFF tag holding the `NDPI_SOURCELENS` value that classifies a directory.
const NDPI_SOURCELENS: u16 = 65421;
/// TIFF field types that can carry `NDPI_SOURCELENS` as Hamamatsu writes it.
const FIELD_FLOAT: u16 = 11;
const FIELD_DOUBLE: u16 = 12;
/// `NDPI_SOURCELENS` of the directory holding the macro image.
const MACRO_LENS: f32 = -1.0;

/// Bytes of a strip that are read to locate its frame header. A JPEG header
/// stays under 2 KiB even with full quantization and Huffman tables.
const HEADER_PROBE: u64 = 64 * 1024;
/// Bytes scanned at a time while looking for restart markers.
const SCAN_CHUNK: usize = 1 << 20;
/// Upper bound on the restart intervals of one level, so a corrupt interval
/// length cannot ask for an unbounded tile table.
const MAX_RESTART_INTERVALS: u64 = 4_000_000;

/// Quality to re-encode with when the caller does not choose one.
const DEFAULT_JPEG_QUALITY: u8 = 75;

/// Largest level that may be emitted as one whole-strip tile.
///
/// A level whose restart intervals do not line up with whole MCU rows cannot be
/// cut into tiles, so it is decoded whole instead - once for every output cell
/// it covers, which is only affordable while the level is small. Coarse levels
/// are small; a full-resolution level that did not line up would be a writer
/// bug rather than something worth converting slowly.
const WHOLE_STRIP_LIMIT_PIXELS: u64 = 4_000_000;

/// Parses an NDPI file into the format-independent model.
pub fn parse(path: &Path) -> Result<Slide> {
    let mut file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let file_size = file.metadata()?.len();
    let big = tiff::read_header(&mut file)?;
    let directories = tiff::read_chain(&mut file, big, file_size)?;
    if directories.is_empty() {
        bail!("NDPI contains no TIFF directories");
    }

    let mut pyramid = Vec::new();
    let mut macro_image = None;
    for raw in &directories {
        let Some(lens) = source_lens(&mut file, raw, big)? else {
            continue;
        };
        if lens == MACRO_LENS {
            macro_image = Some(macro_image_from(&mut file, raw, big, file_size)?);
            continue;
        }
        if lens <= 0.0 {
            // Neither a level nor the macro image: the label page and the
            // other auxiliary directories NDPI stores beside the pyramid.
            continue;
        }
        pyramid.push(Page {
            raw,
            lens,
            width: dimension(&mut file, raw, tag::IMAGE_WIDTH, big)?,
            height: dimension(&mut file, raw, tag::IMAGE_LENGTH, big)?,
        });
    }
    if pyramid.is_empty() {
        bail!("NDPI contains no pyramid levels");
    }
    // The finest level is the largest one. NDPI writes the pyramid finest
    // first, but ordering by area keeps that true for files that do not.
    pyramid.sort_by_key(|page| std::cmp::Reverse(u64::from(page.width) * u64::from(page.height)));

    let level0_width = pyramid[0].width;
    let level0_height = pyramid[0].height;
    let context = LevelContext {
        file_size,
        big,
        level0_width,
    };
    let finest = read_level(&mut file, &pyramid[0], 0, &context, None)?;
    // Every other level is written on the pitch the finest level established;
    // a level that cannot be cut into tiles uses it to place its single tile.
    if finest.tiling.tile_width == 0 || finest.tiling.tile_height == 0 {
        bail!("NDPI finest level is not cut into restart intervals this reader can address");
    }
    let pitch = (finest.tiling.tile_width, finest.tiling.tile_height);
    let mut levels = Vec::with_capacity(pyramid.len());
    levels.push(finest);
    for (index, page) in pyramid.iter().enumerate().skip(1) {
        levels.push(read_level(&mut file, page, index, &context, Some(pitch))?);
    }
    assign_tile_groups(&mut levels, pitch.0, pitch.1);

    Ok(Slide {
        path: PathBuf::from(path),
        metadata: Metadata {
            width: level0_width,
            height: level0_height,
            mpp: physical_resolution(&mut file, &pyramid[0], &context).unwrap_or(0.25),
            // The objective magnification comes from the same field that
            // classifies the directory, so the finest level holds the slide's.
            app_mag: f64::from(pyramid[0].lens),
            jpeg_quality: DEFAULT_JPEG_QUALITY,
        },
        tile_width: pitch.0,
        tile_height: pitch.1,
        compression: Compression::Jpeg,
        levels,
        associated_images: macro_image.into_iter().collect(),
        thumbnail: None,
        sources: Vec::new(),
    })
}

/// Container-level values every level needs while it is read.
struct LevelContext {
    file_size: u64,
    big: bool,
    level0_width: u32,
}

/// One pyramid directory, before its strip has been examined.
struct Page<'a> {
    raw: &'a RawDirectory,
    lens: f32,
    width: u32,
    height: u32,
}

/// Reads a directory's `NDPI_SOURCELENS` value, which classifies it.
///
/// Hamamatsu stores the objective magnification here as a float, using `-1`
/// for the macro image and leaving it absent or zero on the directories that
/// belong to neither the pyramid nor the macro.
fn source_lens(file: &mut File, raw: &RawDirectory, big: bool) -> Result<Option<f32>> {
    let Some(entry) = raw.entry(NDPI_SOURCELENS) else {
        return Ok(None);
    };
    let unit = entry
        .item_size()
        .context("unsupported NDPI_SOURCELENS field type")?;
    let bytes = tiff::entry_bytes(file, entry, unit, big)?;
    Ok(Some(match entry.kind {
        FIELD_FLOAT if bytes.len() >= 4 => f32::from_le_bytes(bytes[..4].try_into().unwrap()),
        FIELD_DOUBLE if bytes.len() >= 8 => {
            f64::from_le_bytes(bytes[..8].try_into().unwrap()) as f32
        }
        _ => tiff::entry_scalar(file, entry, big)? as f32,
    }))
}

/// Reads the slide's micrometres per pixel from the finest level.
fn physical_resolution(file: &mut File, page: &Page<'_>, context: &LevelContext) -> Option<f64> {
    let resolution = match page.raw.entry(tag::X_RESOLUTION) {
        Some(entry) => tiff::entry_rational(file, entry, context.big).ok()?,
        None => None,
    };
    let unit =
        u16::try_from(coarse_scalar(file, page.raw, tag::RESOLUTION_UNIT, context.big).ok()?)
            .ok()?;
    tiff::mpp_from(resolution, unit)
}

/// Reads a directory's single strip as the macro image.
///
/// NDPI stores the macro as one self-contained JPEG, so nothing has to be
/// reassembled before the SVS writer can use it.
fn macro_image_from(
    file: &mut File,
    raw: &RawDirectory,
    big: bool,
    file_size: u64,
) -> Result<AssociatedImage> {
    let strips = strip_ranges(file, raw, big, file_size)?;
    let [strip] = strips[..] else {
        bail!("NDPI macro image is not a single strip");
    };
    Ok(AssociatedImage {
        kind: "macro".to_owned(),
        width: dimension(file, raw, tag::IMAGE_WIDTH, big)?,
        height: dimension(file, raw, tag::IMAGE_LENGTH, big)?,
        data: strip,
        ..Default::default()
    })
}

/// Turns one pyramid directory into a level.
fn read_level(
    file: &mut File,
    page: &Page<'_>,
    index: usize,
    context: &LevelContext,
    slide_pitch: Option<(u32, u32)>,
) -> Result<Level> {
    let compression = coarse_scalar(file, page.raw, tag::COMPRESSION, context.big)?;
    if compression != u64::from(tiff::COMPRESSION_JPEG) {
        bail!(
            "NDPI level {index} uses compression {compression}, which this reader does not decode"
        );
    }
    let strips = strip_ranges(file, page.raw, context.big, context.file_size)?;
    let [strip] = strips[..] else {
        bail!("NDPI level {index} is not a single strip");
    };
    let scan = scan_strip(file, strip)?;
    let downsample = if page.width > 0 {
        f64::from(context.level0_width) / f64::from(page.width)
    } else {
        1.0
    };

    // One interval is one tile only when the interval divides the MCU row, so
    // that each tile covers whole rows of MCUs.
    let aligned = scan.restart_interval != 0
        && scan.mcu_columns % scan.restart_interval == 0
        && scan.interval_width() > 0;
    if aligned {
        return tiled_level(file, page, index, strip, &scan, downsample);
    }
    let pitch = slide_pitch.with_context(|| {
        format!(
            "NDPI finest level restarts every {} MCUs, which does not divide its {} MCU columns \
             into whole rows",
            scan.restart_interval, scan.mcu_columns
        )
    })?;
    if u64::from(page.width) * u64::from(page.height) > WHOLE_STRIP_LIMIT_PIXELS {
        bail!(
            "NDPI level {index} restarts every {} MCUs, which does not divide its {} MCU columns \
             into whole rows, and at {}x{} pixels it is too large to decode as a single tile",
            scan.restart_interval,
            scan.mcu_columns,
            page.width,
            page.height
        );
    }
    Ok(whole_strip_level(index, page, strip, downsample, pitch))
}

/// Builds a level whose restart intervals are exactly its tiles.
fn tiled_level(
    file: &mut File,
    page: &Page<'_>,
    index: usize,
    strip: ByteRange,
    scan: &StripScan,
    downsample: f64,
) -> Result<Level> {
    let interval = u64::from(scan.restart_interval);
    let columns = u64::from(scan.mcu_columns) / interval;
    let rows = u64::from(scan.mcu_rows);
    let expected = columns
        .checked_mul(rows)
        .context("NDPI level tile count overflow")?;
    if expected != scan.intervals {
        bail!(
            "NDPI level {index} describes {expected} restart intervals but stores {}",
            scan.intervals
        );
    }

    let mut tiles = Vec::with_capacity(usize::try_from(expected)?);
    for unit in 0..expected {
        let unit = usize::try_from(unit)?;
        let start = match unit {
            0 => scan.entropy_start,
            _ => scan.restart_offsets[unit - 1] + 2,
        };
        let end = match scan.restart_offsets.get(unit) {
            Some(offset) => *offset,
            None => scan.eoi,
        };
        tiles.push(ByteRange {
            offset: strip.offset + start,
            length: end - start,
        });
    }

    let tile_width = scan.interval_width();
    let tile_height = scan.mcu_height;
    Ok(Level {
        index,
        width: page.width,
        height: page.height,
        downsample,
        tile_cols: u32::try_from(columns).context("NDPI level has too many tile columns")?,
        tile_rows: u32::try_from(rows).context("NDPI level has too many tile rows")?,
        tiles,
        tile_positions: Vec::new(),
        tile_groups: Vec::new(),
        tiling: TileLayout {
            tile_width,
            tile_height,
            prefix: patched_header(file, strip, scan, tile_width, tile_height)?,
            suffix: EOI_MARKER.to_vec(),
        },
    })
}

/// Copies a strip's header with the frame size rewritten to the tile geometry.
///
/// Both fields are two bytes wide, so the segment lengths and therefore every
/// offset in the header stay valid and the result can be reused by every tile
/// of the level.
fn patched_header(
    file: &mut File,
    strip: ByteRange,
    scan: &StripScan,
    tile_width: u32,
    tile_height: u32,
) -> Result<Vec<u8>> {
    let mut header = vec![0u8; usize::try_from(scan.entropy_start)?];
    file.seek(SeekFrom::Start(strip.offset))?;
    file.read_exact(&mut header)?;
    // The frame size is stored as two big-endian 16-bit fields.
    let height = u16::try_from(tile_height).context("tile is taller than a JPEG frame")?;
    let width = u16::try_from(tile_width).context("tile is wider than a JPEG frame")?;
    let at = scan.frame_at;
    header[at + 1..at + 3].copy_from_slice(&height.to_be_bytes());
    header[at + 3..at + 5].copy_from_slice(&width.to_be_bytes());
    Ok(header)
}

/// Builds a level small enough to decode as a single tile.
///
/// The level's grid is expressed on the slide's tile pitch so that the writer
/// reproduces the geometry the other levels use, while the one tile covers the
/// whole level and is cropped into place by its placement.
fn whole_strip_level(
    index: usize,
    page: &Page<'_>,
    strip: ByteRange,
    downsample: f64,
    pitch: (u32, u32),
) -> Level {
    Level {
        index,
        width: page.width,
        height: page.height,
        downsample,
        tile_cols: page.width.div_ceil(pitch.0.max(1)),
        tile_rows: page.height.div_ceil(pitch.1.max(1)),
        tiles: vec![strip],
        tile_positions: vec![TilePlacement {
            x: 0,
            y: 0,
            width: page.width,
            height: page.height,
            src_x: 0,
            src_y: 0,
        }],
        tile_groups: Vec::new(),
        tiling: TileLayout::default(),
    }
}

/// What the reader needs to know about one level's JPEG strip.
///
/// Every offset is relative to the start of the strip.
struct StripScan {
    /// Offset of the entropy-coded data, i.e. the start of the first interval.
    entropy_start: u64,
    /// Offset of the EOI marker that closes the strip.
    eoi: u64,
    /// Offset of every restart marker, in order.
    restart_offsets: Vec<u64>,
    /// Number of restart intervals, i.e. the marker count plus one.
    intervals: u64,
    /// MCUs between two restart markers, from the DRI segment.
    restart_interval: u32,
    /// Offset of the SOF payload, where the frame size is stored.
    frame_at: usize,
    mcu_width: u32,
    mcu_height: u32,
    mcu_columns: u32,
    mcu_rows: u32,
}

impl StripScan {
    /// Pixel width of one restart interval.
    fn interval_width(&self) -> u32 {
        self.restart_interval.saturating_mul(self.mcu_width)
    }
}

/// Reads a strip's JPEG segmentation without decoding any pixels.
fn scan_strip(file: &mut File, strip: ByteRange) -> Result<StripScan> {
    let probe_length = strip.length.min(HEADER_PROBE);
    let mut probe = vec![0u8; usize::try_from(probe_length)?];
    file.seek(SeekFrom::Start(strip.offset))?;
    file.read_exact(&mut probe)?;
    let segments = read_segments(&probe)?;

    let width = segments.frame.width;
    let height = segments.frame.height;
    if width == 0 || height == 0 {
        bail!("NDPI strip declares an empty frame");
    }
    let mcu_width = 8 * u32::from(segments.frame.sampling_h.max(1));
    let mcu_height = 8 * u32::from(segments.frame.sampling_v.max(1));
    let mcu_columns = width.div_ceil(mcu_width);
    let mcu_rows = height.div_ceil(mcu_height);

    let (restart_offsets, eoi) = find_restarts(file, strip, segments.entropy_start)?;
    let intervals = u64::try_from(restart_offsets.len())? + 1;
    if intervals > MAX_RESTART_INTERVALS {
        bail!("NDPI strip has {intervals} restart intervals, which is implausible");
    }
    Ok(StripScan {
        entropy_start: segments.entropy_start,
        eoi,
        restart_offsets,
        intervals,
        restart_interval: segments.restart_interval,
        frame_at: segments.frame_at,
        mcu_width,
        mcu_height,
        mcu_columns,
        mcu_rows,
    })
}

/// Scans a strip's entropy-coded data for restart markers and the EOI.
///
/// The scan is streamed because a full-resolution strip is tens of megabytes
/// and only the marker positions are kept.
fn find_restarts(file: &mut File, strip: ByteRange, entropy_start: u64) -> Result<(Vec<u64>, u64)> {
    let end = strip.offset + strip.length;
    let mut offsets = Vec::new();
    let mut eoi = None;
    let mut last_ff = None;
    let mut buffer = vec![0u8; SCAN_CHUNK];
    let mut position = strip.offset + entropy_start;
    'outer: while position < end {
        let want = usize::try_from((end - position).min(SCAN_CHUNK as u64))?;
        file.seek(SeekFrom::Start(position))?;
        let chunk = &mut buffer[..want];
        file.read_exact(chunk)?;
        for (index, &byte) in chunk.iter().enumerate() {
            let absolute = position + index as u64;
            if byte == 0xff {
                last_ff = Some(absolute - strip.offset);
                continue;
            }
            match last_ff.take() {
                Some(start) if (0xd0..=0xd7).contains(&byte) => offsets.push(start),
                Some(start) if byte == 0xd9 => {
                    eoi = Some(start);
                    break 'outer;
                }
                // FF00 is stuffed data, anything else is a marker and therefore
                // the end of the single scan this reader relies on.
                Some(_) if byte != 0x00 => break 'outer,
                _ => {}
            }
        }
        position += want as u64;
    }
    let eoi = eoi.context("NDPI strip has no EOI marker")?;
    Ok((offsets, eoi))
}

/// Where each segment of interest sits inside a strip.
struct Segments {
    frame: FrameHeader,
    restart_interval: u32,
    /// Offset of the entropy-coded data, i.e. of the first restart interval.
    entropy_start: u64,
    /// Offset of the SOF payload, i.e. of its sample precision byte.
    frame_at: usize,
}

/// The fields of a JPEG frame header that describe the MCU grid.
#[derive(Default)]
struct FrameHeader {
    width: u32,
    height: u32,
    sampling_h: u8,
    sampling_v: u8,
}

/// Walks the segment chain that opens a JPEG strip.
///
/// The chain ends at SOS, whose payload is the entropy-coded data; the walk
/// therefore returns the offset that data starts at rather than the segment.
fn read_segments(probe: &[u8]) -> Result<Segments> {
    if !probe.starts_with(&SOI_MARKER) {
        bail!("NDPI strip does not start with a JPEG SOI marker");
    }
    let mut frame = None;
    let mut frame_at = 0usize;
    let mut restart_interval = 0u32;
    let mut cursor = SOI_MARKER.len();
    while cursor + 2 <= probe.len() {
        if probe[cursor] != 0xff {
            bail!("malformed JPEG segment at offset {cursor}");
        }
        // Fill bytes are allowed between segments.
        let mut marker_at = cursor;
        while probe.get(marker_at + 1) == Some(&0xff) {
            marker_at += 1;
        }
        let code = *probe.get(marker_at + 1).context("truncated JPEG segment")?;
        let length_at = marker_at + 2;
        cursor = length_at;
        if code == 0x01 || code == 0xd8 || (0xd0..=0xd7).contains(&code) {
            continue;
        }
        if code == 0xd9 {
            bail!("NDPI strip ends before its SOS segment");
        }
        let length = usize::from(u16::from_be_bytes([
            *probe.get(length_at).context("truncated JPEG segment")?,
            *probe.get(length_at + 1).context("truncated JPEG segment")?,
        ]));
        if length < 2 {
            bail!("JPEG segment {code:#04x} declares an impossible length {length}");
        }
        let payload = length_at + 2;
        if code == 0xda {
            return Ok(Segments {
                frame: frame.context("NDPI strip has no JPEG frame header")?,
                restart_interval,
                entropy_start: u64::try_from(length_at + length)
                    .context("JPEG segment chain is too long")?,
                frame_at,
            });
        }
        match code {
            0xdd => {
                restart_interval = u32::from(u16::from_be_bytes([
                    *probe.get(payload).context("truncated DRI segment")?,
                    *probe.get(payload + 1).context("truncated DRI segment")?,
                ]));
            }
            _ if is_frame_header(code) => {
                frame = Some(read_frame_header(
                    probe.get(payload..).context("truncated SOF segment")?,
                )?);
                frame_at = payload;
            }
            _ => {}
        }
        cursor = length_at
            .checked_add(length)
            .context("JPEG segment chain overflow")?;
    }
    bail!("NDPI strip has no SOS segment")
}

/// Whether `code` introduces a frame header, i.e. an SOF segment.
fn is_frame_header(code: u8) -> bool {
    matches!(
        code,
        0xc0 | 0xc1 | 0xc2 | 0xc3 | 0xc5 | 0xc6 | 0xc7 | 0xc9 | 0xca | 0xcb | 0xcd | 0xce | 0xcf
    )
}

/// Reads the frame size and the first component's sampling factors.
///
/// The first component carries the luma sampling factors, which define how
/// many pixels one MCU spans.
fn read_frame_header(payload: &[u8]) -> Result<FrameHeader> {
    if payload.len() < 6 {
        bail!("truncated JPEG frame header");
    }
    let components = usize::from(payload[5]);
    if components == 0 || payload.len() < 6 + components * 3 {
        bail!("truncated JPEG frame header");
    }
    let sampling = payload[7];
    Ok(FrameHeader {
        height: u32::from(u16::from_be_bytes([payload[1], payload[2]])),
        width: u32::from(u16::from_be_bytes([payload[3], payload[4]])),
        sampling_h: sampling >> 4,
        sampling_v: sampling & 0x0f,
    })
}

/// Reads a directory's strip offsets as validated byte ranges.
fn strip_ranges(
    file: &mut File,
    raw: &RawDirectory,
    big: bool,
    file_size: u64,
) -> Result<Vec<ByteRange>> {
    let (Some(offsets), Some(counts)) = (
        raw.entry(tag::STRIP_OFFSETS),
        raw.entry(tag::STRIP_BYTE_COUNTS),
    ) else {
        return Ok(Vec::new());
    };
    let offsets = tiff::entry_numbers(file, offsets, big)?;
    let counts = tiff::entry_numbers(file, counts, big)?;
    if offsets.len() != counts.len() {
        bail!("NDPI strip offsets and byte counts disagree");
    }
    Ok(offsets
        .into_iter()
        .zip(counts)
        .map(|(offset, length)| {
            let range = ByteRange { offset, length };
            if range.validate(file_size, "NDPI strip").is_ok() {
                range
            } else {
                ByteRange::EMPTY
            }
        })
        .collect())
}

/// Reads an unsigned scalar field, defaulting to zero when it is absent.
fn coarse_scalar(file: &mut File, raw: &RawDirectory, code: u16, big: bool) -> Result<u64> {
    match raw.entry(code) {
        Some(entry) => tiff::entry_scalar(file, entry, big),
        None => Ok(0),
    }
}

/// Reads an unsigned scalar field that must be present and non-zero.
fn dimension(file: &mut File, raw: &RawDirectory, code: u16, big: bool) -> Result<u32> {
    let value = coarse_scalar(file, raw, code, big)?;
    let value = u32::try_from(value).with_context(|| format!("NDPI tag {code} is out of range"))?;
    if value == 0 {
        bail!("NDPI directory declares no image size");
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a JPEG header chain with the segments `read_segments` cares
    /// about, so the walk can be tested without a real strip.
    fn header(width: u16, height: u16, sampling: u8, restart: u16) -> Vec<u8> {
        let mut data = SOI_MARKER.to_vec();
        data.extend_from_slice(&[0xff, 0xdb, 0x00, 0x04, 0x00, 0x00]); // DQT, ignored
        data.extend_from_slice(&[0xff, 0xc0, 0x00, 0x11, 8]);
        data.extend_from_slice(&height.to_be_bytes());
        data.extend_from_slice(&width.to_be_bytes());
        data.extend_from_slice(&[3, 1, sampling, 0, 2, 0x11, 0, 3, 0x11, 0]);
        data.extend_from_slice(&[0xff, 0xdd, 0x00, 0x04]);
        data.extend_from_slice(&restart.to_be_bytes());
        // SOS: length 12 covers the length field plus a ten byte payload.
        data.extend_from_slice(&[0xff, 0xda, 0x00, 0x0c, 3, 1, 0, 2, 0, 3, 0, 0, 0x3f, 0]);
        data
    }

    #[test]
    fn reads_the_frame_size_and_sampling_factors() {
        let data = header(38400, 6912, 0x21, 240);
        let segments = read_segments(&data).unwrap();
        assert_eq!(segments.frame.width, 38400);
        assert_eq!(segments.frame.height, 6912);
        assert_eq!(segments.frame.sampling_h, 2);
        assert_eq!(segments.frame.sampling_v, 1);
        assert_eq!(segments.restart_interval, 240);
        assert_eq!(segments.entropy_start, data.len() as u64);
        let sof = data
            .windows(2)
            .position(|pair| pair == [0xff, 0xc0])
            .unwrap();
        assert_eq!(segments.frame_at, sof + 4);
    }

    #[test]
    fn skips_fill_bytes_between_segments() {
        let mut data = header(16, 8, 0x11, 4);
        let sos = data
            .windows(2)
            .position(|pair| pair == [0xff, 0xda])
            .unwrap();
        data.splice(sos..sos, [0xff, 0xff, 0xff]);
        let segments = read_segments(&data).unwrap();
        assert_eq!(segments.entropy_start, data.len() as u64);
    }

    #[test]
    fn rejects_a_strip_without_a_frame_header() {
        let mut data = SOI_MARKER.to_vec();
        data.extend_from_slice(&[0xff, 0xda, 0x00, 0x0c, 3, 1, 0, 2, 0, 3, 0, 0, 0x3f, 0]);
        assert!(read_segments(&data).is_err());
    }

    #[test]
    fn rejects_a_truncated_frame_header() {
        assert!(read_frame_header(&[8, 0, 8, 0, 16, 3, 1]).is_err());
        assert!(read_frame_header(&[8]).is_err());
    }

    #[test]
    fn interval_width_is_the_restart_span_in_pixels() {
        let scan = StripScan {
            entropy_start: 0,
            eoi: 0,
            restart_offsets: Vec::new(),
            intervals: 1,
            restart_interval: 240,
            frame_at: 0,
            mcu_width: 16,
            mcu_height: 8,
            mcu_columns: 2400,
            mcu_rows: 864,
        };
        assert_eq!(scan.interval_width(), 3840);
    }

    #[test]
    fn levels_override_the_slide_tile_pitch() {
        let mut level = Level::default();
        assert_eq!(level.stored_tile_pitch(256, 256), (256, 256));
        level.tiling.tile_width = 3840;
        level.tiling.tile_height = 8;
        assert_eq!(level.stored_tile_pitch(256, 256), (3840, 8));
    }
}
