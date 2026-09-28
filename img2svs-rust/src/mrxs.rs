//! Native reader for 3DHISTECH Pannoramic `.mrxs` slides.
//!
//! An `.mrxs` file is only a placeholder; the real content lives in the
//! sibling directory of the same name:
//!
//! - `Slidedat.ini` describes the zoom levels, the camera grid and the data
//!   files (`FILE_%d`).
//! - `Index.dat` holds the tile index: per zoom level a chain of pages, each
//!   page a list of 16-byte records `(image_index, offset, length, fileno)`
//!   pointing into the `Data*.dat` files, which are raw JPEG byte streams.
//! - Camera positions are stored in a non-hierarchical record, either raw
//!   (`VIMSLIDE_POSITION_BUFFER`) or zlib-compressed
//!   (`StitchingIntensityLayer`), as 9-byte `(flag, x, y)` records.
//!
//! The layout mirrors OpenSlide's `openslide-vendor-mirax.c`, which is the
//! reference implementation this reader was validated against. Decoded tiles
//! are placed at exact pixel coordinates, so the common SVS writer composites
//! them through the same positioned-tile path as KFB.
//!
//! Current limitations: every zoom level must store JPEG tiles (PNG/BMP24
//! variants are rejected), and tiles missing from the index are filled with
//! white regardless of `IMAGE_FILL_COLOR_BGR`.

use crate::binary::{le_at, Reader};
use crate::model::{
    assign_tile_groups, AssociatedImage, ByteRange, Compression, Level, Metadata, Slide,
    SlideSource, Thumbnail, TilePlacement,
};
use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

/// Version string at the start of `Index.dat`.
const INDEX_VERSION: &[u8] = b"01.02";
/// Bytes of one hierarchical tile record.
const HIER_RECORD_SIZE: usize = 16;
/// Bytes of one slide-position record: 1 flag byte plus two LE i32.
const POSITION_RECORD_SIZE: usize = 9;
/// Output tile edge; the SVS writer merges tiles only when needed for JPEG.
const OUTPUT_TILE_SIZE: u32 = 256;

/// Non-hierarchical names used to locate the records we care about.
const NONHIER_SCAN_DATA_LAYER: &str = "Scan data layer";
const NONHIER_STITCHING_INTENSITY: &str = "StitchingIntensityLayer";
const VALUE_VIMSLIDE_POSITION_BUFFER: &str = "VIMSLIDE_POSITION_BUFFER";
const VALUE_STITCHING_INTENSITY_LAYER: &str = "StitchingIntensityLevel";
const VALUE_SLIDE_THUMBNAIL: &str = "ScanDataLayer_SlideThumbnail";
const VALUE_SLIDE_BARCODE: &str = "ScanDataLayer_SlideBarcode";
const VALUE_SLIDE_PREVIEW: &str = "ScanDataLayer_SlidePreview";

/// One zoom level section of `Slidedat.ini`.
struct ZoomSection {
    concat_exponent: u32,
    overlap_x: f64,
    overlap_y: f64,
    mpp_x: f64,
    mpp_y: f64,
    image_width: u32,
    image_height: u32,
}

/// One hierarchical tile record: which `Data*.dat` file holds the JPEG and
/// where the tile sits on the level-0 image grid.
struct HierRecord {
    image_index: i32,
    fileno: usize,
    offset: u64,
    length: u64,
}

