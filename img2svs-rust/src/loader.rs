//! Maps a file onto the reader that can parse it.
//!
//! Both entry points - the CLI and the GUI - reach a slide through this
//! module, so a format can never end up reachable from only one of them. The
//! table below is the single list of what the converter accepts; the file
//! dialog filter, the "supported formats" strip and the `--help` text are all
//! derived from it or kept next to it.

use crate::model::Slide;
use crate::{dmetrix, indexed, mrxs, ndpi, sdpc, tiff};
use anyhow::{bail, Context, Result};
use std::path::Path;

/// Every container the converter reads: file extension plus the label the GUI
/// shows for it. Order is the order the GUI lists them in.
pub const SUPPORTED_FORMATS: &[(&str, &str)] = &[
    ("csp", "CSP"),
    ("dmetrix", "DMETRIX"),
    ("kfb", "KFB"),
    ("mdss", "MDSS"),
    ("mdsx", "MDSX"),
    ("msdx", "MSDX"),
    ("mrxs", "MRXS"),
    ("ndpi", "NDPI"),
    ("sdpc", "SDPC"),
    ("dyqx", "DYQX"),
    ("svs", "SVS"),
    ("tif", "TIF/TIFF"),
    ("tiff", "TIF/TIFF"),
];

/// Extensions in [`SUPPORTED_FORMATS`], for the file dialog filter.
pub fn supported_extensions() -> Vec<&'static str> {
    SUPPORTED_FORMATS
        .iter()
        .map(|(extension, _)| *extension)
        .collect()
}

/// Resolves `path` and parses it with the reader for its extension.
///
/// The canonicalised path ends up in [`Slide::path`], which is what the writer
/// uses to derive a default output name next to the source.
pub fn open_slide(path: &Path) -> Result<Slide> {
    let path = path
        .canonicalize()
        .with_context(|| format!("input not found: {}", path.display()))?;
    let extension = extension_of(&path);
    match extension.as_str() {
        "dmetrix" => dmetrix::parse(&path),
        "sdpc" | "dyqx" => sdpc::parse(&path),
        "csp" | "kfb" | "mdss" | "mdsx" | "msdx" => indexed::parse(&path),
        "mrxs" => mrxs::parse(&path),
        "tif" | "tiff" | "svs" => tiff::parse(&path),
        "ndpi" => ndpi::parse(&path),
        other => bail!("unsupported input extension .{other}"),
    }
}

/// Lower-cased extension of `path`, empty when it has none.
fn extension_of(path: &Path) -> String {
    path.extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::path::PathBuf;

    #[test]
    fn extension_matching_is_case_insensitive() {
        assert_eq!(extension_of(&PathBuf::from("a/b.SVS")), "svs");
        assert_eq!(extension_of(&PathBuf::from("a/b")), "");
    }

    #[test]
    fn format_table_has_one_entry_per_extension() {
        // The GUI derives both its dialog filter and its format strip from this
        // table, so a repeated extension would show up twice in both.
        let extensions = supported_extensions();
        let unique: HashSet<&str> = extensions.iter().copied().collect();
        assert_eq!(unique.len(), extensions.len());
        assert_eq!(extensions.len(), 13);
    }
}
