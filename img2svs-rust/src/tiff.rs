//! Native reader for plain TIFF containers (`.tif` / `.tiff`).
//!
//! A vendor-neutral TIFF is a chain of directories: the full-resolution image,
//! its reduced-resolution pyramid levels, and the label / macro / thumbnail
//! images that ride along in the same file.  Tiles stay on disk as byte ranges
//! so the SVS writer can stream them, which also lets a JPEG tile pass through
//! unchanged whenever the source tile geometry already matches the output.
//!
//! Only little-endian files are handled; every vendor that ships TIFF to
//! pathology pipelines writes `II` headers, and accepting the byte-swapped
//! form would double the parsing code for no practical gain.

use crate::jpeg::{EOI_MARKER, SOI_MARKER};
use crate::model::{
    AssociatedImage, ByteRange, Compression, Level, Metadata, Slide, Thumbnail, TileLayout,
};
use anyhow::{bail, Context, Result};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// Compression codes accepted here.
pub(crate) const COMPRESSION_JPEG: u16 = 7;

/// TIFF tag numbers this reader needs, by their specification names.
///
/// The module also owns the numbers shared with the NDPI reader, whose files
/// are TIFF containers with vendor-specific directories.
pub(crate) mod tag {
    pub const NEW_SUBFILE_TYPE: u16 = 254;
    pub const IMAGE_WIDTH: u16 = 256;
    pub const IMAGE_LENGTH: u16 = 257;
    pub const COMPRESSION: u16 = 259;
    pub const IMAGE_DESCRIPTION: u16 = 270;
    pub const STRIP_OFFSETS: u16 = 273;
    pub const SAMPLES_PER_PIXEL: u16 = 277;
    pub const STRIP_BYTE_COUNTS: u16 = 279;
    pub const X_RESOLUTION: u16 = 282;
    pub const PLANAR_CONFIGURATION: u16 = 284;
    pub const RESOLUTION_UNIT: u16 = 296;
    pub const TILE_WIDTH: u16 = 322;
    pub const TILE_LENGTH: u16 = 323;
    pub const TILE_OFFSETS: u16 = 324;
    pub const TILE_BYTE_COUNTS: u16 = 325;
    pub const JPEG_TABLES: u16 = 347;
}

/// `NewSubfileType` bit that marks a reduced-resolution pyramid level.
const FILETYPE_REDUCED_IMAGE: u32 = 0x1;

/// Units that let a resolution tag become a physical pixel size.
const RESOLUTION_INCH: u16 = 2;
const RESOLUTION_CENTIMETRE: u16 = 3;

/// Micrometres in one unit of the corresponding resolution tag.
const MICROMETRES_PER_INCH: f64 = 25_400.0;
pub(crate) const MICROMETRES_PER_CENTIMETRE: f64 = 10_000.0;

/// Quality assumed when a slide has to be re-encoded.
const DEFAULT_JPEG_QUALITY: u8 = 75;

/// Upper bound on the directory chain, so a cyclic `next` pointer cannot spin.
const MAX_DIRECTORIES: usize = 64;