pub fn parse(path: &Path) -> Result<Slide> {
    let data_dir = path.with_extension("");
    let ini_path = data_dir.join("Slidedat.ini");
    let ini = read_ini(&ini_path).with_context(|| format!("read {}", ini_path.display()))?;

    let slide_id = ini_text(&ini, "general.slide_id").context("missing GENERAL SLIDE_ID")?;
    let images_across = ini_u32(&ini, "general.imagenumber_x").context("missing IMAGENUMBER_X")?;
    let images_down = ini_u32(&ini, "general.imagenumber_y").context("missing IMAGENUMBER_Y")?;
    let image_divisions = ini_u32(&ini, "general.cameraimagedivisionsperside").unwrap_or(1);
    if images_across == 0 || images_down == 0 || image_divisions == 0 {
        bail!("invalid MRXS camera grid");
    }

    let sections = read_zoom_sections(&ini)?;
    let data_files = read_data_files(&ini, &data_dir)?;
    let index = read_index(&data_dir, &ini, &slide_id)?;

    let positions_x = images_across / image_divisions;
    let positions_y = images_down / image_divisions;
    let position_count = (positions_x * positions_y) as usize;
    let mut positions = read_slide_positions(&ini, &index, &data_files, position_count)?
        .unwrap_or_else(|| {
            nominal_positions(&sections[0], positions_x, position_count, image_divisions)
        });

    // Level-0 concat multiplies the stored coordinates (always 1 in practice).
    let level0_concat = 1i32 << sections[0].concat_exponent;
    for position in &mut positions {
        position.0 *= level0_concat;
        position.1 *= level0_concat;
    }

    let (base_width, base_height) =
        base_dimensions(images_across, images_down, image_divisions, &sections[0]);

    let levels = build_levels(
        &index,
        &slide_id,
        &sections,
        &data_files,
        &positions,
        images_across,
        images_down,
        image_divisions,
        base_width,
        base_height,
    )?;
    if levels.is_empty() {
        bail!("MRXS slide contains no zoom levels");
    }

    let thumbnail = read_nonhier_image(
        &ini,
        &index,
        &data_files,
        NONHIER_SCAN_DATA_LAYER,
        VALUE_SLIDE_PREVIEW,
        "preview_image_width",
        "preview_image_height",
    )
    .map(|(data, width, height)| Thumbnail {
        width,
        height,
        data,
        ..Default::default()
    });

    let mut associated_images = Vec::new();
    for (kind, value) in [
        ("macro", VALUE_SLIDE_THUMBNAIL),
        ("label", VALUE_SLIDE_BARCODE),
    ] {
        if let Some((data, _, _)) = read_nonhier_image(
            &ini,
            &index,
            &data_files,
            NONHIER_SCAN_DATA_LAYER,
            value,
            "",
            "",
        ) {
            associated_images.push(AssociatedImage {
                kind: kind.to_owned(),
                data,
                ..Default::default()
            });
        }
    }

    let level0 = &levels[0];
    let section0 = &sections[0];
    Ok(Slide {
        path: PathBuf::from(path),
        metadata: Metadata {
            width: level0.width,
            height: level0.height,
            mpp: (section0.mpp_x + section0.mpp_y) / 2.0,
            app_mag: ini_f64(&ini, "general.objective_magnification").unwrap_or(0.0),
            jpeg_quality: 75,
        },
        tile_width: OUTPUT_TILE_SIZE,
        tile_height: OUTPUT_TILE_SIZE,
        compression: Compression::Jpeg,
        levels,
        associated_images,
        thumbnail,
        sources: data_files
            .iter()
            .map(|file| SlideSource {
                path: file.path.clone(),
                base: file.base,
            })
            .collect(),
    })
}

