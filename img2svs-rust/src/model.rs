use std::path::PathBuf;

#[derive(Clone, Copy, Debug)]
pub struct ByteRange {
    pub offset: u64,
    pub length: u64,
}

impl ByteRange {
    pub fn present(self) -> bool {
        self.offset > 0 && self.length > 0
    }
}

#[derive(Clone, Debug)]
pub struct Metadata {
    pub width: u32,
    pub height: u32,
    pub mpp: f64,
    pub app_mag: f64,
    pub jpeg_quality: u8,
}

#[derive(Clone, Debug)]
pub struct Level {
    pub index: usize,
    pub width: u32,
    pub height: u32,
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

#[derive(Clone, Copy, Debug)]
pub struct TilePlacement {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Debug)]
pub struct AssociatedImage {
    pub kind: String,
    pub data: ByteRange,
}

#[derive(Clone, Copy, Debug)]
pub struct Thumbnail {
    pub width: u32,
    pub height: u32,
    pub data: ByteRange,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Compression {
    Jpeg,
    Hevc,
}

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
