//! Format-independent description of a slide.
//!
//! Every vendor reader in this crate reports the same shape: slide metadata plus
//! one or more pyramid levels whose tiles are still on disk. Tiles are therefore
//! described as [`ByteRange`]s into the source file rather than as decoded
//! pixels, which keeps memory use flat for multi-gigabyte inputs.

use anyhow::{bail, Result};
use std::path::PathBuf;

/// A half-open `[offset, offset + length)` byte range inside a slide file.
#[derive(Clone, Copy, Debug, Default)]
pub struct ByteRange {
    pub offset: u64,
    pub length: u64,
}

impl ByteRange {
    /// The range used for "the source stores no tile here".
    pub const EMPTY: Self = Self {
        offset: 0,
        length: 0,
    };

    /// Whether this range points at data at all.
    pub fn present(self) -> bool {
        self.offset > 0 && self.length > 0
    }

    /// Fails unless the range is present and fits inside `file_size`.
    ///
    /// `label` names the structure being checked (for example
    /// `"DMetrix level tile"`) so a corrupt container reports which record was
    /// unusable instead of only the raw numbers.
    pub fn validate(self, file_size: u64, label: &str) -> Result<()> {
        if !self.present() || self.offset >= file_size || self.length > file_size - self.offset {
            bail!("invalid byte range for {label}");
        }
        Ok(())
    }
}

/// Physical properties taken from the container header.
#[derive(Clone, Debug)]
pub struct Metadata {
    pub width: u32,
    pub height: u32,
    /// Micrometres per pixel of level 0.
    pub mpp: f64,
    /// Nominal objective magnification.
    pub app_mag: f64,
    /// JPEG quality to use when this slide has to be re-encoded.
    pub jpeg_quality: u8,
}

/// Storage layout of one level's tiles.
///
/// Most containers store tiles that are complete compressed streams, one per
/// grid cell, on a pitch shared by every level of the slide; for those the
/// default layout is used and [`Level::tiling`] stays empty.
///
/// Hamamatsu NDPI needs more than that. A level is a single JPEG strip cut
/// into restart intervals, and the interval is sized so that one interval is
/// one tile. Since each level halves in size while the interval keeps its
/// pixel width, every level ends up on its own tile pitch - which is why
/// OpenSlide reports `tile-width`/`tile-height` per level rather than per
/// slide. A tile is then a byte range of the strip that has to be wrapped in
/// the strip's header, with the frame size rewritten to the tile geometry,
/// plus an EOI behind it.
#[derive(Clone, Debug, Default)]
pub struct TileLayout {
    /// Tile pitch of this level. Zero means the slide-level pitch applies.
    pub tile_width: u32,
    pub tile_height: u32,
    /// Header spliced in front of every tile payload. Empty means the payload
    /// is used exactly as stored.
    pub prefix: Vec<u8>,
    /// Trailer spliced behind every tile payload.
    pub suffix: Vec<u8>,
}

impl TileLayout {
    /// Whether tiles are stored as complete streams on the slide-level pitch.
    pub fn is_plain(&self) -> bool {
        self.prefix.is_empty() && self.suffix.is_empty()
    }
}

/// One pyramid level, stored as tiles on the source grid.
#[derive(Clone, Debug, Default)]
pub struct Level {
    pub index: usize,
    pub width: u32,
    pub height: u32,
    /// Linear scale relative to level 0.
    pub downsample: f64,
    pub tile_cols: u32,
    pub tile_rows: u32,
    pub tiles: Vec<ByteRange>,
    /// Optional source coordinates for formats with non-grid tile placement.
    /// Empty means `tiles` is already row-major on the regular grid.
    pub tile_positions: Vec<TilePlacement>,
    /// Optional output-cell to source-tile map for sparse/non-grid indexes.
    pub tile_groups: Vec<Vec<usize>>,
    /// Pitch and payload wrapping for this level's tiles.
    pub tiling: TileLayout,
}

impl Level {
    /// Tile pitch this level is stored on, falling back to the slide default
    /// when the level does not override it.
    pub fn stored_tile_pitch(&self, slide_tile_width: u32, slide_tile_height: u32) -> (u32, u32) {
        (
            if self.tiling.tile_width == 0 {
                slide_tile_width
            } else {
                self.tiling.tile_width
            },
            if self.tiling.tile_height == 0 {
                slide_tile_height
            } else {
                self.tiling.tile_height
            },
        )
    }
}

