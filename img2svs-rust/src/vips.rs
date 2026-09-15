//! libvips adapter for TIFF and formats whose vendor decoder is provided by
//! the bundled OpenSlide runtime (NDPI and MRXS).

use crate::jpeg::decode_image;
use crate::svs;
use anyhow::{anyhow, bail, Context, Result};
use image::RgbImage;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;

#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt;

#[cfg(target_os = "windows")]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Command-line tools of the bundled libvips runtime.
const VIPS_EXECUTABLE: &str = if cfg!(windows) { "vips.exe" } else { "vips" };
const VIPSHEADER_EXECUTABLE: &str = if cfg!(windows) {
    "vipsheader.exe"
} else {
    "vipsheader"
};

/// Micrometres in one millimetre, the scale between MPP and the
/// pixels-per-millimetre resolution reported by libvips.
const MICROMETRES_PER_MILLIMETRE: f64 = 1_000.0;
/// Lower bound applied to MPP before converting, so a missing resolution cannot
/// turn into a division by zero.
const MIN_MPP: f64 = 0.000_001;

/// Builds a command for a bundled vips tool with `bin` prepended to `PATH`.
fn vips_command(bin: &Path, executable: &Path) -> Result<Command> {
    let path = env::var_os("PATH").unwrap_or_default();
    let joined =
        env::join_paths(std::iter::once(bin.to_path_buf()).chain(env::split_paths(&path)))?;
    let mut command = Command::new(executable);
    hide_console_window(&mut command);
    command.env("PATH", joined);
    Ok(command)
}

pub fn convert(input: &Path, output: &Path, quality: u8, overwrite: bool) -> Result<()> {
    if output.exists() && !overwrite {
        println!(
            "Skip  : {} -> {} (already exists)",
            input.display(),
            output.display()
        );
        return Ok(());
    }
    let bin = locate_vips_bin()
        .context("TIFF/NDPI/MRXS requires the bundled OpenSlide/libvips runtime")?;
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent)?;
    }
    let temporary = temporary_path(output);
    let result = (|| {
        let (mpp, app_mag) = thread::scope(|scope| {
            let mpp = scope.spawn(|| read_mpp(&bin, input));
            let app_mag = scope.spawn(|| read_app_mag(&bin, input));
            (
                mpp.join().ok().flatten().unwrap_or(0.25),
                app_mag.join().ok().flatten().unwrap_or(0.0),
            )
        });
        let (thumbnail, images) = thread::scope(|scope| {
            let thumbnail = scope.spawn(|| load_thumbnail(&bin, input, &temporary));
            let associated =
                scope.spawn(|| load_associated_images(&bin, input, quality, &temporary));
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
                    &format!("--xres={}", vips_resolution(mpp)),
                    &format!("--yres={}", vips_resolution(mpp)),
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
    let bin = locate_vips_bin()
        .context("TIFF/NDPI/MRXS requires the bundled OpenSlide/libvips runtime")?;
    let executable = bin.join(VIPSHEADER_EXECUTABLE);
    let output = vips_command(&bin, &executable)?
        .arg("-a")
        .arg(input)
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
    let executable = bin.join(VIPS_EXECUTABLE);
    let output = vips_command(bin, &executable)?
        .arg(operation)
        .args(positional)
        .args(options)
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
    read_text_field(bin, input, field)?.parse().ok()
}

fn read_text_field(bin: &Path, input: &Path, field: &str) -> Option<String> {
    let executable = bin.join(VIPSHEADER_EXECUTABLE);
    let output = vips_command(bin, &executable)
        .ok()?
        .arg("-f")
        .arg(field)
        .arg(input)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    (!value.is_empty()).then_some(value)
}

fn read_mpp(bin: &Path, input: &Path) -> Option<f64> {
    if let Some(mpp) = read_field(bin, input, "openslide.mpp-x").filter(|value| *value > 0.0) {
        return Some(mpp);
    }
    let pixels_per_millimeter =
        read_field(bin, input, "xres").or_else(|| read_field(bin, input, "yres"))?;
    if !pixels_per_millimeter.is_finite() || pixels_per_millimeter <= 0.0 {
        return None;
    }
    let unit = read_text_field(bin, input, "resolution-unit")?.to_ascii_lowercase();
    if !unit.contains("cm") && !unit.contains("centimeter") && !unit.contains("in") {
        return None;
    }
    Some(MICROMETRES_PER_MILLIMETRE / pixels_per_millimeter)
}

fn vips_resolution(mpp: f64) -> f64 {
    MICROMETRES_PER_MILLIMETRE / mpp.max(MIN_MPP)
}

fn read_app_mag(bin: &Path, input: &Path) -> Option<f64> {
    read_field(bin, input, "openslide.objective-power")
        .filter(|value| *value > 0.0)
        .or_else(|| {
            let description = read_text_field(bin, input, "image-description")?;
            parse_labeled_number(&description, &["objective power", "appmag"])
        })
}

fn parse_labeled_number(text: &str, labels: &[&str]) -> Option<f64> {
    let lowercase = text.to_ascii_lowercase();
    labels.iter().find_map(|label| {
        let index = lowercase.find(label)? + label.len();
        let remainder = lowercase[index..].trim_start_matches(|character: char| {
            character.is_ascii_whitespace() || matches!(character, '=' | ':' | '|')
        });
        let length = remainder
            .char_indices()
            .take_while(|(_, character)| {
                character.is_ascii_digit() || matches!(character, '.' | '+' | '-')
            })
            .map(|(index, character)| index + character.len_utf8())
            .last()?;
        remainder[..length]
            .parse::<f64>()
            .ok()
            .filter(|value| *value > 0.0)
    })
}

fn hide_console_window(command: &mut Command) {
    #[cfg(target_os = "windows")]
    command.creation_flags(CREATE_NO_WINDOW);
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
    candidates
        .into_iter()
        .find(|path| path.join(VIPS_EXECUTABLE).is_file())
}

fn temporary_path(output: &Path) -> PathBuf {
    let file_name = output.file_name().unwrap_or_default().to_string_lossy();
    output.with_file_name(format!(".{file_name}.{}.tmp", std::process::id()))
}

#[cfg(test)]
mod tests {
    use super::{parse_labeled_number, vips_resolution};

    #[test]
    fn parses_tiff_objective_power() {
        assert_eq!(
            parse_labeled_number("Objective Power=20", &["objective power", "appmag"]),
            Some(20.0)
        );
    }

    #[test]
    fn parses_aperio_app_mag() {
        assert_eq!(
            parse_labeled_number(
                "Aperio Image Library|AppMag = 40.000000|MPP = 0.250000",
                &["objective power", "appmag"]
            ),
            Some(40.0)
        );
    }

    #[test]
    fn converts_mpp_to_vips_pixels_per_millimeter() {
        assert_eq!(vips_resolution(0.25), 4_000.0);
    }
}
