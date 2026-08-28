//! Rust readers for the vendor containers that store JPEG tiles in an index.
//! The layouts mirror the validated Python readers, but all data stays on disk
//! and is consumed by the common SVS writer through byte ranges.

use crate::binary::Reader;
use crate::model::{
    AssociatedImage, ByteRange, Compression, Level, Metadata, Slide, Thumbnail, TilePlacement,
};
use anyhow::{bail, Context, Result};
use base64::Engine;
use quick_xml::events::Event;
use quick_xml::Reader as XmlReader;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

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
        "mdsx" | "msdx" => parse_mdsx(path),
        extension => bail!("unsupported indexed slide extension: .{extension}"),
    }
}

fn parse_csp(path: &Path) -> Result<Slide> {
    const ITEM25_HEADER: &[u8] =
        b"\x02\x00\x25\x00\x0f\x00\x07\x00\x00\x00\x00\x00\x00\x00\x24\x00\x00\x00\x00\x00\x00\x00";
    const STREAM_HEADER_MARKER: &[u8] = b"\xff\xd8\xff\xe0";
    let file_size = fs::metadata(path)?.len();
    let mut file = std::fs::File::open(path)?;
    let mut header = vec![0; 4096];
    std::io::Read::read_exact(&mut file, &mut header).context("read CSP header")?;
    if !header.starts_with(b"MEDIC") {
        bail!("unsupported CSP signature");
    }
    let stream_start = header
        .windows(STREAM_HEADER_MARKER.len())
        .position(|window| window == STREAM_HEADER_MARKER)
        .context("could not locate CSP JPEG stream start")? as u64;
    let tail_start = u64_at(&header, 0x1e).context("CSP header is too small")?;
    if tail_start == 0 || tail_start >= file_size {
        bail!("invalid CSP tail offset");
    }
    use std::io::{Seek, SeekFrom};
    file.seek(SeekFrom::Start(tail_start))?;
    let mut tail = Vec::new();
    std::io::Read::read_to_end(&mut file, &mut tail)?;
    let positions = find_all(&tail, ITEM25_HEADER);
    if positions.is_empty() {
        bail!("no CSP tile records were found in tail metadata");
    }

    let mut boundaries = vec![0usize];
    for index in 0..positions.len() - 1 {
        if positions[index + 1] - positions[index] != 58 {
            boundaries.push(index + 1);
        }
    }
    boundaries.push(positions.len());
    let mut levels = Vec::new();
    let mut full_width = 0u32;
    for block in 0..boundaries.len() - 1 {
        let mut entries = HashMap::new();
        let mut width = 0u32;
        let mut height = 0u32;
        for record in boundaries[block]..boundaries[block + 1] {
            let position = positions[record] + 22;
            if position + 36 > tail.len() {
                bail!("truncated CSP tile record");
            }
            let tile_width = u32_at(&tail, position)?;
            let tile_height = u32_at(&tail, position + 4)?;
            let data = ByteRange {
                offset: stream_start + u32_at(&tail, position + 8)? as u64,
                length: u32_at(&tail, position + 16)? as u64,
            };
            let x = u32_at(&tail, position + 24)?;
            let y = u32_at(&tail, position + 28)?;
            validate_range(data, file_size, "CSP tile")?;
            entries
                .entry((x, y))
                .or_insert((data, tile_width, tile_height));
            width = width.max(x.checked_add(tile_width).context("CSP width overflow")?);
            height = height.max(y.checked_add(tile_height).context("CSP height overflow")?);
        }
        if width == 0 || height == 0 {
            bail!("invalid CSP level dimensions at block {block}");
        }
        let cols = (width + 255) / 256;
        let rows = (height + 255) / 256;
        let mut tiles = Vec::with_capacity((cols * rows) as usize);
        for row in 0..rows {
            for col in 0..cols {
                let x = col * 256;
                let y = row * 256;
                let (data, _tile_width, _tile_height) = entries.get(&(x, y)).copied().unwrap_or((
                    ByteRange {
                        offset: 0,
                        length: 0,
                    },
                    (width - x).min(256),
                    (height - y).min(256),
                ));
                tiles.push(data);
            }
        }
        if block == 0 {
            full_width = width;
        }
        levels.push(Level {
            index: block,
            width,
            height,
            downsample: full_width.max(1) as f64 / width as f64,
            tile_cols: cols,
            tile_rows: rows,
            tiles,
            tile_positions: Vec::new(),
            tile_groups: Vec::new(),
        });
    }
    let mpp = csp_float(&tail, 4, 10).unwrap_or(0.25);
    let app_mag = csp_float(&tail, 4, 9).unwrap_or(40.0);
    let associated = parse_csp_associated(&header, stream_start, file_size)?;
    Ok(Slide {
        path: PathBuf::from(path),
        metadata: Metadata {
            width: levels[0].width,
            height: levels[0].height,
            mpp,
            app_mag,
            jpeg_quality: 75,
        },
        tile_width: 256,
        tile_height: 256,
        compression: Compression::Jpeg,
        levels,
        associated_images: associated,
        thumbnail: None,
    })
}

