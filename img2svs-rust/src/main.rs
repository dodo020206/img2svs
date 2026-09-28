//! A console build (built without the `gui` feature) keeps the console
//! subsystem so command-line output stays visible; the GUI build hides it.
#![cfg_attr(
    all(target_os = "windows", feature = "gui"),
    windows_subsystem = "windows"
)]

mod binary;
mod dmetrix;
#[cfg(feature = "gui")]
mod gui;
mod hevc;
mod indexed;
mod jpeg;
mod loader;
mod model;
mod mrxs;
mod ndpi;
mod report;
mod sdpc;
mod svs;
mod tiff;

use anyhow::{bail, Result};
use clap::Parser;
use std::path::{Path, PathBuf};
use std::time::Instant;

/// Accepted input formats, shared by both build variants. Kept in step with
/// `loader::SUPPORTED_FORMATS`, which is what actually dispatches on them.
/// `.svs` is not in the list: it is the output format.
#[cfg(feature = "gui")]
const INPUT_HELP: &str =
    "Input .csp/.dmetrix/.kfb/.mdss/.mdsx/.msdx/.mrxs/.ndpi/.sdpc/.dyqx/.tif/.tiff file. \
     Omit it to launch the GUI.";
#[cfg(not(feature = "gui"))]
const INPUT_HELP: &str =
    "Input .csp/.dmetrix/.kfb/.mdss/.mdsx/.msdx/.mrxs/.ndpi/.sdpc/.dyqx/.tif/.tiff file.";

/// Names the build variant so the two distributed executables can be told apart.
#[cfg(feature = "gui")]
const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), " (gui)");
#[cfg(not(feature = "gui"))]
const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), " (cli)");

#[derive(Parser, Debug)]
#[command(
    name = "img2svs",
    version = VERSION,
    about = "Convert supported whole-slide files to Aperio SVS"
)]
struct Args {
    #[arg(help = INPUT_HELP)]
    input: Option<PathBuf>,
    /// Output .svs file. Defaults to the input path with an .svs extension.
    #[arg(short, long)]
    output: Option<PathBuf>,
    /// Output JPEG quality (1-100).
    #[arg(long)]
    jpeg_quality: Option<u8>,
    /// Replace an existing output file.
    #[arg(long)]
    overwrite: bool,
    /// Only parse and print slide metadata.
    #[arg(long)]
    info: bool,
    /// Launch the native Rust GUI.
    #[cfg(feature = "gui")]
    #[arg(long)]
    gui: bool,
    /// Launch the GUI and close after its first rendered frame (build smoke test).
    #[cfg(feature = "gui")]
    #[arg(long, hide = true)]
    smoke_test: bool,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("Error: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args = Args::parse();
    #[cfg(feature = "gui")]
    if args.gui || args.smoke_test || args.input.is_none() {
        return gui::run(gui::LaunchOptions {
            smoke_test: args.smoke_test,
        });
    }
    let Some(input_arg) = args.input else {
        bail!("no input file given; pass a slide path and see --help");
    };
    let slide = loader::open_slide(&input_arg)?;
    report::print_slide(&slide);
    if args.info {
        return Ok(());
    }
    // `slide.path` is the canonicalised input, so a default output lands next
    // to the real file rather than next to whatever path was typed.
    let output = args
        .output
        .unwrap_or_else(|| with_extension(&slide.path, "svs"));
    let quality = validate_quality(args.jpeg_quality.unwrap_or(slide.metadata.jpeg_quality))?;
    let started = Instant::now();
    svs::write_slide(
        &slide,
        &output,
        &svs::WriteOptions {
            jpeg_quality: quality,
            overwrite: args.overwrite,
        },
    )?;
    println!("Output: {}", output.display());
    println!("Time  : {:.2} s", started.elapsed().as_secs_f64());
    Ok(())
}

/// Rejects a JPEG quality outside the range the encoder accepts.
fn validate_quality(quality: u8) -> Result<u8> {
    if !(1..=100).contains(&quality) {
        bail!("--jpeg-quality must be between 1 and 100");
    }
    Ok(quality)
}

fn with_extension(path: &Path, extension: &str) -> PathBuf {
    let mut result = path.to_path_buf();
    result.set_extension(extension);
    result
}
