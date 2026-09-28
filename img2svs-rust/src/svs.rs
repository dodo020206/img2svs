use crate::hevc::Decoder as HevcDecoder;
use crate::jpeg::{
    decode_image, decode_rgb, encode_jpeg, encode_jpeg_with_capacity, merge_jpeg_tables,
    split_jpeg_tables, thumbnail as make_thumbnail, transcode_jpeg_to_420, white_image, SOI_MARKER,
};
use crate::model::{ByteRange, Compression, Level, Slide};
use anyhow::{anyhow, bail, Context, Result};
use image::{Rgb, RgbImage};
use memmap2::MmapOptions;
use std::borrow::Cow;
use std::fs::{self, File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::{self, JoinHandle};

const APERIO_VERSION: &str = "Aperio Image Library v12.4.3";
/// Micrometres in one centimetre, the scale of the `ResolutionUnit=3` header
/// fields written into every page.
const MICROMETRES_PER_CENTIMETRE: f64 = 10_000.0;

/// Field type codes written into IFD entries, as defined by the TIFF 6.0
/// specification and its BigTIFF extension.
const TIFF_TYPE_ASCII: u16 = 2;
const TIFF_TYPE_SHORT: u16 = 3;
const TIFF_TYPE_LONG: u16 = 4;
const TIFF_TYPE_RATIONAL: u16 = 5;
const TIFF_TYPE_UNDEFINED: u16 = 7;

/// Field tags written into the pages this writer emits.
const TAG_NEW_SUBFILE_TYPE: u16 = 254;
const TAG_IMAGE_WIDTH: u16 = 256;
const TAG_IMAGE_LENGTH: u16 = 257;
const TAG_BITS_PER_SAMPLE: u16 = 258;
const TAG_COMPRESSION: u16 = 259;
const TAG_PHOTOMETRIC_INTERPRETATION: u16 = 262;
const TAG_IMAGE_DESCRIPTION: u16 = 270;
const TAG_STRIP_OFFSETS: u16 = 273;
const TAG_ORIENTATION: u16 = 274;
const TAG_SAMPLES_PER_PIXEL: u16 = 277;
const TAG_ROWS_PER_STRIP: u16 = 278;
const TAG_STRIP_BYTE_COUNTS: u16 = 279;
const TAG_X_RESOLUTION: u16 = 282;
const TAG_Y_RESOLUTION: u16 = 283;
const TAG_PLANAR_CONFIGURATION: u16 = 284;
const TAG_RESOLUTION_UNIT: u16 = 296;
const TAG_TILE_WIDTH: u16 = 322;
const TAG_TILE_LENGTH: u16 = 323;
const TAG_TILE_OFFSETS: u16 = 324;
const TAG_TILE_BYTE_COUNTS: u16 = 325;
const TAG_SAMPLE_FORMAT: u16 = 339;
const TAG_JPEG_TABLES: u16 = 347;
const TAG_YCBCR_SUB_SAMPLING: u16 = 530;
const TAG_REFERENCE_BLACK_WHITE: u16 = 532;

pub struct WriteOptions {
    pub jpeg_quality: u8,
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
    let mut input = SourceFiles::open(slide)?;
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
        MICROMETRES_PER_CENTIMETRE / slide.metadata.mpp,
        None,
    )?;
    for level in slide.levels.iter().skip(1) {
        writer.write_tiled_page(slide, level, options.jpeg_quality, true, &tile_pool)?;
    }

    for associated in &slide.associated_images {
        let image = decode_embedded(
            &mut input,
            associated.data,
            &associated.strips,
            &associated.jpeg_tables,
            (associated.width, associated.height),
        )
        .with_context(|| format!("decode associated image {}", associated.kind))?;
        writer.write_strip_page(
            &image,
            options.jpeg_quality,
            MICROMETRES_PER_CENTIMETRE / slide.metadata.mpp,
            Some(&format!("{}\r", associated.kind)),
        )?;
    }
    writer.finish()
}