fn parse_csp_associated(
    header: &[u8],
    stream_start: u64,
    file_size: u64,
) -> Result<Vec<AssociatedImage>> {
    let pattern = b"\x02\x00\x01\x00\x0e\x00";
    let mut images = Vec::new();
    for start in find_all(header, pattern) {
        let end = (start + 220).min(header.len());
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
        validate_range(data, file_size, "CSP associated image")?;
        images.push((width, height, data));
        if images.len() == 3 {
            break;
        }
    }
    images.sort_by_key(|(width, height, _)| *width as u64 * *height as u64);
    Ok(images
        .into_iter()
        .enumerate()
        .filter_map(|(index, (_, _, data))| {
            Some(AssociatedImage {
                kind: if index == 0 { "label" } else { "macro" }.to_owned(),
                data,
            })
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

fn csp_float(data: &[u8], group: u16, item: u16) -> Option<f64> {
    let pattern = csp_pattern(group, item, 9, 4);
    let position = find_all(data, &pattern).first().copied()? + 22;
    Some(f32::from_le_bytes(data.get(position..position + 4)?.try_into().ok()?) as f64)
}

fn csp_scalar(data: &[u8], pattern: &[u8], start: usize, end: usize) -> Option<u64> {
    let position = find_subslice(&data[start..end], pattern)? + start + 22;
    match pattern.len() {
        26 => Some(u32::from_le_bytes(data.get(position..position + 4)?.try_into().ok()?) as u64),
        30 => Some(u64::from_le_bytes(
            data.get(position..position + 8)?.try_into().ok()?,
        )),
        _ => None,
    }
}

fn parse_kfb(path: &Path) -> Result<Slide> {
    const LEVEL_STEP: i32 = 8_388_608;
    let mut reader = Reader::open(path)?;
    let file_size = reader.len();
    reader.seek(4)?;
    let version = reader.bytes(4, "KFB version")?;
    if !version.starts_with(b"KFB") {
        bail!("unsupported KFB signature");
    }
    reader.bytes(8, "KFB header")?;
    let tile_count = reader.i32()?;
    let base_height = reader.i32()?;
    let base_width = reader.i32()?;
    let scan_scale = reader.i32()? as f64;
    if !reader.bytes(4, "KFB compression")?.starts_with(b"JP") {
        bail!("unsupported KFB compression");
    }
    reader.bytes(4, "KFB reserved")?;
    let _spend_time = reader.i32()?;
    let _scan_time = reader.bytes(8, "KFB scan time")?;
    let macro_offset = reader.u32()? as u64;
    let label_offset = reader.u32()? as u64;
    let preview_offset = reader.u64()?;
    let tiles_offset = reader.u64()?;
    let mpp = reader.f32()? as f64;
    reader.bytes(8, "KFB resolution reserved")?;
    let tile_size = reader.i32()?;
    if tile_count < 0 || base_width <= 0 || base_height <= 0 || tile_size <= 0 || mpp <= 0.0 {
        bail!("invalid KFB header");
    }
    let zoom_levels = ((base_width.max(base_height) as f64).log2().ceil() as usize) + 1;
    let mut levels: Vec<Level> = (0..zoom_levels)
        .map(|index| {
            let downsample = 1u32 << index.min(31);
            let width = (base_width as u32 / downsample).max(1);
            let height = (base_height as u32 / downsample).max(1);
            Level {
                index,
                width,
                height,
                downsample: downsample as f64,
                tile_cols: (width + tile_size as u32 - 1) / tile_size as u32,
                tile_rows: (height + tile_size as u32 - 1) / tile_size as u32,
                tiles: Vec::new(),
                tile_positions: Vec::new(),
                tile_groups: Vec::new(),
            }
        })
        .collect();
    reader.seek(tiles_offset)?;
    let mut base_level_id = None;
    for _ in 0..tile_count {
        reader.bytes(4, "KFB tile reserved")?;
        let x = reader.i32()?;
        let y = reader.i32()?;
        let width = reader.i32()?;
        let height = reader.i32()?;
        let tile_id = reader.i32()?;
        let base = *base_level_id.get_or_insert(tile_id);
        let delta = base - tile_id;
        if delta < 0 || delta % LEVEL_STEP != 0 {
            bail!("invalid KFB level id mapping");
        }
        let index = (delta / LEVEL_STEP) as usize;
        if index >= levels.len() || x < 0 || y < 0 || width <= 0 || height <= 0 {
            bail!("invalid KFB tile entry");
        }
        reader.bytes(8, "KFB tile reserved")?;
        let length = reader.i32()?;
        let relative = reader.bytes(8, "KFB tile offset")?;
        let relative = i64::from_le_bytes(relative.try_into().unwrap());
        reader.bytes(20, "KFB tile tail")?;
        if length < 0 {
            bail!("invalid KFB tile range");
        }
        let absolute = tiles_offset as i128 + relative as i128;
        if absolute < 0 || absolute > u64::MAX as i128 {
            bail!("invalid KFB tile offset");
        }
        let data = ByteRange {
            offset: absolute as u64,
            length: length as u64,
        };
        validate_range(data, file_size, "KFB tile")?;
        levels[index].tiles.push(data);
        levels[index].tile_positions.push(TilePlacement {
            x: x as u32,
            y: y as u32,
            width: width as u32,
            height: height as u32,
        });
    }
    for level in &mut levels {
        let cell_count = (level.tile_cols * level.tile_rows) as usize;
        level.tile_groups = vec![Vec::new(); cell_count];
        for (tile_index, position) in level.tile_positions.iter().enumerate() {
            let left = (position.x / tile_size as u32).min(level.tile_cols.saturating_sub(1));
            let top = (position.y / tile_size as u32).min(level.tile_rows.saturating_sub(1));
            let right = ((position.x + position.width.saturating_sub(1)) / tile_size as u32)
                .min(level.tile_cols.saturating_sub(1));
            let bottom = ((position.y + position.height.saturating_sub(1)) / tile_size as u32)
                .min(level.tile_rows.saturating_sub(1));
            for row in top..=bottom {
                for col in left..=right {
                    level.tile_groups[(row * level.tile_cols + col) as usize].push(tile_index);
                }
            }
        }
    }
    let associated = [("macro", macro_offset), ("label", label_offset)]
        .into_iter()
        .filter_map(|(kind, offset)| {
            if offset == 0 {
                None
            } else {
                Some(read_kfb_image(&mut reader, offset, file_size, kind))
            }
        })
        .collect::<Result<Vec<_>>>()?;
    let thumbnail = if preview_offset == 0 {
        None
    } else {
        Some(read_kfb_thumbnail(&mut reader, preview_offset, file_size)?)
    };
    Ok(Slide {
        path: PathBuf::from(path),
        metadata: Metadata {
            width: base_width as u32,
            height: base_height as u32,
            mpp,
            app_mag: scan_scale,
            jpeg_quality: 75,
        },
        tile_width: tile_size as u32,
        tile_height: tile_size as u32,
        compression: Compression::Jpeg,
        levels,
        associated_images: associated,
        thumbnail,
    })
}

fn read_kfb_image(
    reader: &mut Reader,
    offset: u64,
    file_size: u64,
    kind: &str,
) -> Result<AssociatedImage> {
    let (width, height, data) = read_kfb_embedded(reader, offset, file_size)?;
    let _ = (width, height);
    Ok(AssociatedImage {
        kind: kind.to_owned(),
        data,
    })
}

fn read_kfb_thumbnail(reader: &mut Reader, offset: u64, file_size: u64) -> Result<Thumbnail> {
    let (width, height, data) = read_kfb_embedded(reader, offset, file_size)?;
    Ok(Thumbnail {
        width,
        height,
        data,
    })
}

fn read_kfb_embedded(
    reader: &mut Reader,
    offset: u64,
    file_size: u64,
) -> Result<(u32, u32, ByteRange)> {
    reader.seek(offset)?;
    reader.bytes(8, "KFB embedded header")?;
    let height = reader.i32()?;
    let width = reader.i32()?;
    reader.bytes(4, "KFB embedded reserved")?;
    let length = reader.i32()?;
    reader.bytes(28, "KFB embedded tail")?;
    if width <= 0 || height <= 0 || length <= 0 {
        bail!("invalid KFB embedded image entry");
    }
    let data = ByteRange {
        offset: offset + 52,
        length: length as u64,
    };
    validate_range(data, file_size, "KFB embedded image")?;
    Ok((width as u32, height as u32, data))
}

fn parse_mdsx(path: &Path) -> Result<Slide> {
    let mut reader = Reader::open(path)?;
    if reader.bytes(4, "MDSX magic")? != b"BKIO" {
        bail!("unsupported MDSX container");
    }
    let mut block_offsets = Vec::new();
    reader.seek(84)?;
    for _ in 0..5 {
        reader.bytes(8, "MDSX block header")?;
        block_offsets.push(reader.u32()? as u64);
        reader.bytes(4, "MDSX block tail")?;
    }
    reader.seek(block_offsets[0] + 20)?;
    let property_xml = read_mdsx_range(&mut reader)?;
    let macro_range = read_mdsx_tagged_range(&mut reader)?;
    let label_range = read_mdsx_tagged_range(&mut reader)?;
    let slide_xml = read_mdsx_tagged_range(&mut reader)?;
    let property_values = xml_values(&decode_mdsx_xml(&reader.range(
        property_xml.offset,
        property_xml.length,
        "MDSX property XML",
    )?)?)?;
    let matrix = parse_matrix(&decode_mdsx_xml(&reader.range(
        slide_xml.offset,
        slide_xml.length,
        "MDSX slide XML",
    )?)?)?;
    if matrix.tile_width != matrix.tile_height {
        bail!("unsupported non-square MDSX tile size");
    }
    let mut levels = Vec::new();
    for index in 0..matrix.layer_count {
        let (rows, cols) = matrix
            .layers
            .get(index)
            .copied()
            .context("missing MDSX layer")?;
        let divisor = 1u32 << index.min(31);
        let width = matrix.width.div_ceil(divisor).max(1);
        let height = matrix.height.div_ceil(divisor).max(1);
        reader.seek(164 + index as u64 * 16)?;
        reader.bytes(8, "MDSX level index header")?;
        let tiles_offset = reader.u32()? as u64;
        let tiles_length = reader.u32()? as u64;
        if tiles_length < 4 {
            bail!("invalid MDSX tile index length");
        }
        let count = (tiles_length - 4) / 10;
        if count != rows as u64 * cols as u64 {
            bail!("MDSX tile count mismatch at level {index}");
        }
        reader.seek(tiles_offset + 4)?;
        let mut tiles = Vec::with_capacity(count as usize);
        for tile_index in 0..count {
            reader.bytes(2, "MDSX tile reserved")?;
            let offset = reader.u32()? as u64;
            let length = reader.u32()? as u64;
            let data = ByteRange { offset, length };
            validate_range(data, reader.len(), "MDSX tile")?;
            let row = tile_index / cols as u64;
            let col = tile_index % cols as u64;
            let x = (col as u32) * matrix.tile_width;
            let y = (row as u32) * matrix.tile_width;
            tiles.push(data);
            let _ = (x, y);
        }
        levels.push(Level {
            index,
            width,
            height,
            downsample: 2f64.powi(index as i32),
            tile_cols: cols,
            tile_rows: rows,
            tiles,
            tile_positions: Vec::new(),
            tile_groups: Vec::new(),
        });
    }
    let base_dir = path.parent().unwrap_or_else(|| Path::new("."));
    let info = read_ini(base_dir.join("info.ini"));
    let meta = read_ini(base_dir.join("meta"));
    let mpp = first_float([
        meta.get("property.scale"),
        info.get("info.scale"),
        property_values.get("Scale"),
    ])
    .context("missing MDSX scale")?;
    let app_mag = first_float([
        meta.get("property.scanobjective"),
        info.get("info.scanlens"),
        property_values.get("ScanObjective"),
    ])
    .context("missing MDSX objective")?;
    let quality = first_int([
        meta.get("property.compressquality"),
        property_values.get("CompressQuality"),
    ])
    .unwrap_or(75)
    .clamp(1, 100) as u8;
    let associated_images = [("label", label_range), ("macro", macro_range)]
        .into_iter()
        .filter(|(_, data)| data.present())
        .map(|(kind, data)| AssociatedImage {
            kind: kind.to_owned(),
            data,
        })
        .collect();
    Ok(Slide {
        path: PathBuf::from(path),
        metadata: Metadata {
            width: matrix.width,
            height: matrix.height,
            mpp,
            app_mag,
            jpeg_quality: quality,
        },
        tile_width: matrix.tile_width,
        tile_height: matrix.tile_width,
        compression: Compression::Jpeg,
        levels,
        associated_images,
        thumbnail: None,
    })
}

fn read_mdsx_range(reader: &mut Reader) -> Result<ByteRange> {
    Ok(ByteRange {
        offset: reader.u32()? as u64,
        length: reader.u32()? as u64,
    })
}

fn read_mdsx_tagged_range(reader: &mut Reader) -> Result<ByteRange> {
    reader.bytes(6, "MDSX tag")?;
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
    if decoded.len() >= 2 && decoded[1] == 0 {
        let units: Vec<u16> = decoded
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
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

fn read_ini(path: PathBuf) -> HashMap<String, String> {
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

fn first_float(values: [Option<&String>; 3]) -> Option<f64> {
    values
        .into_iter()
        .flatten()
        .find_map(|value| value.parse().ok())
}
fn first_int(values: [Option<&String>; 2]) -> Option<i32> {
    values
        .into_iter()
        .flatten()
        .find_map(|value| value.parse().ok())
}

fn validate_range(data: ByteRange, file_size: u64, context: &str) -> Result<()> {
    if !data.present() || data.offset >= file_size || data.length > file_size - data.offset {
        bail!("invalid byte range for {context}");
    }
    Ok(())
}

fn u32_at(data: &[u8], offset: usize) -> Result<u32> {
    Ok(u32::from_le_bytes(
        data.get(offset..offset + 4)
            .context("truncated binary value")?
            .try_into()
            .unwrap(),
    ))
}
fn u64_at(data: &[u8], offset: usize) -> Result<u64> {
    Ok(u64::from_le_bytes(
        data.get(offset..offset + 8)
            .context("truncated binary value")?
            .try_into()
            .unwrap(),
    ))
}
fn find_all(data: &[u8], needle: &[u8]) -> Vec<usize> {
    if needle.is_empty() {
        return Vec::new();
    }
    data.windows(needle.len())
        .enumerate()
        .filter_map(|(index, window)| (window == needle).then_some(index))
        .collect()
}
fn find_subslice(data: &[u8], needle: &[u8]) -> Option<usize> {
    data.windows(needle.len())
        .position(|window| window == needle)
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
            .map(|level| format!(
                "L{}={}x{} ({}x{} tiles)",
                level.index, level.width, level.height, level.tile_cols, level.tile_rows
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
                .map(|image| image.kind.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        }
    );
}