/// Source rectangle of a tile that is not aligned to the regular output grid.
#[derive(Clone, Copy, Debug)]
pub struct TilePlacement {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    /// Offset into the decoded tile at which the placed rectangle starts.
    /// Zero for containers that store one placed image per tile payload.
    pub src_x: u32,
    pub src_y: u32,
}

/// A label or macro image embedded in the container.
#[derive(Clone, Debug, Default)]
pub struct AssociatedImage {
    pub kind: String,
    /// Self-contained payload, used by containers that store the image as one
    /// blob of JPEG, PNG or BMP bytes.
    pub data: ByteRange,
    /// Declared raster size, used to trim the padding a stripped TIFF leaves
    /// in its final strip.
    pub width: u32,
    pub height: u32,
    /// TIFF pages stored as several strips that share [`Self::jpeg_tables`].
    /// When non-empty these replace `data`.
    pub strips: Vec<ByteRange>,
    /// Contents of TIFF tag 347, the tables every strip of the page omits.
    pub jpeg_tables: Vec<u8>,
}

/// The low-resolution preview the container already stores.
#[derive(Clone, Debug, Default)]
pub struct Thumbnail {
    pub width: u32,
    pub height: u32,
    /// Self-contained payload, used by containers that store one blob.
    pub data: ByteRange,
    /// TIFF pages stored as several strips that share [`Self::jpeg_tables`].
    /// When non-empty these replace `data`.
    pub strips: Vec<ByteRange>,
    /// Contents of TIFF tag 347, the tables every strip of the page omits.
    pub jpeg_tables: Vec<u8>,
}

/// How the tiles of one slide are compressed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Compression {
    Jpeg,
    Hevc,
}

/// Maps each level's sparse placements onto its dense output grid.
///
/// A placement may cover several output cells, so every cell collects the
/// list of tiles that can contribute to it. Shared by the KFB and MRXS
/// readers, whose tiles sit at arbitrary pixel coordinates, and by the NDPI
/// reader's fallback for levels it cannot cut into tiles.
///
/// `tile_width` and `tile_height` are the slide-level pitch; a level that
/// stores its tiles on a different pitch overrides them.
pub fn assign_tile_groups(levels: &mut [Level], slide_tile_width: u32, slide_tile_height: u32) {
    for level in levels {
        if level.tile_positions.is_empty() {
            // Grid levels place their tiles by index, so no map is needed and
            // building one would size a vector by the whole tile pyramid.
            level.tile_groups = Vec::new();
            continue;
        }
        let (tile_width, tile_height) =
            level.stored_tile_pitch(slide_tile_width, slide_tile_height);
        let last_col = level.tile_cols.saturating_sub(1);
        let last_row = level.tile_rows.saturating_sub(1);
        level.tile_groups = vec![Vec::new(); (level.tile_cols * level.tile_rows) as usize];
        for (tile_index, position) in level.tile_positions.iter().enumerate() {
            let left = (position.x / tile_width).min(last_col);
            let top = (position.y / tile_height).min(last_row);
            let right =
                ((position.x + position.width.saturating_sub(1)) / tile_width).min(last_col);
            let bottom =
                ((position.y + position.height.saturating_sub(1)) / tile_height).min(last_row);
            for row in top..=bottom {
                for col in left..=right {
                    level.tile_groups[(row * level.tile_cols + col) as usize].push(tile_index);
                }
            }
        }
    }
}

/// One backing file of a multi-file container.
///
/// Readers describe tile payloads with virtual offsets in a single address
/// space; each `SlideSource` maps a window of that space onto a real file.
#[derive(Clone, Debug)]
pub struct SlideSource {
    pub path: PathBuf,
    /// Virtual offset at which this file's first byte appears.
    pub base: u64,
}

/// Everything the SVS writer needs in order to stream a source file.
#[derive(Clone, Debug)]
pub struct Slide {
    pub path: PathBuf,
    pub metadata: Metadata,
    pub tile_width: u32,
    pub tile_height: u32,
    pub compression: Compression,
    pub levels: Vec<Level>,
    pub associated_images: Vec<AssociatedImage>,
    pub thumbnail: Option<Thumbnail>,
    /// Backing files for containers that spread payloads across several files
    /// (e.g. MRXS `Data*.dat`). Empty means every byte range addresses
    /// `path` itself.
    pub sources: Vec<SlideSource>,
}