/// Decodes one embedded image, whichever way the container stores it.
///
/// Vendors either hand us a single self-contained blob (JPEG, PNG or BMP), or
/// a stripped TIFF page whose strips omit the tables carried once in tag 347.
/// The stripped form is decoded strip by strip and stacked in raster order,
/// which needs no extra geometry because every strip is a valid JPEG on its
/// own once the shared tables are spliced back in.
fn decode_embedded(
    input: &mut SourceFiles,
    data: ByteRange,
    strips: &[ByteRange],
    jpeg_tables: &[u8],
    declared: (u32, u32),
) -> Result<RgbImage> {
    if strips.is_empty() {
        let bytes = input.read_range(data.offset, data.length)?;
        if bytes.is_empty() {
            bail!("embedded image has no payload");
        }
        return decode_image(&bytes);
    }
    let mut pieces = Vec::with_capacity(strips.len());
    for (index, strip) in strips.iter().enumerate() {
        let bytes = input.read_range(strip.offset, strip.length)?;
        let merged = merge_jpeg_tables(jpeg_tables, &bytes)
            .with_context(|| format!("assemble strip {index}"))?;
        let piece = decode_rgb(&merged).with_context(|| format!("decode strip {index}"))?;
        pieces.push(piece);
    }
    let stacked = stack_vertically(&pieces).context("stack stripped image")?;
    Ok(trim_to_declared(stacked, declared))
}

/// Stacks decoded strips into the page they belong to.
fn stack_vertically(pieces: &[RgbImage]) -> Result<RgbImage> {
    let width = pieces
        .first()
        .context("stripped image has no strips")?
        .width();
    let height: u32 = pieces.iter().map(RgbImage::height).sum();
    if width == 0 || height == 0 {
        bail!("stripped image has no pixels");
    }
    let mut image = RgbImage::new(width, height);
    let mut top = 0u32;
    for piece in pieces {
        let copy_width = piece.width().min(width);
        for y in 0..piece.height() {
            for x in 0..copy_width {
                image.put_pixel(x, top + y, *piece.get_pixel(x, y));
            }
        }
        top += piece.height();
    }
    Ok(image)
}

/// Crops a stacked page back to the size its directory declares.
///
/// A stripped TIFF pads the final strip out to `RowsPerStrip`, so the decoded
/// pieces together are taller than the page; without this the label and macro
/// images come out with a band of padding along the bottom.
fn trim_to_declared(image: RgbImage, declared: (u32, u32)) -> RgbImage {
    let (width, height) = declared;
    if width == 0 || height == 0 || (image.width() == width && image.height() == height) {
        return image;
    }
    let width = width.min(image.width());
    let height = height.min(image.height());
    if width == 0 || height == 0 {
        return image;
    }
    let mut trimmed = RgbImage::new(width, height);
    image::imageops::replace(&mut trimmed, &image, 0, 0);
    trimmed
}

/// Read handles over the virtual source space of a slide.
///
/// Backing files of a slide, ordered by the virtual offset of their first
/// byte.
///
/// Single-file slides hold exactly one entry whose base is zero; multi-file
/// containers (MRXS `Data*.dat`) hold one entry per file, and byte ranges are
/// resolved against those virtual base offsets.
fn backing_files(slide: &Slide) -> Vec<(u64, PathBuf)> {
    if slide.sources.is_empty() {
        return vec![(0, slide.path.clone())];
    }
    let mut files: Vec<(u64, PathBuf)> = slide
        .sources
        .iter()
        .map(|source| (source.base, source.path.clone()))
        .collect();
    files.sort_by_key(|(base, _)| *base);
    files
}

/// Opens a file, naming it in the error so a missing `Data*.dat` of a
/// multi-file container is identifiable.
fn open_backing_file(path: &Path) -> Result<File> {
    File::open(path).with_context(|| format!("open input {}", path.display()))
}

/// Seeking readers over the backing files of a slide.
struct SourceFiles {
    files: Vec<(u64, File)>,
}

impl SourceFiles {
    fn open(slide: &Slide) -> Result<Self> {
        let files = backing_files(slide)
            .into_iter()
            .map(|(base, path)| Ok((base, open_backing_file(&path)?)))
            .collect::<Result<Vec<_>>>()?;
        Ok(Self { files })
    }

    /// Reads one tile payload, splicing the level's wrapper around it.
    ///
    /// Containers that store complete streams per tile return the bytes as
    /// they are on disk; the wrapper only exists for levels cut out of a
    /// single JPEG strip.
    fn read_tile(&mut self, level: &Level, range: ByteRange) -> Result<Vec<u8>> {
        if level.tiling.is_plain() || !range.present() {
            return self.read_range(range.offset, range.length);
        }
        let body = self.read_range(range.offset, range.length)?;
        Ok(splice_tile(
            &level.tiling.prefix,
            &body,
            &level.tiling.suffix,
        ))
    }