/// Reads a `.tif` / `.tiff` slide into the format-independent model.
pub fn parse(path: &Path) -> Result<Slide> {
    let mut file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let file_size = file.metadata()?.len();
    let big = read_header(&mut file)?;
    let directories = read_directories(&mut file, big, file_size)?;
    if directories.is_empty() {
        bail!("TIFF contains no image directories");
    }

    let mut images = Vec::new();
    let mut associated_images = Vec::new();
    let mut thumbnail = None;
    for directory in directories {
        let Some(kind) = directory.associated_kind() else {
            images.push(directory);
            continue;
        };
        // Only JPEG pages can be reassembled from their strips here; anything
        // else is left out rather than failing the main image conversion.
        if directory.compression != COMPRESSION_JPEG {
            continue;
        }
        if kind == "thumbnail" {
            thumbnail = Some(directory.into_thumbnail());
        } else {
            associated_images.push(directory.into_associated(kind));
        }
    }

    // The largest image is level 0 and the remaining ones are its reduced
    // copies; ordering by area keeps that true even when a vendor emits the
    // pyramid before the full-resolution page.
    images.sort_by_key(|directory| std::cmp::Reverse(directory.pixels()));
    let mut images = images.into_iter();
    let main = images.next().context("TIFF has no full-resolution image")?;
    if main.tiles.is_empty() {
        bail!("TIFF full-resolution image is not tiled");
    }
    let level0_width = main.width;
    let level0_height = main.height;
    let tile_width = main.tile_width;
    let tile_height = main.tile_height;
    // Physical resolution and magnification come from the full-resolution
    // page; the reduced levels repeat them but level 0 is authoritative.
    let mpp = mpp_from(main.resolution, main.resolution_unit).unwrap_or(0.25);
    let app_mag = parse_labeled_number(
        main.description.as_deref().unwrap_or_default(),
        &["objective power", "appmag"],
    )
    .unwrap_or(0.0);
    let mut levels = vec![main.into_level(0, level0_width)?];
    for directory in images {
        // A stripped page that is not an embedded image cannot contribute
        // tiles, so it is skipped rather than failing the whole conversion.
        if directory.tiles.is_empty() {
            continue;
        }
        let index = levels.len();
        levels.push(directory.into_level(index, level0_width)?);
    }

    Ok(Slide {
        path: PathBuf::from(path),
        metadata: Metadata {
            width: level0_width,
            height: level0_height,
            mpp,
            app_mag,
            jpeg_quality: DEFAULT_JPEG_QUALITY,
        },
        tile_width,
        tile_height,
        compression: Compression::Jpeg,
        levels,
        associated_images,
        thumbnail,
        sources: Vec::new(),
    })
}

/// Reads the byte-order and magic fields, returning whether the file is a
/// BigTIFF.
pub(crate) fn read_header(file: &mut File) -> Result<bool> {
    file.seek(SeekFrom::Start(0))?;
    let mut header = [0u8; 4];
    file.read_exact(&mut header)?;
    if &header[0..2] != b"II" {
        bail!("only little-endian TIFF is supported");
    }
    match u16::from_le_bytes([header[2], header[3]]) {
        42 => Ok(false),
        43 => Ok(true),
        other => bail!("unsupported TIFF magic number {other}"),
    }
}

/// One raw directory of the chain, with its entries left uninterpreted.
///
/// Shared with the NDPI reader, which needs the same walk but assigns its own
/// meaning to the directories.
pub(crate) struct RawDirectory {
    pub offset: u64,
    pub entries: Vec<Entry>,
}

impl RawDirectory {
    /// The first entry carrying `code`.
    pub(crate) fn entry(&self, code: u16) -> Option<Entry> {
        self.entries
            .iter()
            .find(|entry| entry.code == code)
            .copied()
    }
}

/// Walks the directory chain, reading every entry but decoding nothing.
pub(crate) fn read_chain(file: &mut File, big: bool, file_size: u64) -> Result<Vec<RawDirectory>> {
    let first = if big {
        read_at_u64(file, 8)?
    } else {
        u64::from(read_at_u32(file, 4)?)
    };
    let mut directories = Vec::new();
    let mut offset = first;
    while offset != 0 {
        if offset >= file_size {
            bail!("TIFF directory offset {offset} is outside the file");
        }
        if directories.len() == MAX_DIRECTORIES {
            bail!("TIFF has more than {MAX_DIRECTORIES} directories");
        }
        file.seek(SeekFrom::Start(offset))?;
        let entry_count = if big {
            read_u64(file)?
        } else {
            u64::from(read_u16(file)?)
        };
        let mut entries = Vec::with_capacity(usize::try_from(entry_count)?);
        for _ in 0..entry_count {
            let code = read_u16(file)?;
            let kind = read_u16(file)?;
            let (count, value) = if big {
                (read_u64(file)?, read_u64(file)?)
            } else {
                (u64::from(read_u32(file)?), u64::from(read_u32(file)?))
            };
            entries.push(Entry {
                code,
                kind,
                count,
                value,
            });
        }
        let next = if big {
            read_u64(file)?
        } else {
            u64::from(read_u32(file)?)
        };
        directories.push(RawDirectory { offset, entries });
        offset = next;
    }
    Ok(directories)
}

/// Walks the chain again, keeping only the entries that describe an image.
fn read_directories(file: &mut File, big: bool, file_size: u64) -> Result<Vec<Directory>> {
    read_chain(file, big, file_size)?
        .into_iter()
        .map(|raw| read_directory(file, &raw, big, file_size))
        .collect()
}

