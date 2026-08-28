mod binary;
mod dmetrix;
mod gui;
mod hevc;
mod indexed;
mod jpeg;
mod model;
mod sdpc;
mod svs;
mod vips;

use anyhow::{bail, Context, Result};
use clap::Parser;
use std::path::{Path, PathBuf};
use std::time::Instant;

#[derive(Parser, Debug)]
#[command(
    name = "img2svs",
    version,
    about = "Convert supported whole-slide files to Aperio SVS"
)]
struct Args {
    /// Input .csp/.dmetrix/.kfb/.mdsx/.msdx/.mrxs/.ndpi/.sdpc/.dyqx file.
    /// Omit it to launch the GUI.
    input: Option<PathBuf>,
    /// Output .svs file. Defaults to the input path with an .svs extension.
    #[arg(short, long)]
    output: Option<PathBuf>,
    /// Output JPEG quality (1-100).
    #[arg(long)]
    jpeg_quality: Option<u8>,
    /// Do not write label/macro associated images.
    #[arg(long)]
    skip_associated: bool,
    /// Replace an existing output file.
    #[arg(long)]
    overwrite: bool,
    /// Only parse and print slide metadata.
    #[arg(long)]
    info: bool,
    /// Launch the native Rust GUI.
    #[arg(long)]
    gui: bool,
    /// Launch the GUI and close after its first rendered frame (build smoke test).
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
    if args.gui || args.smoke_test || args.input.is_none() {
        return gui::run(gui::LaunchOptions {
            smoke_test: args.smoke_test,
        });
    }
    let input_arg = args.input.expect("input checked above");
    let input = input_arg
        .canonicalize()
        .with_context(|| format!("input not found: {}", input_arg.display()))?;
    let backend = input
        .extension()
        .and_then(|v| v.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let slide = match backend.as_str() {
        "dmetrix" => dmetrix::parse(&input)?,
        "sdpc" | "dyqx" => sdpc::parse(&input)?,
        "csp" | "kfb" | "mdsx" | "msdx" => indexed::parse(&input)?,
        "ndpi" | "mrxs" => {
            let output = args
                .output
                .clone()
                .unwrap_or_else(|| with_extension(&input, "svs"));
            let quality = args.jpeg_quality.unwrap_or(75);
            if !(1..=100).contains(&quality) {
                bail!("--jpeg-quality must be between 1 and 100");
            }
            if args.info {
                return vips::print_info(&input);
            }
            vips::convert(
                &input,
                &output,
                quality,
                args.skip_associated,
                args.overwrite,
            )?;
            println!("Output: {}", output.display());
            return Ok(());
        }
        other => bail!("unsupported input extension .{other}"),
    };
    indexed::print_info(&slide);
    if args.info {
        return Ok(());
    }
    let output = args.output.unwrap_or_else(|| with_extension(&input, "svs"));
    let quality = args.jpeg_quality.unwrap_or(slide.metadata.jpeg_quality);
    if !(1..=100).contains(&quality) {
        bail!("--jpeg-quality must be between 1 and 100");
    }
    let started = Instant::now();
    svs::write_slide(
        &slide,
        &output,
        &svs::WriteOptions {
            jpeg_quality: quality,
            skip_associated: args.skip_associated,
            overwrite: args.overwrite,
        },
    )?;
    println!("Output: {}", output.display());
    println!("Time  : {:.2} s", started.elapsed().as_secs_f64());
    Ok(())
}

fn with_extension(path: &Path, extension: &str) -> PathBuf {
    let mut result = path.to_path_buf();
    result.set_extension(extension);
    result
}
