//! Format-independent description of a slide.
//!
//! Every vendor reader in this crate reports the same shape: slide metadata plus
//! one or more pyramid levels whose tiles are still on disk. Tiles are therefore
//! described as [`ByteRange`]s into the source file rather than as decoded
//! pixels, which keeps memory use flat for multi-gigabyte inputs.

use anyhow::{bail, Result};
use std::path::PathBuf;

/// A half-open `[offset, offset + length)` byte range inside a slide file.
#[derive(Clone, Copy, Debug)]
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

/// One pyramid level, stored as tiles on the source grid.
#[derive(Clone, Debug)]
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
}

/// Source rectangle of a tile that is not aligned to the regular output grid.
#[derive(Clone, Copy, Debug)]
pub struct TilePlacement {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// A label or macro image embedded in the container.
#[derive(Clone, Debug)]
pub struct AssociatedImage {
    pub kind: String,
    pub data: ByteRange,
}

/// The low-resolution preview the container already stores.
#[derive(Clone, Copy, Debug)]
pub struct Thumbnail {
    pub width: u32,
    pub height: u32,
    pub data: ByteRange,
}

/// How the tiles of one slide are compressed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Compression {
    Jpeg,
    Hevc,
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
}