fn read_directory(
    file: &mut File,
    raw: &RawDirectory,
    big: bool,
    file_size: u64,
) -> Result<Directory> {
    let offset = raw.offset;
    let entry = |code: u16| raw.entry(code);
    let scalar = |file: &mut File, code: u16| -> Result<u64> {
        match entry(code) {
            Some(entry) => entry_scalar(file, entry, big),
            None => Ok(0),
        }
    };

    let width = u32_value(scalar(file, tag::IMAGE_WIDTH)?, "image width")?;
    let height = u32_value(scalar(file, tag::IMAGE_LENGTH)?, "image length")?;
    if width == 0 || height == 0 {
        bail!("TIFF directory at {offset} declares an empty image");
    }
    let compression = u16_value(scalar(file, tag::COMPRESSION)?, "compression")?;
    let samples = u16_value(scalar(file, tag::SAMPLES_PER_PIXEL)?, "samples per pixel")?;
    let planar = u16_value(
        scalar(file, tag::PLANAR_CONFIGURATION)?,
        "planar configuration",
    )?;
    if samples > 1 && planar != 1 {
        bail!("TIFF directory at {offset} uses planar configuration {planar}");
    }

    let tile_width = u32_value(scalar(file, tag::TILE_WIDTH)?, "tile width")?;
    let tile_height = u32_value(scalar(file, tag::TILE_LENGTH)?, "tile length")?;
    let tiles = pair_ranges(
        file,
        entry(tag::TILE_OFFSETS),
        entry(tag::TILE_BYTE_COUNTS),
        big,
        file_size,
        "tile",
    )?;
    let strips = pair_ranges(
        file,
        entry(tag::STRIP_OFFSETS),
        entry(tag::STRIP_BYTE_COUNTS),
        big,
        file_size,
        "strip",
    )?;

    let jpeg_tables = match entry(tag::JPEG_TABLES) {
        Some(entry) => entry_ascii(file, entry, big)?,
        None => Vec::new(),
    };
    let description = match entry(tag::IMAGE_DESCRIPTION) {
        Some(entry) => Some(
            String::from_utf8_lossy(&entry_ascii(file, entry, big)?)
                .trim_end_matches('\0')
                .trim()
                .to_owned(),
        ),
        None => None,
    };
    let resolution = match entry(tag::X_RESOLUTION) {
        Some(entry) => entry_rational(file, entry, big)?,
        None => None,
    };
    let resolution_unit = u16_value(scalar(file, tag::RESOLUTION_UNIT)?, "resolution unit")?;

    Ok(Directory {
        width,
        height,
        tile_width,
        tile_height,
        compression,
        description,
        resolution,
        resolution_unit,
        subfile_type: u32_value(scalar(file, tag::NEW_SUBFILE_TYPE)?, "subfile type")?,
        tiles,
        strips,
        jpeg_tables,
    })
}

/// One raw directory entry.
#[derive(Clone, Copy)]
pub(crate) struct Entry {
    pub(crate) code: u16,
    pub(crate) kind: u16,
    pub(crate) count: u64,
    pub(crate) value: u64,
}

impl Entry {
    /// Size in bytes of a single item of this field type.
    pub(crate) fn item_size(self) -> Option<u64> {
        match self.kind {
            1 | 2 | 6 | 7 => Some(1),
            3 | 8 => Some(2),
            4 | 9 | 11 => Some(4),
            5 | 10 | 12 | 16 | 17 | 18 => Some(8),
            _ => None,
        }
    }
}

/// Reads a field whose values are all scalars, returning the first one.
pub(crate) fn entry_scalar(file: &mut File, entry: Entry, big: bool) -> Result<u64> {
    if entry.kind == 5 || entry.kind == 10 {
        // RATIONAL is two 32-bit halves; callers that need the value use
        // `entry_rational` instead.
        return Ok(0);
    }
    let unit = entry
        .item_size()
        .with_context(|| format!("unsupported TIFF field type {}", entry.kind))?;
    let raw = entry_bytes(file, entry, unit, big)?;
    if raw.len() < unit as usize {
        return Ok(0);
    }
    Ok(match entry.kind {
        1 | 2 | 6 | 7 => u64::from(raw[0]),
        3 | 8 => u64::from(u16::from_le_bytes([raw[0], raw[1]])),
        16..=18 => u64::from_le_bytes(raw[..8].try_into().unwrap()),
        _ => u64::from(u32::from_le_bytes(raw[..4].try_into().unwrap())),
    })
}