    fn read_range(&mut self, offset: u64, length: u64) -> Result<Vec<u8>> {
        if length == 0 {
            return Ok(Vec::new());
        }
        let size = usize::try_from(length).context("tile is too large for this platform")?;
        let index = self
            .files
            .partition_point(|(base, _)| *base <= offset)
            .checked_sub(1)
            .context("source range precedes the first backing file")?;
        let (base, file) = &mut self.files[index];
        file.seek(SeekFrom::Start(offset - *base))?;
        let mut bytes = vec![0; size];
        std::io::Read::read_exact(file, &mut bytes)?;
        Ok(bytes)
    }
}

fn render_thumbnail(
    slide: &Slide,
    input: &mut SourceFiles,
    quality: u8,
    hevc: Option<&mut HevcDecoder>,
) -> Result<RgbImage> {
    // An unusable embedded thumbnail is not fatal: falling through to rendering
    // the smallest level costs time but always yields an image.
    if let Some(thumbnail) = &slide.thumbnail {
        if let Ok(image) = decode_embedded(
            input,
            thumbnail.data,
            &thumbnail.strips,
            &thumbnail.jpeg_tables,
            (thumbnail.width, thumbnail.height),
        ) {
            return Ok(make_thumbnail(&image, 1024));
        }
    }
    let level = slide.levels.last().context("slide has no pyramid levels")?;
    let image = render_level(slide, input, level, quality, hevc)?;
    Ok(make_thumbnail(&image, 1024))
}

fn render_level(
    slide: &Slide,
    input: &mut SourceFiles,
    level: &Level,
    _quality: u8,
    mut hevc: Option<&mut HevcDecoder>,
) -> Result<RgbImage> {
    let mut canvas = white_image(level.width, level.height);
    let pitch = level.stored_tile_pitch(slide.tile_width, slide.tile_height);
    for (index, range) in level.tiles.iter().enumerate() {
        if !range.present() {
            continue;
        }
        let bytes = input.read_tile(level, *range)?;
        let tile = decode_tile(slide, &bytes, hevc.as_deref_mut())?;
        if let Some(position) = level.tile_positions.get(index) {
            copy_region(
                &mut canvas,
                &tile,
                position.x,
                position.y,
                position.src_x,
                position.src_y,
                position.width,
                position.height,
            );
        } else {
            let row = index as u32 / level.tile_cols;
            let col = index as u32 % level.tile_cols;
            copy_region(
                &mut canvas,
                &tile,
                col * pitch.0,
                row * pitch.1,
                0,
                0,
                tile.width(),
                tile.height(),
            );
        }
    }
    Ok(canvas)
}

/// Wraps a tile payload in the header and trailer its level needs.
///
/// Only levels stored as restart intervals of one JPEG strip use this; every
/// other container puts a complete stream on disk and passes `prefix` and
/// `suffix` empty, in which case the body is returned unchanged.
fn splice_tile(prefix: &[u8], body: &[u8], suffix: &[u8]) -> Vec<u8> {
    if prefix.is_empty() && suffix.is_empty() {
        return body.to_vec();
    }
    let mut bytes = Vec::with_capacity(prefix.len() + body.len() + suffix.len());
    bytes.extend_from_slice(prefix);
    bytes.extend_from_slice(body);
    bytes.extend_from_slice(suffix);
    bytes
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

/// Copies a rectangle of `src` into `dst`, clipping to both images.
///
/// `src_x`/`src_y` select the top-left corner inside `src`; the copy stops at
/// whichever edge - source or destination - is reached first.
fn copy_region(
    dst: &mut RgbImage,
    src: &RgbImage,
    left: u32,
    top: u32,
    src_x: u32,
    src_y: u32,
    width: u32,
    height: u32,
) {
    if left >= dst.width() || top >= dst.height() {
        return;
    }
    let width = width
        .min(src.width().saturating_sub(src_x))
        .min(dst.width() - left);
    let height = height
        .min(src.height().saturating_sub(src_y))
        .min(dst.height() - top);
    for y in 0..height {
        for x in 0..width {
            dst.put_pixel(left + x, top + y, *src.get_pixel(src_x + x, src_y + y));
        }
    }
}

/// Memory-mapped view over the virtual source space of a slide.
struct SourceMaps {
    maps: Vec<(u64, memmap2::Mmap)>,
}

impl SourceMaps {
    fn open(slide: &Slide) -> Result<Self> {
        let maps = backing_files(slide)
            .into_iter()
            .map(|(base, path)| {
                let file = open_backing_file(&path)?;
                // SAFETY: the converter opens the source read-only and never
                // mutates it while this mapping is alive.
                let map = unsafe { MmapOptions::new().map(&file)? };
                Ok((base, map))
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Self { maps })
    }

    /// Reads one tile payload, splicing the level's wrapper around it.
    ///
    /// The common case - a container that stores complete streams - borrows
    /// from the mapping, so a multi-gigabyte conversion does not copy every
    /// tile twice.
    fn read_tile<'a>(&'a self, level: &Level, range: ByteRange) -> Result<Cow<'a, [u8]>> {
        if level.tiling.is_plain() || !range.present() {
            return Ok(Cow::Borrowed(self.range(range)?));
        }
        let body = self.range(range)?;
        Ok(Cow::Owned(splice_tile(
            &level.tiling.prefix,
            body,
            &level.tiling.suffix,
        )))
    }

    fn range(&self, range: ByteRange) -> Result<&[u8]> {
        let start = usize::try_from(range.offset).context("tile offset exceeds this platform")?;
        let length = usize::try_from(range.length).context("tile length exceeds this platform")?;
        let index = self
            .maps
            .partition_point(|(base, _)| *base <= range.offset)
            .checked_sub(1)
            .context("tile range precedes the first backing file")?;
        let (base, map) = &self.maps[index];
        let local = start - usize::try_from(*base).context("source base overflows")?;
        let end = local.checked_add(length).context("tile range overflow")?;
        map.get(local..end).context("tile range exceeds input file")
    }
}