/// Parses `Slidedat.ini` into a lowercase `section.key` map.
fn read_ini(path: &Path) -> Result<HashMap<String, String>> {
    let text = fs::read_to_string(path)?;
    let mut section = String::new();
    let mut values = HashMap::new();
    for line in text.trim_start_matches('\u{feff}').lines().map(str::trim) {
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
    Ok(values)
}

fn ini_text(ini: &HashMap<String, String>, key: &str) -> Option<String> {
    ini.get(key).filter(|value| !value.is_empty()).cloned()
}

fn ini_u32(ini: &HashMap<String, String>, key: &str) -> Option<u32> {
    ini.get(key)?.trim().parse().ok()
}

fn ini_f64(ini: &HashMap<String, String>, key: &str) -> Option<f64> {
    ini.get(key)?.trim().parse().ok()
}

fn ini_i32(ini: &HashMap<String, String>, key: &str) -> Option<i32> {
    ini.get(key)?.trim().parse().ok()
}

/// Locates the "Slide zoom level" hierarchy and reads each level's section.
fn read_zoom_sections(ini: &HashMap<String, String>) -> Result<Vec<ZoomSection>> {
    let hier_count = ini_u32(ini, "hierarchical.hier_count").context("missing HIER_COUNT")?;
    // Falls back to hierarchy 0: a slide without the named hierarchy still
    // stores its pyramid there.
    let hier = (0..hier_count)
        .find(|&index| {
            ini_text(ini, &format!("hierarchical.hier_{index}_name")).unwrap_or_default()
                == "Slide zoom level"
        })
        .unwrap_or(0);
    let zoom_levels = ini_u32(ini, &format!("hierarchical.hier_{hier}_count"))
        .context("missing zoom level count")?;
    let mut sections = Vec::with_capacity(zoom_levels as usize);
    for level in 0..zoom_levels {
        let section_key = format!("hierarchical.hier_{hier}_val_{level}_section");
        let section = ini_text(ini, &section_key)
            .with_context(|| format!("missing zoom level section {level}"))?
            .to_ascii_lowercase();
        let format =
            ini_text(ini, &format!("{section}.image_format")).context("missing IMAGE_FORMAT")?;
        if !format.eq_ignore_ascii_case("jpeg") {
            bail!("unsupported MRXS tile format {format} (only JPEG is native)");
        }
        let concat_exponent = ini_i32(ini, &format!("{section}.image_concat_factor")).unwrap_or(0);
        if concat_exponent < 0 {
            bail!("invalid IMAGE_CONCAT_FACTOR");
        }
        sections.push(ZoomSection {
            concat_exponent: concat_exponent as u32,
            overlap_x: ini_f64(ini, &format!("{section}.overlap_x")).unwrap_or(0.0),
            overlap_y: ini_f64(ini, &format!("{section}.overlap_y")).unwrap_or(0.0),
            mpp_x: ini_f64(ini, &format!("{section}.micrometer_per_pixel_x"))
                .context("missing MICROMETER_PER_PIXEL_X")?,
            mpp_y: ini_f64(ini, &format!("{section}.micrometer_per_pixel_y"))
                .context("missing MICROMETER_PER_PIXEL_Y")?,
            image_width: ini_u32(ini, &format!("{section}.digitizer_width"))
                .context("missing DIGITIZER_WIDTH")?,
            image_height: ini_u32(ini, &format!("{section}.digitizer_height"))
                .context("missing DIGITIZER_HEIGHT")?,
        });
    }
    Ok(sections)
}

/// One `Data*.dat` backing file and its virtual base offset.
struct DataFile {
    path: PathBuf,
    base: u64,
    length: u64,
}

fn read_data_files(ini: &HashMap<String, String>, data_dir: &Path) -> Result<Vec<DataFile>> {
    let count = ini_u32(ini, "datafile.file_count").context("missing DATAFILE FILE_COUNT")?;
    let mut files = Vec::with_capacity(count as usize);
    let mut base = 0u64;
    for index in 0..count {
        let name = ini_text(ini, &format!("datafile.file_{index}"))
            .with_context(|| format!("missing DATAFILE FILE_{index}"))?;
        let path = data_dir.join(&name);
        let length = fs::metadata(&path)
            .with_context(|| format!("missing data file {}", path.display()))?
            .len();
        files.push(DataFile { path, base, length });
        base += length;
    }
    Ok(files)
}

/// Loads `Index.dat` after checking its version and slide identifier.
fn read_index(data_dir: &Path, ini: &HashMap<String, String>, slide_id: &str) -> Result<Vec<u8>> {
    let name = ini_text(ini, "hierarchical.indexfile").unwrap_or_else(|| "Index.dat".to_owned());
    let path = data_dir.join(&name);
    let index = fs::read(&path).with_context(|| format!("read {}", path.display()))?;
    if !index.starts_with(INDEX_VERSION) {
        bail!("Index.dat doesn't have expected version");
    }
    let uuid_start = INDEX_VERSION.len();
    if index.len() < uuid_start + slide_id.len() + 8
        || &index[uuid_start..uuid_start + slide_id.len()] != slide_id.as_bytes()
    {
        bail!("Index.dat doesn't have a matching slide identifier");
    }
    Ok(index)
}

/// Finds the record number of a named non-hierarchical value.
fn nonhier_recordno(
    ini: &HashMap<String, String>,
    target_name: &str,
    target_value: &str,
) -> Option<usize> {
    let count = ini_u32(ini, "hierarchical.nonhier_count")?;
    let mut offset = 0usize;
    for index in 0..count {
        let name = ini_text(ini, &format!("hierarchical.nonhier_{index}_name"))?;
        let value_count = ini_u32(ini, &format!("hierarchical.nonhier_{index}_count"))? as usize;
        if name == target_name {
            for value_index in 0..value_count {
                let value = ini_text(
                    ini,
                    &format!("hierarchical.nonhier_{index}_val_{value_index}"),
                )?;
                if value == target_value {
                    return Some(offset + value_index);
                }
            }
            return None;
        }
        offset += value_count;
    }
    None
}

/// Follows the pointer chain of a non-hierarchical record to its payload.
fn read_nonhier_record(index: &[u8], slide_id: &str, recordno: usize) -> Result<(usize, u64, u64)> {
    let nonhier_root = INDEX_VERSION.len() + slide_id.len() + 4;
    let table = le_at::<i32>(index, nonhier_root, "Index.dat")? as usize;
    let record_pointer = le_at::<i32>(index, table + 4 * recordno, "Index.dat")?;
    if record_pointer < 0 {
        bail!("invalid non-hierarchical record pointer");
    }
    let mut cursor = record_pointer as usize;
    if le_at::<i32>(index, cursor, "Index.dat")? != 0 {
        bail!("expected 0 value at beginning of data page");
    }
    cursor = le_at::<i32>(index, cursor + 4, "Index.dat")? as usize;
    if le_at::<i32>(index, cursor, "Index.dat")? < 1 {
        bail!("expected at least one data item");
    }
    // The data page is four i32 fields - next-page pointer, item count, and
    // two reserved zeroes - so the payload triple starts 16 bytes in.
    let position = le_at::<i32>(index, cursor + 16, "Index.dat")?;
    let size = le_at::<i32>(index, cursor + 20, "Index.dat")?;
    let fileno = le_at::<i32>(index, cursor + 24, "Index.dat")?;
    if position < 0 || size < 0 || fileno < 0 {
        bail!("invalid non-hierarchical record");
    }
    Ok((fileno as usize, position as u64, size as u64))
}

/// Reads a non-hierarchical record that holds a JPEG image.
fn read_nonhier_image(
    ini: &HashMap<String, String>,
    index: &[u8],
    data_files: &[DataFile],
    target_name: &str,
    target_value: &str,
    width_key: &str,
    height_key: &str,
) -> Option<(ByteRange, u32, u32)> {
    let recordno = nonhier_recordno(ini, target_name, target_value)?;
    let slide_id = ini_text(ini, "general.slide_id")?;
    // A malformed record is not fatal here: the caller treats a missing
    // label/macro image as "the container has none".
    let (fileno, offset, length) = read_nonhier_record(index, &slide_id, recordno).ok()?;
    let file = data_files.get(fileno)?;
    if offset + length > file.length || length == 0 {
        return None;
    }
    let (mut width, mut height) = (0, 0);
    if !width_key.is_empty() {
        // The dimensions live in the level section of the value; find it via
        // the NONHIER_%d_VAL_%d_SECTION pointer.
        if let Some(section) = nonhier_value_section(ini, target_name, target_value) {
            width = ini_u32(ini, &format!("{section}.{width_key}")).unwrap_or(0);
            height = ini_u32(ini, &format!("{section}.{height_key}")).unwrap_or(0);
        }
    }
    Some((
        ByteRange {
            offset: file.base + offset,
            length,
        },
        width,
        height,
    ))
}

/// Resolves the ini section that describes a non-hierarchical value.
fn nonhier_value_section(
    ini: &HashMap<String, String>,
    target_name: &str,
    target_value: &str,
) -> Option<String> {
    let count = ini_u32(ini, "hierarchical.nonhier_count")?;
    for index in 0..count {
        let name = ini_text(ini, &format!("hierarchical.nonhier_{index}_name"))?;
        if name != target_name {
            continue;
        }
        let value_count = ini_u32(ini, &format!("hierarchical.nonhier_{index}_count"))?;
        for value_index in 0..value_count {
            let value = ini_text(
                ini,
                &format!("hierarchical.nonhier_{index}_val_{value_index}"),
            )?;
            if value == target_value {
                return ini_text(
                    ini,
                    &format!("hierarchical.nonhier_{index}_val_{value_index}_section"),
                )
                .map(|section| section.to_ascii_lowercase());
            }
        }
    }
    None
}

/// Reads the camera position records, preferring the raw VIMSLIDE buffer and
/// falling back to the zlib-compressed stitching layer.
fn read_slide_positions(
    ini: &HashMap<String, String>,
    index: &[u8],
    data_files: &[DataFile],
    position_count: usize,
) -> Result<Option<Vec<(i32, i32)>>> {
    let slide_id = ini_text(ini, "general.slide_id").context("missing SLIDE_ID")?;
    let expected_size = (POSITION_RECORD_SIZE * position_count) as u64;

    let vimslide = nonhier_recordno(ini, NONHIER_SCAN_DATA_LAYER, VALUE_VIMSLIDE_POSITION_BUFFER);
    let stitching = nonhier_recordno(
        ini,
        NONHIER_STITCHING_INTENSITY,
        VALUE_STITCHING_INTENSITY_LAYER,
    );
    let (recordno, compressed) = match (vimslide, stitching) {
        (Some(recordno), _) => (recordno, false),
        (None, Some(recordno)) => (recordno, true),
        (None, None) => return Ok(None),
    };
    let (fileno, offset, length) = read_nonhier_record(index, &slide_id, recordno)?;
    let file = data_files
        .get(fileno)
        .context("invalid position record file")?;
    if offset + length > file.length {
        bail!("position record exceeds its data file");
    }
    let mut reader = Reader::open(&file.path)?;
    let raw = reader.range(offset, length, "MRXS position record")?;
    let buffer = if compressed {
        let mut decoded = Vec::with_capacity(expected_size as usize);
        flate2::read::ZlibDecoder::new(raw.as_slice())
            .read_to_end(&mut decoded)
            .context("decompress position buffer")?;
        decoded
    } else {
        raw
    };
    if buffer.len() as u64 != expected_size {
        bail!("slide position buffer has unexpected size");
    }

    let mut positions = Vec::with_capacity(position_count);
    for record in buffer.as_chunks::<POSITION_RECORD_SIZE>().0 {
        let flag = record[0];
        // Only bit 0 is defined ("this camera has an image"); the rest of the
        // byte is reserved and must read as zero.
        if flag & 0xfe != 0 {
            bail!("unexpected slide position flag {flag}");
        }
        let x = i32::from_le_bytes(record[1..5].try_into().unwrap());
        let y = i32::from_le_bytes(record[5..9].try_into().unwrap());
        positions.push((x, y));
    }
    Ok(Some(positions))
}

/// Synthesizes a regular camera grid when the slide stores no position data.
fn nominal_positions(
    section: &ZoomSection,
    positions_x: u32,
    position_count: usize,
    image_divisions: u32,
) -> Vec<(i32, i32)> {
    let advance_x = section.image_width as f64 * image_divisions as f64 - section.overlap_x;
    let advance_y = section.image_height as f64 * image_divisions as f64 - section.overlap_y;
    (0..position_count)
        .map(|index| {
            let column = index as u32 % positions_x;
            let row = index as u32 / positions_x;
            (
                (column as f64 * advance_x) as i32,
                (row as f64 * advance_y) as i32,
            )
        })
        .collect()
}

/// Slide dimensions in level-0 pixels: full image sizes everywhere except at
/// camera seams, where the overlap is shared between neighbouring photos.
fn base_dimensions(
    images_across: u32,
    images_down: u32,
    image_divisions: u32,
    section: &ZoomSection,
) -> (i64, i64) {
    (
        axis_extent(
            images_across,
            section.image_width,
            section.overlap_x,
            image_divisions,
        ),
        axis_extent(
            images_down,
            section.image_height,
            section.overlap_y,
            image_divisions,
        ),
    )
}

/// Length of one axis of the stitched base image.
///
/// Cameras overlap only inside a division; the seam at the end of a division
/// and the very last camera contribute their full width, so `count - 1`
/// cameras are shortened by the overlap.
fn axis_extent(count: u32, image_size: u32, overlap: f64, divisions: u32) -> i64 {
    (0..count)
        .map(|index| {
            let at_division_end = index % divisions == divisions - 1 && index != count - 1;
            if at_division_end {
                (image_size as f64 - overlap) as i64
            } else {
                i64::from(image_size)
            }
        })
        .sum()
}

/// Reads one zoom level's page chain from `Index.dat`.
fn read_hier_records(index: &[u8], slide_id: &str, zoom_level: usize) -> Result<Vec<HierRecord>> {
    // The root holds a pointer to a per-level pointer table; each entry there
    // addresses a `(0, page_pointer)` sentinel pair, and the page pointer
    // addresses the first `(page_len, next, records...)` page.
    let hier_root = INDEX_VERSION.len() + slide_id.len();
    let table = le_at::<i32>(index, hier_root, "Index.dat")?;
    if table < 0 {
        bail!("invalid hierarchical root pointer");
    }
    let level_pointer = le_at::<i32>(index, table as usize + 4 * zoom_level, "Index.dat")?;
    if level_pointer < 0 {
        bail!("invalid zoom level pointer");
    }
    let mut cursor = level_pointer as usize;
    if le_at::<i32>(index, cursor, "Index.dat")? != 0 {
        bail!("expected 0 value at beginning of data page");
    }
    cursor = le_at::<i32>(index, cursor + 4, "Index.dat")? as usize;

    let mut records = Vec::new();
    loop {
        let page_len = le_at::<i32>(index, cursor, "Index.dat")?;
        if page_len < 0 {
            bail!("invalid tile page length");
        }
        let next = le_at::<i32>(index, cursor + 4, "Index.dat")?;
        for entry in 0..page_len as usize {
            let record = cursor + 8 + entry * HIER_RECORD_SIZE;
            let image_index = le_at::<i32>(index, record, "Index.dat")?;
            let offset = le_at::<i32>(index, record + 4, "Index.dat")?;
            let length = le_at::<i32>(index, record + 8, "Index.dat")?;
            let fileno = le_at::<i32>(index, record + 12, "Index.dat")?;
            if image_index < 0 || offset < 0 || length < 0 || fileno < 0 {
                bail!("invalid tile record");
            }
            records.push(HierRecord {
                image_index,
                fileno: fileno as usize,
                offset: offset as u64,
                length: length as u64,
            });
        }
        if next == 0 {
            break;
        }
        if next < 0 {
            bail!("invalid tile page chain");
        }
        cursor = next as usize;
    }
    Ok(records)
}

/// Builds every pyramid level with exact pixel placements for its tiles.
#[allow(clippy::too_many_arguments)]
fn build_levels(
    index: &[u8],
    slide_id: &str,
    sections: &[ZoomSection],
    data_files: &[DataFile],
    positions: &[(i32, i32)],
    images_across: u32,
    images_down: u32,
    image_divisions: u32,
    base_width: i64,
    base_height: i64,
) -> Result<Vec<Level>> {
    let positions_across = images_across / image_divisions;
    let image0_width = sections[0].image_width;
    let image0_height = sections[0].image_height;
    let mut active = vec![false; positions.len()];
    let mut levels = Vec::with_capacity(sections.len());
    let mut total_concat_exponent = 0u32;
    let level0_concat = 1u32 << sections[0].concat_exponent;

    for (zoom_level, section) in sections.iter().enumerate() {
        total_concat_exponent += section.concat_exponent;
        if total_concat_exponent > 30 {
            bail!("image_concat exponent too large");
        }
        let concat = 1u32 << total_concat_exponent;
        let positions_per_image = (concat / image_divisions).max(1);
        let tiles_per_image = positions_per_image;
        let tile_width = section.image_width as f64 / tiles_per_image as f64;
        let tile_height = section.image_height as f64 / tiles_per_image as f64;
        let width = (base_width / i64::from(concat)).max(1) as u32;
        let height = (base_height / i64::from(concat)).max(1) as u32;

        let records = read_hier_records(index, slide_id, zoom_level)?;
        let mut tiles = Vec::new();
        let mut tile_positions = Vec::new();
        for record in records {
            let x = record.image_index as u32 % images_across;
            let y = record.image_index as u32 / images_across;
            if y >= images_down || x % concat != 0 || y % concat != 0 {
                bail!("tile record outside the level-0 image grid");
            }
            let file = data_files
                .get(record.fileno)
                .context("invalid tile fileno")?;
            if record.offset + record.length > file.length {
                bail!("tile record exceeds its data file");
            }
            for yi in 0..tiles_per_image {
                let yy = y + yi * image_divisions;
                if yy >= images_down {
                    break;
                }
                for xi in 0..tiles_per_image {
                    let xx = x + xi * image_divisions;
                    if xx >= images_across {
                        break;
                    }
                    let camera =
                        ((yy / image_divisions) * positions_across + xx / image_divisions) as usize;
                    let (camera_x, camera_y) = positions[camera];
                    if zoom_level == 0 {
                        // A position at the origin (except the true first
                        // camera) marks an unscanned field.
                        if camera_x == 0 && camera_y == 0 && camera != 0 {
                            continue;
                        }
                        active[camera] = true;
                    } else if !active[camera] {
                        continue;
                    }
                    let pos0_x =
                        camera_x as f64 + image0_width as f64 * (xx % image_divisions) as f64;
                    let pos0_y =
                        camera_y as f64 + image0_height as f64 * (yy % image_divisions) as f64;
                    tiles.push(ByteRange {
                        offset: file.base + record.offset,
                        length: record.length,
                    });
                    tile_positions.push(TilePlacement {
                        x: (pos0_x / concat as f64).round().max(0.0) as u32,
                        y: (pos0_y / concat as f64).round().max(0.0) as u32,
                        width: tile_width.round().max(1.0) as u32,
                        height: tile_height.round().max(1.0) as u32,
                        src_x: (tile_width * xi as f64).round() as u32,
                        src_y: (tile_height * yi as f64).round() as u32,
                    });
                }
            }
        }
        levels.push(Level {
            index: zoom_level,
            width,
            height,
            downsample: f64::from(concat / level0_concat),
            tile_cols: width.div_ceil(OUTPUT_TILE_SIZE),
            tile_rows: height.div_ceil(OUTPUT_TILE_SIZE),
            tiles,
            tile_positions,
            tile_groups: Vec::new(),
            tiling: Default::default(),
        });
    }
    assign_tile_groups(&mut levels, OUTPUT_TILE_SIZE, OUTPUT_TILE_SIZE);
    Ok(levels)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t1_section() -> ZoomSection {
        ZoomSection {
            concat_exponent: 0,
            overlap_x: 180.869565217392,
            overlap_y: 180.869565217392,
            mpp_x: 0.136904761904762,
            mpp_y: 0.136904761904762,
            image_width: 302,
            image_height: 252,
        }
    }

    #[test]
    fn base_dimensions_match_openslide_t1() {
        let (width, height) = base_dimensions(600, 1632, 8, &t1_section());
        assert_eq!((width, height), (167_806, 374_521));
    }

    #[test]
    fn nominal_grid_uses_photo_advance() {
        let positions = nominal_positions(&t1_section(), 75, 75 * 204, 8);
        let advance_x = (302.0 * 8.0 - 180.869565217392) as i32;
        let advance_y = (252.0 * 8.0 - 180.869565217392) as i32;
        assert_eq!(positions[0], (0, 0));
        assert_eq!(positions[1], (advance_x, 0));
        assert_eq!(positions[75], (0, advance_y));
    }

    #[test]
    fn recordno_offsets_accumulate_layer_counts() {
        let mut ini = HashMap::new();
        ini.insert("hierarchical.nonhier_count".to_owned(), "2".to_owned());
        ini.insert(
            "hierarchical.nonhier_0_name".to_owned(),
            "Scan data layer".to_owned(),
        );
        ini.insert("hierarchical.nonhier_0_count".to_owned(), "4".to_owned());
        for value_index in 0..3 {
            ini.insert(
                format!("hierarchical.nonhier_0_val_{value_index}"),
                format!("ScanDataLayer_Unused{value_index}"),
            );
        }
        ini.insert(
            "hierarchical.nonhier_0_val_3".to_owned(),
            "ScanDataLayer_SlideBarcode".to_owned(),
        );
        ini.insert(
            "hierarchical.nonhier_1_name".to_owned(),
            "StitchingIntensityLayer".to_owned(),
        );
        ini.insert("hierarchical.nonhier_1_count".to_owned(), "1".to_owned());
        ini.insert(
            "hierarchical.nonhier_1_val_0".to_owned(),
            "StitchingIntensityLevel".to_owned(),
        );
        assert_eq!(
            nonhier_recordno(&ini, "Scan data layer", "ScanDataLayer_SlideBarcode"),
            Some(3)
        );
        assert_eq!(
            nonhier_recordno(&ini, "StitchingIntensityLayer", "StitchingIntensityLevel"),
            Some(4)
        );
    }
}