/// Reads a RATIONAL field (type 5) as a floating point value.
pub(crate) fn entry_rational(file: &mut File, entry: Entry, big: bool) -> Result<Option<f64>> {
    if entry.count == 0 {
        return Ok(None);
    }
    if entry.kind != 5 && entry.kind != 10 {
        // Some writers store plain integers; treat those as the numerator.
        let value = entry_scalar(file, entry, big)?;
        return Ok((value > 0).then_some(value as f64));
    }
    let raw = entry_bytes(file, entry, 8, big)?;
    if raw.len() < 8 {
        return Ok(None);
    }
    let numerator = u32::from_le_bytes(raw[0..4].try_into().unwrap());
    let denominator = u32::from_le_bytes(raw[4..8].try_into().unwrap());
    if denominator == 0 {
        return Ok(None);
    }
    Ok(Some(f64::from(numerator) / f64::from(denominator)))
}

/// Reads an ASCII or UNDEFINED field as raw bytes.
fn entry_ascii(file: &mut File, entry: Entry, big: bool) -> Result<Vec<u8>> {
    let unit = entry
        .item_size()
        .with_context(|| format!("unsupported TIFF field type {}", entry.kind))?;
    if unit != 1 {
        bail!("TIFF field {} is not byte oriented", entry.code);
    }
    entry_bytes(file, entry, unit, big)
}

/// Reads the payload of an entry, whether it is stored inline or behind an
/// offset.  The 4-byte (classic) or 8-byte (BigTIFF) value slot is the whole
/// payload when it fits, which is how short ASCII and byte arrays are kept.
pub(crate) fn entry_bytes(file: &mut File, entry: Entry, unit: u64, big: bool) -> Result<Vec<u8>> {
    let total = entry
        .count
        .checked_mul(unit)
        .context("TIFF field size overflow")?;
    let inline = if big { 8 } else { 4 };
    let mut bytes = vec![0u8; usize::try_from(total)?];
    if total <= inline {
        let raw = if big {
            entry.value.to_le_bytes().to_vec()
        } else {
            (entry.value as u32).to_le_bytes().to_vec()
        };
        let length = bytes.len();
        bytes.copy_from_slice(&raw[..length]);
    } else {
        file.seek(SeekFrom::Start(entry.value))?;
        file.read_exact(&mut bytes)?;
    }
    Ok(bytes)
}

/// Reads the parallel offset and byte-count arrays that describe tiles or
/// strips, pairing them into validated ranges.
fn pair_ranges(
    file: &mut File,
    offsets: Option<Entry>,
    counts: Option<Entry>,
    big: bool,
    file_size: u64,
    label: &str,
) -> Result<Vec<ByteRange>> {
    let (Some(offsets), Some(counts)) = (offsets, counts) else {
        return Ok(Vec::new());
    };
    let offsets = entry_numbers(file, offsets, big)?;
    let counts = entry_numbers(file, counts, big)?;
    if offsets.len() != counts.len() {
        bail!(
            "TIFF {label} arrays disagree: {} offsets, {} byte counts",
            offsets.len(),
            counts.len()
        );
    }
    Ok(offsets
        .into_iter()
        .zip(counts)
        .map(|(offset, length)| keep_valid_range(offset, length, file_size))
        .collect())
}

/// Reads a field as a list of unsigned numbers.
pub(crate) fn entry_numbers(file: &mut File, entry: Entry, big: bool) -> Result<Vec<u64>> {
    let unit = entry
        .item_size()
        .with_context(|| format!("unsupported TIFF field type {}", entry.kind))?;
    let raw = entry_bytes(file, entry, unit, big)?;
    let mut values = Vec::with_capacity(entry.count as usize);
    for index in 0..usize::try_from(entry.count)? {
        let start = index * unit as usize;
        if start + unit as usize > raw.len() {
            break;
        }
        values.push(match entry.kind {
            1 | 2 | 6 | 7 => u64::from(raw[start]),
            3 | 8 => u64::from(u16::from_le_bytes(
                raw[start..start + 2].try_into().unwrap(),
            )),
            16..=18 => u64::from_le_bytes(raw[start..start + 8].try_into().unwrap()),
            _ => u64::from(u32::from_le_bytes(
                raw[start..start + 4].try_into().unwrap(),
            )),
        });
    }
    Ok(values)
}