struct TiffWriter {
    file: File,
    /// Position of the 4-byte next-directory pointer of the previous IFD,
    /// patched once the following directory has been written.
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
            previous_next_pointer: None,
        })
    }

    fn begin_ifd(&mut self) -> Result<u64> {
        let offset = self.file.stream_position()?;
        // The header's own pointer is at byte 4; later directories are chained
        // through the previous directory's next-pointer slot.
        let pointer = self.previous_next_pointer.unwrap_or(4);
        self.patch_u32(
            pointer,
            u32::try_from(offset).context("TIFF exceeds classic 4 GiB offsets")?,
        )?;
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
        let (pitch_width, pitch_height) =
            level.stored_tile_pitch(slide.tile_width, slide.tile_height);
        // The SVS tile payload has to start on a multiple of 16 in both axes,
        // so tiles are merged until the pitch is one. Reading (rather than
        // scaling) keeps the pixels identical to the source.
        let merge_cols = 16 / gcd(pitch_width, 16);
        let merge_rows = 16 / gcd(pitch_height, 16);
        let tile_width = pitch_width * merge_cols;
        let tile_height = pitch_height * merge_rows;
        let output_cols = level.tile_cols.div_ceil(merge_cols);
        let output_rows = level.tile_rows.div_ceil(merge_rows);
        let mut offsets = Vec::with_capacity((output_cols * output_rows) as usize);
        let mut counts = Vec::with_capacity(offsets.capacity());

        let total = usize::try_from(output_cols as u64 * output_rows as u64)
            .context("output tile count exceeds this platform")?;
        // Tiles we re-encode are written as abbreviated streams; the shared
        // quantization/Huffman tables ride once in the IFD's JPEGTables tag.
        // Pass-through tiles keep their own embedded tables, which TIFF
        // Compression=7 permits on a per-tile basis.
        let jpeg_tables = {
            let probe = white_image(16, 16);
            let (tables, _) = split_jpeg_tables(&encode_jpeg(&probe, quality)?)?;
            tables
        };
        for batch_start in (0..total).step_by(tile_pool.batch_size()) {
            let batch_end = (batch_start + tile_pool.batch_size()).min(total);
            let tasks: Vec<_> = (batch_start..batch_end)
                .map(|index| TileTask {
                    slot: index - batch_start,
                    level_index: level.index,
                    placement: TilePlacement {
                        output_row: index as u32 / output_cols,
                        output_col: index as u32 % output_cols,
                        merge_rows,
                        merge_cols,
                        output_width: tile_width,
                        output_height: tile_height,
                    },
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
        let resolution = MICROMETRES_PER_CENTIMETRE
            / slide.metadata.mpp
            / if reduced { level.downsample } else { 1.0 };
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
            jpeg_tables: Some(jpeg_tables),
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
        let extras_offset = ifd_offset + page.ifd_size();
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
    placement: TilePlacement,
    quality: u8,
}

/// Position and extent of one output tile inside the merged tile grid.
#[derive(Clone, Copy)]
struct TilePlacement {
    output_row: u32,
    output_col: u32,
    merge_rows: u32,
    merge_cols: u32,
    output_width: u32,
    output_height: u32,
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
        // Measured limits on the reference machine: JPEG decoding scales to all
        // cores, the HEVC decoder holds more state per worker so it is capped
        // lower and leaves one core to the writer.
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
        let source = Arc::new(SourceMaps::open(slide.as_ref())?);
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
        let mut ordered: Vec<Option<Vec<u8>>> = vec![None; tasks.len()];
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
    source: Arc<SourceMaps>,
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
    source: &SourceMaps,
    task: TileTask,
    hevc: Option<&mut HevcDecoder>,
) -> Result<Vec<u8>> {
    let level = slide
        .levels
        .get(task.level_index)
        .context("invalid pyramid level")?;
    let place = task.placement;
    let passthrough = place.merge_rows == 1
        && place.merge_cols == 1
        && slide.compression == Compression::Jpeg
        // Wrapped tiles are restart intervals, not streams, so they always
        // have to be re-encoded rather than copied.
        && level.tiling.is_plain()
        && level.tile_positions.is_empty()
        && task.quality == slide.metadata.jpeg_quality
        && place.output_row + 1 < level.tile_rows
        && place.output_col + 1 < level.tile_cols;
    if passthrough {
        let range = *level
            .tiles
            .get((place.output_row * level.tile_cols + place.output_col) as usize)
            .context("invalid source tile index")?;
        let bytes = source.range(range)?;
        if bytes.is_empty() {
            return encode_tile_image(
                &compose_tile(slide, source, level, place, hevc)?,
                task.quality,
            );
        }
        if !jpeg_is_420(bytes) {
            // Lossless transcode keeps the source quantization tables, which
            // differ from the shared JPEGTables tag — keep the full stream.
            let encoded = transcode_jpeg_to_420(bytes, task.quality).or_else(|_| {
                encode_jpeg_with_capacity(&decode_rgb(bytes)?, task.quality, bytes.len())
            })?;
            return Ok(encoded);
        }
        return Ok(bytes.to_vec());
    }
    let image = compose_tile(slide, source, level, place, hevc)?;
    encode_tile_image(&image, task.quality)
}

/// Encodes a composed tile, memoizing the output when it is uniformly white.
///
/// Slides whose scan area covers a fraction of the canvas (MRXS in
/// particular) produce hundreds of thousands of identical white tiles; the
/// JPEG for a given geometry and quality is byte-identical, so it is encoded
/// once per worker thread instead of once per tile.
fn encode_tile_image(image: &RgbImage, quality: u8) -> Result<Vec<u8>> {
    const WHITE: [u8; 3] = [255, 255, 255];
    if !image
        .as_raw()
        .as_chunks::<3>()
        .0
        .iter()
        .all(|&p| p == WHITE)
    {
        // Re-encoded tiles are written as abbreviated streams (tables live in
        // the shared JPEGTables tag), matching Aperio's layout.
        return Ok(split_jpeg_tables(&encode_jpeg(image, quality)?)?.1);
    }
    thread_local! {
        static WHITE_TILE_CACHE: std::cell::RefCell<
            std::collections::HashMap<(u8, u32, u32), Vec<u8>>,
        > = std::cell::RefCell::new(std::collections::HashMap::new());
    }
    WHITE_TILE_CACHE.with(|cache| {
        let key = (quality, image.width(), image.height());
        let mut cache = cache.borrow_mut();
        if let Some(encoded) = cache.get(&key) {
            return Ok(encoded.clone());
        }
        // Cache the abbreviated stream; tables are shared via JPEGTables.
        let encoded = split_jpeg_tables(&encode_jpeg(image, quality)?)?.1;
        cache.insert(key, encoded.clone());
        Ok(encoded)
    })
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
        jpeg_tables: Option<Vec<u8>>,
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
            Page::Tiled { jpeg_tables, .. } => 20 + usize::from(jpeg_tables.is_some()),
            Page::Strip { description, .. } => {
                if description.is_some() {
                    18
                } else {
                    17
                }
            }
        }
    }

    /// Bytes the directory itself occupies: entry count, entries, and the
    /// next-directory pointer. Everything the entries point at lives behind it.
    fn ifd_size(&self) -> u64 {
        2 + self.entry_count() as u64 * 12 + 4
    }

    /// Offsets of the payload the directory's entries point at.
    ///
    /// `extras_offset` is the position right behind the directory, which is
    /// where [`Self::write_extra`] starts writing and what
    /// [`Self::entries`] has to agree with.
    fn extra_data(&self, extras_offset: u64) -> Extra {
        let start = extras_offset;
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
                Extra {
                    bits,
                    tile_offsets,
                    tile_counts,
                    desc,
                    xres,
                    yres,
                    sample,
                    reference_bw,
                    tables: reference_bw + 48,
                    desc_len: description.len(),
                }
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
                Extra {
                    bits,
                    tile_offsets: 0,
                    tile_counts: 0,
                    desc,
                    xres,
                    yres,
                    sample,
                    reference_bw,
                    tables: 0,
                    desc_len: description.as_ref().map_or(0, String::len),
                }
            }
        }
    }

    fn entries(&self, extra_offset: u64) -> Result<Vec<Entry>> {
        let extra = self.extra_data(extra_offset);
        let mut e = match self {
            Page::Tiled {
                width,
                height,
                tile_width,
                tile_height,
                offsets,
                counts,
                reduced,
                jpeg_tables,
                ..
            } => {
                let mut entries = vec![
                    long(TAG_NEW_SUBFILE_TYPE, if *reduced { 1 } else { 0 }),
                    long(TAG_IMAGE_WIDTH, *width),
                    long(TAG_IMAGE_LENGTH, *height),
                    short_array_at(TAG_BITS_PER_SAMPLE, extra.bits),
                    short(TAG_COMPRESSION, 7),
                    short(TAG_PHOTOMETRIC_INTERPRETATION, 6),
                    short(TAG_ORIENTATION, 1),
                    short(TAG_SAMPLES_PER_PIXEL, 3),
                    short(TAG_PLANAR_CONFIGURATION, 1),
                    short(TAG_RESOLUTION_UNIT, 3),
                    long(TAG_TILE_WIDTH, *tile_width),
                    long(TAG_TILE_LENGTH, *tile_height),
                    long_array_at(TAG_TILE_OFFSETS, offsets, extra.tile_offsets)?,
                    long_array_at(TAG_TILE_BYTE_COUNTS, counts, extra.tile_counts)?,
                    rational_at(TAG_X_RESOLUTION, extra.xres),
                    rational_at(TAG_Y_RESOLUTION, extra.yres),
                    short_array_at(TAG_SAMPLE_FORMAT, extra.sample),
                    ascii_at(TAG_IMAGE_DESCRIPTION, extra.desc, extra.desc_len + 1)?,
                    short_pair(TAG_YCBCR_SUB_SAMPLING, 2, 2),
                    rational_array_at(TAG_REFERENCE_BLACK_WHITE, extra.reference_bw, 6)?,
                ];
                if let Some(tables) = jpeg_tables {
                    entries.push(Entry {
                        tag: TAG_JPEG_TABLES,
                        kind: TIFF_TYPE_UNDEFINED,
                        count: u32::try_from(tables.len()).context("JPEGTables is too large")?,
                        value: u32::try_from(extra.tables)
                            .context("TIFF exceeds classic 4 GiB offsets")?,
                    });
                }
                entries
            }
            Page::Strip {
                width,
                height,
                offset,
                count,
                description,
                ..
            } => {
                let mut v = vec![
                    long(TAG_IMAGE_WIDTH, *width),
                    long(TAG_IMAGE_LENGTH, *height),
                    short_array_at(TAG_BITS_PER_SAMPLE, extra.bits),
                    short(TAG_COMPRESSION, 7),
                    short(TAG_PHOTOMETRIC_INTERPRETATION, 6),
                    short(TAG_ORIENTATION, 1),
                    short(TAG_SAMPLES_PER_PIXEL, 3),
                    short(TAG_PLANAR_CONFIGURATION, 1),
                    long(TAG_ROWS_PER_STRIP, *height),
                    long(TAG_STRIP_OFFSETS, *offset),
                    long(TAG_STRIP_BYTE_COUNTS, *count),
                    short(TAG_RESOLUTION_UNIT, 3),
                    rational_at(TAG_X_RESOLUTION, extra.xres),
                    rational_at(TAG_Y_RESOLUTION, extra.yres),
                    short_array_at(TAG_SAMPLE_FORMAT, extra.sample),
                    short_pair(TAG_YCBCR_SUB_SAMPLING, 2, 2),
                    rational_array_at(TAG_REFERENCE_BLACK_WHITE, extra.reference_bw, 6)?,
                ];
                if description.is_some() {
                    v.push(ascii_at(
                        TAG_IMAGE_DESCRIPTION,
                        extra.desc,
                        extra.desc_len + 1,
                    )?);
                }
                v
            }
        };
        // TIFF requires the directory to be sorted by tag.
        e.sort_by_key(|entry| entry.tag);
        Ok(e)
    }

    /// Writes the payload every entry in this page points at.
    ///
    /// The layout comes from [`Self::extra_data`], the same source the IFD
    /// entries are built from, so the offsets stored in the directory cannot
    /// drift away from the bytes written here. `stream_position` is the end of
    /// the directory, which is exactly where `extra_data` starts counting.
    fn write_extra(&self, file: &mut File) -> Result<()> {
        let extra = self.extra_data(file.stream_position()?);
        // BITS_PER_SAMPLE: three 8-bit samples, hence the 6 bytes.
        pad_to(file, extra.bits)?;
        file.write_all(&[8, 0, 8, 0, 8, 0])?;
        match self {
            Page::Tiled {
                offsets,
                counts,
                description,
                jpeg_tables,
                resolution,
                ..
            } => {
                pad_to(file, extra.tile_offsets)?;
                for value in offsets {
                    file.write_all(&value.to_le_bytes())?;
                }
                for value in counts {
                    file.write_all(&value.to_le_bytes())?;
                }
                // The description is NUL terminated, which is the +1 in
                // `extra.xres`.
                file.write_all(description.as_bytes())?;
                file.write_all(&[0])?;
                pad_to(file, extra.xres)?;
                write_rational(file, *resolution)?;
                write_rational(file, *resolution)?;
                write_sample_format(file)?;
                pad_to(file, extra.reference_bw)?;
                write_reference_black_white(file)?;
                if let Some(tables) = jpeg_tables {
                    file.write_all(tables)?;
                }
            }
            Page::Strip {
                description,
                resolution,
                ..
            } => {
                if let Some(text) = description {
                    file.write_all(text.as_bytes())?;
                    file.write_all(&[0])?;
                }
                pad_to(file, extra.xres)?;
                write_rational(file, *resolution)?;
                write_rational(file, *resolution)?;
                write_sample_format(file)?;
                pad_to(file, extra.reference_bw)?;
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
    tables: u64,
    /// Length of the image description in bytes, without its NUL terminator.
    desc_len: usize,
}

fn compose_tile(
    slide: &Slide,
    source: &SourceMaps,
    level: &Level,
    placement: TilePlacement,
    mut hevc: Option<&mut HevcDecoder>,
) -> Result<RgbImage> {
    let TilePlacement {
        output_row,
        output_col,
        merge_rows,
        merge_cols,
        output_width,
        output_height,
    } = placement;
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
            let bytes = source.read_tile(level, range)?;
            let image = decode_tile(slide, bytes.as_ref(), hevc.as_deref_mut())?;
            let available_width = image.width().saturating_sub(position.src_x);
            let available_height = image.height().saturating_sub(position.src_y);
            let source_width = position.width.min(available_width);
            let source_height = position.height.min(available_height);
            let right = (position.x + source_width).min(limit_x);
            let bottom = (position.y + source_height).min(limit_y);
            let left = origin_x.max(position.x);
            let top = origin_y.max(position.y);
            if left >= right || top >= bottom {
                continue;
            }
            let src_left = left - position.x + position.src_x;
            let src_top = top - position.y + position.src_y;
            copy_region(
                &mut output,
                &image,
                left - origin_x,
                top - origin_y,
                src_left,
                src_top,
                right - left,
                bottom - top,
            );
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
            let data = source.read_tile(level, range)?;
            let image = decode_tile(slide, data.as_ref(), hevc.as_deref_mut())?;
            let (pitch_width, pitch_height) =
                level.stored_tile_pitch(slide.tile_width, slide.tile_height);
            copy_region(
                &mut output,
                &image,
                inner_col * pitch_width,
                inner_row * pitch_height,
                0,
                0,
                image.width(),
                image.height(),
            );
        }
    }
    Ok(output)
}

