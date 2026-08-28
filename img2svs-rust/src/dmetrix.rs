use crate::binary::Reader;
use crate::jpeg::decode_image;
use crate::model::{AssociatedImage, ByteRange, Compression, Level, Metadata, Slide};
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

const MAGIC: &[u8; 8] = b"DmetrixN";
const TILE_RECORD_SIZE: u64 = 22;

#[derive(Clone, Copy, Debug)]
struct Descriptor {
    source_id: u16,
    max_x: u32,
    max_y: u32,
    index_offset: u64,
}

pub fn parse(path: &Path) -> Result<Slide> {
    let mut reader = Reader::open(path)?;
    let file_size = reader.len();
    reader.seek(0)?;
    if reader.bytes(8, "DMetrix magic")? != MAGIC {
        bail!("unsupported DMetrix container: {}", path.display());
    }
    let mpp_x = read_f64_at(&mut reader, 0x30)?;
    let mpp_y = read_f64_at(&mut reader, 0x38)?;
    let app_mag = read_u32_at(&mut reader, 0x40)?;
    if !(0.0 < mpp_x
        && mpp_x < 100.0
        && 0.0 < mpp_y
        && mpp_y < 100.0
        && app_mag > 0
        && app_mag <= 200)
    {
        bail!("invalid DMetrix scan metadata");
    }
    let descriptors = read_descriptors(&mut reader)?;
    let associated = read_associated(&mut reader, descriptors[0].index_offset, file_size)?;
    let raw_levels = read_tile_indexes(&mut reader, &descriptors, file_size)?;
    let tile_size = discover_tile_size(
        &mut reader,
        raw_levels.last().context("missing DMetrix levels")?[0],
    )?;

    let mut levels = Vec::new();
    for (index, (descriptor, tiles)) in descriptors.iter().zip(raw_levels.iter()).rev().enumerate()
    {
        let edge = tiles[(descriptor.max_y * (descriptor.max_x + 1) + descriptor.max_x) as usize];
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
            tile_cols: descriptor.max_x + 1,
            tile_rows: descriptor.max_y + 1,
            tiles: tiles.clone(),
            tile_positions: Vec::new(),
            tile_groups: Vec::new(),
        });
    }
    let width = levels[0].width;
    let height = levels[0].height;
    let jpeg_quality = estimate_quality(&mut reader, levels[0].tiles[0]).unwrap_or(75);
    Ok(Slide {
        path: PathBuf::from(path),
        metadata: Metadata {
            width,
            height,
            mpp: (mpp_x + mpp_y) / 2.0,
            app_mag: app_mag as f64,
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

fn read_descriptors(reader: &mut Reader) -> Result<Vec<Descriptor>> {
    reader.seek(0xc2)?;
    let mut result = Vec::new();
    for _ in 0..64 {
        let source_id = reader.u16()?;
        let max_x = reader.u32()?;
        let max_y = reader.u32()?;
        let index_offset = reader.u32()? as u64;
        if index_offset == 0 {
            break;
        }
        if max_x > 100_000 || max_y > 100_000 {
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

fn read_associated(
    reader: &mut Reader,
    first_index: u64,
    file_size: u64,
) -> Result<Vec<AssociatedImage>> {
    let start = first_index
        .checked_sub(2 * TILE_RECORD_SIZE)
        .context("invalid DMetrix associated-image index")?;
    reader.seek(start)?;
    let mut found = [None, None];
    for _ in 0..2 {
        let id = reader.u16()?;
        let _ = reader.u32()?;
        let _ = reader.u32()?;
        let offset = reader.u64()?;
        let length = reader.u32()? as u64;
        let data = ByteRange { offset, length };
        validate_range(data, file_size, "associated image")?;
        if id == 0xffff {
            found[0] = Some(data);
        }
        if id == 0xfffe {
            found[1] = Some(data);
        }
    }
    Ok([("label", found[0]), ("macro", found[1])]
        .into_iter()
        .filter_map(|(kind, data)| {
            data.map(|data| AssociatedImage {
                kind: kind.to_owned(),
                data,
            })
        })
        .collect())
}

fn read_tile_indexes(
    reader: &mut Reader,
    descriptors: &[Descriptor],
    file_size: u64,
) -> Result<Vec<Vec<ByteRange>>> {
    let mut result = Vec::with_capacity(descriptors.len());
    for descriptor in descriptors {
        reader.seek(descriptor.index_offset)?;
        let count = (descriptor.max_x + 1)
            .checked_mul(descriptor.max_y + 1)
            .context("DMetrix tile count overflow")?;
        let mut tiles = vec![
            ByteRange {
                offset: 0,
                length: 0
            };
            count as usize
        ];
        let mut seen = vec![false; count as usize];
        for _ in 0..count {
            let source_id = reader.u16()?;
            let x = reader.u32()?;
            let y = reader.u32()?;
            let data = ByteRange {
                offset: reader.u64()?,
                length: reader.u32()? as u64,
            };
            if source_id != descriptor.source_id || x > descriptor.max_x || y > descriptor.max_y {
                bail!(
                    "invalid DMetrix tile record in level {}",
                    descriptor.source_id
                );
            }
            validate_range(data, file_size, "level tile")?;
            let slot = (y * (descriptor.max_x + 1) + x) as usize;
            if seen[slot] {
                bail!("duplicate DMetrix tile coordinate ({x}, {y})");
            }
            seen[slot] = true;
            tiles[slot] = data;
        }
        if seen.iter().any(|value| !value) {
            bail!("DMetrix tile count mismatch");
        }
        result.push(tiles);
    }
    Ok(result)
}

fn discover_tile_size(reader: &mut Reader, range: ByteRange) -> Result<u32> {
    let image = decode_image(&reader.range(range.offset, range.length, "DMetrix tile")?)?;
    if image.width() != image.height() || !(16..=4096).contains(&image.width()) {
        bail!(
            "unsupported DMetrix tile size: {}x{}",
            image.width(),
            image.height()
        );
    }
    Ok(image.width())
}

fn estimate_quality(reader: &mut Reader, range: ByteRange) -> Option<u8> {
    let data = reader
        .range(range.offset, range.length, "DMetrix JPEG")
        .ok()?;
    if data.len() < 4 || data[0..2] != [0xff, 0xd8] {
        return None;
    }
    // The working Python implementation estimates from quantization tables. Keep
    // the safe default here; encoding quality is explicitly configurable by CLI.
    Some(75)
}

fn validate_range(data: ByteRange, file_size: u64, context: &str) -> Result<()> {
    if !data.present() || data.offset >= file_size || data.length > file_size - data.offset {
        bail!("invalid byte range for DMetrix {context}");
    }
    Ok(())
}

fn read_u32_at(reader: &mut Reader, offset: u64) -> Result<u32> {
    reader.seek(offset)?;
    reader.u32()
}
fn read_f64_at(reader: &mut Reader, offset: u64) -> Result<f64> {
    reader.seek(offset)?;
    reader.f64()
}

pub fn print_info(slide: &Slide) {
    println!(
        "Image : {}x{}, tile={}x{}, levels={}, compression=jpeg",
        slide.metadata.width,
        slide.metadata.height,
        slide.tile_width,
        slide.tile_height,
        slide.levels.len()
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