/// Keeps a range only when it addressable data; TIFF uses offset 0 to mean
/// "no tile here", which the writer turns into a blank cell.
fn keep_valid_range(offset: u64, length: u64, file_size: u64) -> ByteRange {
    let range = ByteRange { offset, length };
    if range.validate(file_size, "TIFF tile").is_ok() {
        range
    } else {
        ByteRange::EMPTY
    }
}

/// One directory of the container.
#[derive(Debug)]
struct Directory {
    width: u32,
    height: u32,
    tile_width: u32,
    tile_height: u32,
    compression: u16,
    description: Option<String>,
    resolution: Option<f64>,
    resolution_unit: u16,
    subfile_type: u32,
    tiles: Vec<ByteRange>,
    strips: Vec<ByteRange>,
    jpeg_tables: Vec<u8>,
}

impl Directory {
    /// Addressable area, used to order the pyramid levels.
    fn pixels(&self) -> u64 {
        u64::from(self.width) * u64::from(self.height)
    }

    /// Which embedded image this directory holds, or `None` for a pyramid
    /// level.
    ///
    /// The description is the only reliable signal: a vendor that tags a page
    /// as a reduced image also has to describe it, and the three keywords are
    /// the naming convention every writer follows.
    fn associated_kind(&self) -> Option<&'static str> {
        if self.subfile_type & FILETYPE_REDUCED_IMAGE != 0 {
            return None;
        }
        let description = self.description.as_deref()?.to_ascii_lowercase();
        if description.contains("label") {
            return Some("label");
        }
        if description.contains("macro") {
            return Some("macro");
        }
        if description.contains("thumbnail") {
            return Some("thumbnail");
        }
        None
    }

    /// Converts a tiled directory into a pyramid level.
    fn into_level(self, index: usize, level0_width: u32) -> Result<Level> {
        // Only JPEG tiles have a decoder here; accepting any other code would
        // defer the failure to the tile loop, where it reads as corrupt data.
        if self.compression != COMPRESSION_JPEG {
            bail!(
                "TIFF level {index} uses compression {} which this reader does not decode",
                self.compression
            );
        }
        let tile_width = self.tile_width.max(1);
        let tile_height = self.tile_height.max(1);
        let tile_cols = self.width.div_ceil(tile_width);
        let tile_rows = self.height.div_ceil(tile_height);
        let expected = u64::from(tile_cols) * u64::from(tile_rows);
        if (self.tiles.len() as u64) < expected {
            bail!(
                "TIFF level {index} is {tile_cols}x{tile_rows} tiles but stores only {}",
                self.tiles.len()
            );
        }
        let mut tiles = self.tiles[..expected as usize].to_vec();
        let tiling = if self.jpeg_tables.is_empty() {
            TileLayout::default()
        } else {
            // Tag 347 keeps the quantization and Huffman tables once and lets
            // every tile omit them, so a tile starts at the SOI the tables
            // already provide. Dropping that byte pair lets the writer splice
            // the two pieces back into one stream.
            let drop = SOI_MARKER.len() as u64;
            for tile in &mut tiles {
                if tile.present() && tile.length > drop {
                    tile.offset += drop;
                    tile.length -= drop;
                } else {
                    *tile = ByteRange::EMPTY;
                }
            }
            TileLayout {
                prefix: self
                    .jpeg_tables
                    .strip_suffix(&EOI_MARKER)
                    .unwrap_or(&self.jpeg_tables)
                    .to_vec(),
                ..Default::default()
            }
        };
        Ok(Level {
            index,
            width: self.width,
            height: self.height,
            downsample: if self.width > 0 {
                f64::from(level0_width) / f64::from(self.width)
            } else {
                1.0
            },
            tile_cols,
            tile_rows,
            tiles,
            tile_positions: Vec::new(),
            tile_groups: Vec::new(),
            tiling,
        })
    }

    /// Converts a stripped directory into a label or macro image.
    fn into_associated(self, kind: &str) -> AssociatedImage {
        AssociatedImage {
            kind: kind.to_owned(),
            width: self.width,
            height: self.height,
            data: ByteRange::EMPTY,
            strips: self.strips,
            jpeg_tables: self.jpeg_tables,
        }
    }

    fn into_thumbnail(self) -> Thumbnail {
        Thumbnail {
            width: self.width,
            height: self.height,
            data: ByteRange::EMPTY,
            strips: self.strips,
            jpeg_tables: self.jpeg_tables,
        }
    }
}