/// Builds the Aperio image description.
///
/// The shape - a first line with the library version, then
/// `WxH [origin WxH] (tile WxH) codec/space Q=…|AppMag = …|MPP = …` - is
/// Aperio's own format, not a free-form string: OpenSlide and other SVS
/// readers parse the `|`-separated fields to recover magnification and
/// microns-per-pixel.
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

/// Whether a JPEG stream already carries 4:2:0 sampling.
///
/// Pass-through tiles are only copied when they are already in the sampling
/// the SVS header declares (`YCbCrSubSampling = 2,2`); anything else has to be
/// transcoded. The check walks the marker chain to the frame header and reads
/// the three components' sampling factors: luma `0x22`, chroma `0x11`.
fn jpeg_is_420(data: &[u8]) -> bool {
    if !data.starts_with(&SOI_MARKER) {
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
        // SOI, EOI and the restart markers carry no length field.
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
    value.div_ceil(alignment) * alignment
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
/// SAMPLE_FORMAT for three unsigned samples.
fn write_sample_format(file: &mut File) -> Result<()> {
    for _ in 0..3 {
        file.write_all(&1u16.to_le_bytes())?;
    }
    Ok(())
}

fn short(tag: u16, value: u16) -> Entry {
    Entry {
        tag,
        kind: TIFF_TYPE_SHORT,
        count: 1,
        value: value as u32,
    }
}
fn short_pair(tag: u16, first: u16, second: u16) -> Entry {
    Entry {
        tag,
        kind: TIFF_TYPE_SHORT,
        count: 2,
        value: first as u32 | ((second as u32) << 16),
    }
}
fn short_array_at(tag: u16, offset: u64) -> Entry {
    Entry {
        tag,
        kind: TIFF_TYPE_SHORT,
        count: 3,
        value: offset as u32,
    }
}
fn long(tag: u16, value: u32) -> Entry {
    Entry {
        tag,
        kind: TIFF_TYPE_LONG,
        count: 1,
        value,
    }
}
fn rational_at(tag: u16, offset: u64) -> Entry {
    Entry {
        tag,
        kind: TIFF_TYPE_RATIONAL,
        count: 1,
        value: offset as u32,
    }
}
fn rational_array_at(tag: u16, offset: u64, count: u32) -> Result<Entry> {
    Ok(Entry {
        tag,
        kind: TIFF_TYPE_RATIONAL,
        count,
        value: u32::try_from(offset).context("TIFF rational array offset overflow")?,
    })
}
fn array_at(tag: u16, count: usize, offset: u64) -> Result<Entry> {
    Ok(Entry {
        tag,
        kind: TIFF_TYPE_LONG,
        count: u32::try_from(count)?,
        value: u32::try_from(offset).context("TIFF extra data offset overflow")?,
    })
}
fn long_array_at(tag: u16, values: &[u32], offset: u64) -> Result<Entry> {
    if values.len() == 1 {
        Ok(long(tag, values[0]))
    } else {
        array_at(tag, values.len(), offset)
    }
}
fn ascii_at(tag: u16, offset: u64, count: usize) -> Result<Entry> {
    Ok(Entry {
        tag,
        kind: TIFF_TYPE_ASCII,
        count: u32::try_from(count)?,
        value: u32::try_from(offset).context("TIFF description offset overflow")?,
    })
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
            jpeg_tables: None,
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
                tiling: Default::default(),
            }],
            associated_images: Vec::new(),
            thumbnail: None,
            sources: Vec::new(),
        };
        let pool = TilePool::with_worker_count(&slide, 4)?;
        let tasks: Vec<_> = (0..4)
            .map(|index| TileTask {
                slot: index,
                level_index: 0,
                placement: TilePlacement {
                    output_row: index as u32 / 2,
                    output_col: index as u32 % 2,
                    merge_rows: 1,
                    merge_cols: 1,
                    output_width: 16,
                    output_height: 16,
                },
                quality: 75,
            })
            .collect();
        let encoded = pool.encode_batch(&tasks)?;
        drop(pool);
        fs::remove_file(path)?;

        // Re-encoded tiles come back as abbreviated streams; rejoin them with
        // the shared tables (as a TIFF reader would via JPEGTables) to decode.
        let (tables, _) = split_jpeg_tables(&encode_jpeg(
            &RgbImage::from_pixel(16, 16, Rgb([255, 255, 255])),
            75,
        )?)?;
        let has_dqt = |data: &[u8]| data.windows(2).any(|w| w == [0xff, 0xdb]);
        for (data, expected) in encoded.iter().zip(colors) {
            let full = if has_dqt(data) {
                data.clone()
            } else {
                let mut joined = tables.clone();
                joined.truncate(joined.len() - 2);
                joined.extend_from_slice(&data[2..]);
                joined
            };
            let image = decode_rgb(&full)?;
            let pixel = image.get_pixel(8, 8);
            for channel in pixel.0 {
                assert!((channel as i16 - expected as i16).abs() <= 3);
            }
        }
        Ok(())
    }
}
