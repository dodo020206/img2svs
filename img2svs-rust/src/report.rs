//! Human-readable slide summary printed by the `--info` flag.
//!
//! All container readers funnel through this module so the same slide is
//! described identically no matter which vendor format it came from.

use crate::model::Slide;

/// Prints dimensions, pyramid layout and associated pages for `slide`.
pub fn print_slide(slide: &Slide) {
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
    println!("Pyr   : {}", pyramid_summary(slide));
    println!("Assoc : {}", associated_summary(slide));
}

/// Renders every level as `L0=1024x768 (4x3 tiles)`, separated by commas.
fn pyramid_summary(slide: &Slide) -> String {
    slide
        .levels
        .iter()
        .map(|level| {
            format!(
                "L{}={}x{} ({}x{} tiles)",
                level.index, level.width, level.height, level.tile_cols, level.tile_rows
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Lists the associated image kinds, or `none` when the container has none.
fn associated_summary(slide: &Slide) -> String {
    if slide.associated_images.is_empty() {
        return "none".to_owned();
    }
    slide
        .associated_images
        .iter()
        .map(|image| image.kind.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}