/// Converts a resolution tag plus its unit into micrometres per pixel.
pub(crate) fn mpp_from(resolution: Option<f64>, unit: u16) -> Option<f64> {
    let value = resolution?;
    if !value.is_finite() || value <= 0.0 {
        return None;
    }
    match unit {
        RESOLUTION_CENTIMETRE => Some(MICROMETRES_PER_CENTIMETRE / value),
        RESOLUTION_INCH => Some(MICROMETRES_PER_INCH / value),
        _ => None,
    }
}

/// Extracts the first number that follows one of `labels` in `text`.
fn parse_labeled_number(text: &str, labels: &[&str]) -> Option<f64> {
    let lowercase = text.to_ascii_lowercase();
    labels.iter().find_map(|label| {
        let index = lowercase.find(label)? + label.len();
        let remainder = lowercase[index..].trim_start_matches(|character: char| {
            character.is_ascii_whitespace() || matches!(character, '=' | ':' | '|')
        });
        let length = remainder
            .char_indices()
            .take_while(|(_, character)| {
                character.is_ascii_digit() || matches!(character, '.' | '+' | '-')
            })
            .map(|(index, character)| index + character.len_utf8())
            .last()?;
        remainder[..length]
            .parse::<f64>()
            .ok()
            .filter(|value| *value > 0.0)
    })
}

fn u16_value(value: u64, label: &str) -> Result<u16> {
    u16::try_from(value).with_context(|| format!("{label} does not fit in 16 bits"))
}

fn u32_value(value: u64, label: &str) -> Result<u32> {
    u32::try_from(value).with_context(|| format!("{label} does not fit in 32 bits"))
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

fn read_at_u32(file: &mut File, offset: u64) -> Result<u32> {
    file.seek(SeekFrom::Start(offset))?;
    read_u32(file)
}

fn read_at_u64(file: &mut File, offset: u64) -> Result<u64> {
    file.seek(SeekFrom::Start(offset))?;
    read_u64(file)
}

#[cfg(test)]
mod tests {
    use super::{mpp_from, parse_labeled_number, RESOLUTION_CENTIMETRE, RESOLUTION_INCH};

    #[test]
    fn parses_tiff_objective_power() {
        assert_eq!(
            parse_labeled_number("Objective Power=20", &["objective power", "appmag"]),
            Some(20.0)
        );
    }

    #[test]
    fn parses_aperio_description() {
        assert_eq!(
            parse_labeled_number(
                "Aperio Image Library|AppMag = 40.000000|MPP = 0.250000",
                &["objective power", "appmag"]
            ),
            Some(40.0)
        );
    }

    #[test]
    fn converts_centimetre_resolution_to_micrometres_per_pixel() {
        // 9162105 / 256 pixels per centimetre is the resolution of the sample
        // slide, i.e. about 0.279 um/px.
        let mpp = mpp_from(Some(9_162_105.0 / 256.0), RESOLUTION_CENTIMETRE).unwrap();
        assert!((mpp - 0.2794).abs() < 1e-4, "{mpp}");
    }

    #[test]
    fn converts_inch_resolution_to_micrometres_per_pixel() {
        assert_eq!(mpp_from(Some(25_400.0), RESOLUTION_INCH), Some(1.0));
    }

    #[test]
    fn rejects_resolution_without_a_physical_unit() {
        assert_eq!(mpp_from(Some(300.0), 1), None);
        assert_eq!(mpp_from(None, RESOLUTION_CENTIMETRE), None);
        assert_eq!(mpp_from(Some(0.0), RESOLUTION_CENTIMETRE), None);
    }
}
