//! Maps a file onto the reader that can parse it.
//!
//! Both entry points - the CLI and the GUI - reach a slide through this
//! module, so a format can never end up reachable from only one of them.
//!
//! [`SUPPORTED_FORMATS`] is the single list of what the converter accepts:
//! [`open_slide`] dispatches on it, and the GUI derives its file dialog filter
//! and its "supported formats" strip from it. The CLI's `--help` text is the
//! one place that repeats the extensions, because `clap` needs them as a
//! compile-time constant.

use crate::model::Slide;
use crate::{dmetrix, indexed, mrxs, ndpi, sdpc, tiff};
use anyhow::{bail, Context, Result};
use std::path::Path;

/// The parser that reads one family of containers.
///
/// Several extensions share a reader - `.sdpc` and `.dyqx` are both SDPC,
/// `.csp`/`.kfb`/`.mdss`/`.mdsx`/`.msdx` are all indexed - so a format row
/// names its reader rather than carrying a function pointer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Reader {
    Dmetrix,
    Sdpc,
    Indexed,
    Mrxs,
    Tiff,
    Ndpi,
}

impl Reader {
    fn parse(self, path: &Path) -> Result<Slide> {
        match self {
            Self::Dmetrix => dmetrix::parse(path),
            Self::Sdpc => sdpc::parse(path),
            Self::Indexed => indexed::parse(path),
            Self::Mrxs => mrxs::parse(path),
            Self::Tiff => tiff::parse(path),
            Self::Ndpi => ndpi::parse(path),
        }
    }
}

/// One accepted container format.
pub(crate) struct Format {
    /// File extension without the leading dot, lower-cased.
    pub(crate) extension: &'static str,
    /// Label the GUI shows for it. A label names the reader rather than the
    /// file name, so two extensions may share one: `.tif` and `.tiff` are both
    /// "TIF/TIFF" to a user. Only the GUI build reads it, and it stays in the
    /// table either way so the list reads the same in both variants.
    #[cfg_attr(not(feature = "gui"), allow(dead_code))]
    pub(crate) label: &'static str,
    reader: Reader,
}

/// Every container the converter reads, in the order the GUI lists them.
pub(crate) const SUPPORTED_FORMATS: &[Format] = &[
    Format {
        extension: "csp",
        label: "CSP",
        reader: Reader::Indexed,
    },
    Format {
        extension: "dmetrix",
        label: "DMETRIX",
        reader: Reader::Dmetrix,
    },
    Format {
        extension: "kfb",
        label: "KFB",
        reader: Reader::Indexed,
    },
    Format {
        extension: "mdss",
        label: "MDSS",
        reader: Reader::Indexed,
    },
    Format {
        extension: "mdsx",
        label: "MDSX",
        reader: Reader::Indexed,
    },
    Format {
        extension: "msdx",
        label: "MSDX",
        reader: Reader::Indexed,
    },
    Format {
        extension: "mrxs",
        label: "MRXS",
        reader: Reader::Mrxs,
    },
    Format {
        extension: "ndpi",
        label: "NDPI",
        reader: Reader::Ndpi,
    },
    Format {
        extension: "sdpc",
        label: "SDPC",
        reader: Reader::Sdpc,
    },
    Format {
        extension: "dyqx",
        label: "DYQX",
        reader: Reader::Sdpc,
    },
    Format {
        extension: "svs",
        label: "SVS",
        reader: Reader::Tiff,
    },
    Format {
        extension: "tif",
        label: "TIF/TIFF",
        reader: Reader::Tiff,
    },
    Format {
        extension: "tiff",
        label: "TIF/TIFF",
        reader: Reader::Tiff,
    },
];

/// Extensions in [`SUPPORTED_FORMATS`], for the file dialog filter.
#[cfg(feature = "gui")]
pub fn supported_extensions() -> Vec<&'static str> {
    SUPPORTED_FORMATS
        .iter()
        .map(|format| format.extension)
        .collect()
}

/// Distinct labels in [`SUPPORTED_FORMATS`], for the "supported formats" strip.
///
/// The strip lists readers rather than file names, so rows that share a label
/// have to collapse into one entry.
#[cfg(feature = "gui")]
pub fn supported_labels() -> Vec<&'static str> {
    let mut labels: Vec<&'static str> = Vec::new();
    for format in SUPPORTED_FORMATS {
        if !labels.contains(&format.label) {
            labels.push(format.label);
        }
    }
    labels
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
    let format = SUPPORTED_FORMATS
        .iter()
        .find(|format| format.extension == extension);
    match format {
        Some(format) => format.reader.parse(&path),
        None => bail!("unsupported input extension .{extension}"),
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
    use std::path::PathBuf;

    #[test]
    fn extension_matching_is_case_insensitive() {
        assert_eq!(extension_of(&PathBuf::from("a/b.SVS")), "svs");
        assert_eq!(extension_of(&PathBuf::from("a/b")), "");
    }

    #[cfg(feature = "gui")]
    #[test]
    fn format_table_has_one_entry_per_extension() {
        // The GUI derives both its dialog filter and its format strip from this
        // table, and `open_slide` dispatches on it, so a repeated extension
        // would make one of them wrong.
        use std::collections::HashSet;

        let extensions = supported_extensions();
        let unique: HashSet<&str> = extensions.iter().copied().collect();
        assert_eq!(unique.len(), extensions.len());
        assert_eq!(extensions.len(), 13);
    }

    #[cfg(feature = "gui")]
    #[test]
    fn format_labels_are_listed_once_each() {
        // `.tif` and `.tiff` share a label, so the strip must show one chip.
        use std::collections::HashSet;

        let labels = supported_labels();
        let unique: HashSet<&str> = labels.iter().copied().collect();
        assert_eq!(unique.len(), labels.len());
        assert_eq!(labels.len(), 12);
        assert_eq!(
            labels.iter().filter(|label| **label == "TIF/TIFF").count(),
            1
        );
    }
}
