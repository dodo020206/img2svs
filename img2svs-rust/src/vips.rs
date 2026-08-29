//! OpenSlide/libvips adapter for formats whose vendor decoder is already
//! provided by the bundled libvips runtime (NDPI and MRXS).

use crate::jpeg::decode_image;
use crate::svs;
use anyhow::{anyhow, bail, Context, Result};
use image::RgbImage;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;

pub fn convert(
    input: &Path,
    output: &Path,
    quality: u8,
    skip_associated: bool,
    overwrite: bool,
) -> Result<()> {
    if output.exists() && !overwrite {
        println!(
            "Skip  : {} -> {} (already exists)",
            input.display(),
            output.display()
        );
        return Ok(());
    }
    let bin =
        locate_vips_bin().context("NDPI/MRXS requires the bundled OpenSlide/libvips runtime")?;
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent)?;
    }
    let temporary = temporary_path(output);
    let result = (|| {
        let (mpp, app_mag) = thread::scope(|scope| {
            let mpp = scope.spawn(|| read_field(&bin, input, "openslide.mpp-x"));
            let app_mag = scope.spawn(|| read_field(&bin, input, "openslide.objective-power"));
            (
                mpp.join().ok().flatten().unwrap_or(0.25),
                app_mag.join().ok().flatten().unwrap_or(0.0),
            )
        });
        let (thumbnail, images) = thread::scope(|scope| {
            let thumbnail = scope.spawn(|| load_thumbnail(&bin, input, &temporary));
            let associated = scope.spawn(|| {
                if skip_associated {
                    Vec::new()
                } else {
                    load_associated_images(&bin, input, quality, &temporary)
                }
            });
            let pyramid = run_vips(
                &bin,
                "tiffsave",
                &[input, &temporary],
                &[
                    "--pyramid=true",
                    "--tile=true",
                    "--tile-width=256",
                    "--tile-height=256",
                    "--compression=jpeg",
                    &format!("--Q={quality}"),
                    &format!("--xres={}", 10000.0 / mpp.max(0.000001)),
                    &format!("--yres={}", 10000.0 / mpp.max(0.000001)),
                    "--resunit=cm",
                ],
            );
            let thumbnail = thumbnail
                .join()
                .map_err(|_| anyhow!("thumbnail worker panicked"))??;
            let associated = associated.join().unwrap_or_default();
            pyramid?;
            Ok::<_, anyhow::Error>((thumbnail, associated))
        })?;
        if !images.is_empty() {
            svs::append_associated_images(&temporary, &images, mpp, quality)?;
        }
        svs::prepend_compatible_pages(&temporary, &thumbnail, mpp, app_mag, quality)?;
        if output.exists() {
            fs::remove_file(output)?;
        }
        fs::rename(&temporary, output)
            .with_context(|| format!("replace output {}", output.display()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn load_thumbnail(bin: &Path, input: &Path, temporary: &Path) -> Result<RgbImage> {
    let stem = temporary
        .file_name()
        .context("temporary output has no name")?;
    let jpeg_path = temporary.with_file_name(format!(".{}.thumbnail.jpg", stem.to_string_lossy()));
    let result = (|| {
        run_vips(bin, "thumbnail", &[input, &jpeg_path], &["1024"])?;
        let image = decode_image(&fs::read(&jpeg_path)?)?;
        Ok(image)
    })();
    let _ = fs::remove_file(&jpeg_path);
    result
}

fn load_associated_images(
    bin: &Path,
    input: &Path,
    quality: u8,
    temporary: &Path,
) -> Vec<(String, RgbImage)> {
    thread::scope(|scope| {
        ["label", "macro"]
            .into_iter()
            .map(|kind| {
                scope.spawn(move || {
                    let stem = temporary.file_name()?.to_string_lossy();
                    let jpeg_path = temporary.with_file_name(format!(".{stem}.{kind}.jpg"));
                    let mut output = jpeg_path.as_os_str().to_os_string();
                    output.push(format!("[Q={quality}]"));
                    let output = PathBuf::from(output);
                    let result = (|| {
                        run_vips(
                            bin,
                            "openslideload",
                            &[input, &output],
                            &[&format!("--associated={kind}")],
                        )?;
                        decode_image(&fs::read(&jpeg_path)?)
                    })();
                    let _ = fs::remove_file(&jpeg_path);
                    result.ok().map(|image| (kind.to_owned(), image))
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
            .filter_map(|worker| worker.join().ok().flatten())
            .collect()
    })
}

pub fn print_info(input: &Path) -> Result<()> {
    let bin =
        locate_vips_bin().context("NDPI/MRXS requires the bundled OpenSlide/libvips runtime")?;
    let executable = bin.join(if cfg!(windows) {
        "vipsheader.exe"
    } else {
        "vipsheader"
    });
    let path = env::var_os("PATH").unwrap_or_default();
    let joined = env::join_paths(std::iter::once(bin).chain(env::split_paths(&path)))?;
    let output = Command::new(&executable)
        .arg("-a")
        .arg(input)
        .env("PATH", joined)
        .output()
        .with_context(|| format!("run {}", executable.display()))?;
    if !output.status.success() {
        bail!(
            "libvips failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    print!("{}", String::from_utf8_lossy(&output.stdout));
    Ok(())
}

fn run_vips(bin: &Path, operation: &str, positional: &[&Path], options: &[&str]) -> Result<()> {
    let executable = bin.join(if cfg!(windows) { "vips.exe" } else { "vips" });
    let mut command = Command::new(&executable);
    command.arg(operation);
    for argument in positional {
        command.arg(argument);
    }
    for option in options {
        command.arg(option);
    }
    let path = env::var_os("PATH").unwrap_or_default();
    let joined =
        env::join_paths(std::iter::once(bin.to_path_buf()).chain(env::split_paths(&path)))?;
    let output = command
        .env("PATH", joined)
        .output()
        .with_context(|| format!("run {}", executable.display()))?;
    if !output.status.success() {
        let message = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        bail!(
            "libvips failed ({}): {}",
            output.status,
            if message.is_empty() {
                "unknown error"
            } else {
                &message
            }
        );
    }
    Ok(())
}

fn read_field(bin: &Path, input: &Path, field: &str) -> Option<f64> {
    let executable = bin.join(if cfg!(windows) {
        "vipsheader.exe"
    } else {
        "vipsheader"
    });
    let path = env::var_os("PATH").unwrap_or_default();
    let joined =
        env::join_paths(std::iter::once(bin.to_path_buf()).chain(env::split_paths(&path))).ok()?;
    let output = Command::new(executable)
        .arg("-f")
        .arg(field)
        .arg(input)
        .env("PATH", joined)
        .output()
        .ok()?;
    String::from_utf8_lossy(&output.stdout).trim().parse().ok()
}

fn locate_vips_bin() -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(root) = env::var_os("VIPS_HOME") {
        let root = PathBuf::from(root);
        candidates.push(root.join("bin"));
        candidates.push(root);
    }
    if let Ok(executable) = env::current_exe() {
        if let Some(parent) = executable.parent() {
            candidates.push(parent.join("vips").join("bin"));
            candidates.push(parent.join("..").join("vips").join("bin"));
        }
    }
    for path in env::split_paths(&env::var_os("PATH").unwrap_or_default()) {
        candidates.push(path);
    }
    candidates.into_iter().find(|path| {
        path.join(if cfg!(windows) { "vips.exe" } else { "vips" })
            .is_file()
    })
}

fn temporary_path(output: &Path) -> PathBuf {
    let file_name = output.file_name().unwrap_or_default().to_string_lossy();
    output.with_file_name(format!(".{file_name}.{}.tmp", std::process::id()))
}
