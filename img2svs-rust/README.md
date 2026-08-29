# img2svs Rust

This is the native Rust implementation of the converter in `../img2svs-python`.

## GUI

Run the executable without arguments, or pass `--gui`:

```powershell
.\target\release\img2svs-rust.exe
.\target\release\img2svs-rust.exe --gui
```

The GUI supports multi-file selection, recursive folder scanning, Windows drag
and drop, output-folder selection, JPEG quality, overwrite/associated-image
options, a queued background worker, progress, per-file status, logs and
cooperative cancellation. Each row has its own `移除` button. Shortcuts are
`Ctrl+O` (files), `Ctrl+Shift+O` (folder), `F5` (start) and `Esc` (stop).

The native backends are:

- `.dmetrix`: JPEG tiles, pyramid levels, label and macro images.
- `.csp`: indexed JPEG tiles, pyramid levels and label/macro images.
- `.kfb`: KFBio indexed JPEG tiles, sparse tile placement, pyramid levels and
  label/macro images.
- `.mdsx` / `.msdx`: BKIO container, UTF-16/Base64 XML, INI metadata, JPEG
  tiles and label/macro images.
- `.sdpc` / `.dyqx`: JPEG- and HEVC-compressed SDPC files, including non-16-aligned source
  tiles such as `616x880`; adjacent source tiles are composed into valid TIFF
  output tiles and the final row/column is white-padded. HEVC uses the bundled
  FFmpeg native runtime when available.
- `.ndpi` / `.mrxs`: OpenSlide/libvips-backed streaming loading and pyramidal
  JPEG SVS output (classic TIFF when possible, BigTIFF only when required),
  including the Aperio description and thumbnail pages; the Rust adapter does
  not materialize the whole slide as an uncompressed intermediate image.

The CLI and output layout are intentionally close to the working Python version:

```text
cargo run --release -- test_data/dmetrix/1.dmetrix -o test_output-rust/1.svs --overwrite
cargo run --release -- test_data/2605551-jpeg.sdpc -o test_output-rust/2605551.svs --overwrite
```

JPEG/HEVC SDPC/DYQX and all formats listed above are supported by the Rust
GUI/CLI. HEVC requires the FFmpeg native runtime: set `FFMPEG_HOME`, or place
the bundled `av.libs` directory next to the executable. NDPI/MRXS require the
OpenSlide/libvips runtime: set `VIPS_HOME`, or place the runtime at
`vips\bin` next to the executable.
Runtime discovery is relative to the executable or environment variables and
does not depend on a development-machine path. NDPI/MRXS conversion keeps
libvips' hardware-aware concurrency default; `VIPS_CONCURRENCY` can be used as
an advanced override after benchmarking the target machine.

## Performance

Native JPEG and HEVC tile decode/encode work runs in a bounded worker pool.
JPEG conversion defaults to the logical CPU count reported by the operating
system (up to 64 workers). HEVC keeps one logical CPU available and is capped
at 32 workers because each worker owns a decoder. Set `IMG2SVS_THREADS` before
launching the CLI or GUI to override the detected value, for example:

```powershell
$env:IMG2SVS_THREADS = '8'
.\target\release\img2svs-rust.exe input.csp -o output.svs --overwrite
```

JPEG tiles use the Rust encoder's SIMD path. Non-4:2:0 JPEG tiles are decoded
directly to YCbCr before 4:2:0 encoding, avoiding an unnecessary RGB color
conversion. Source tiles are read through a shared read-only memory map;
encoded tiles are buffered only in bounded batches and written in source
order, so parallel conversion does not load the complete slide into memory or
change the TIFF tile-offset order.

## Build and smoke test

On a normal Rust Windows installation:

```powershell
 cargo fmt --all -- --check
cargo build --release
 .\target\release\img2svs-rust.exe --smoke-test
```

For the repository's Windows development runtime, `build_windows.ps1` also
copies `../img2svs-python/vips` and the FFmpeg DLLs beside the executable when
those directories are present. Distribute the complete release directory,
including `vips` and `av.libs`, rather than the executable alone.

`--smoke-test` initializes the native window and closes after the first frame;
it is useful for CI or packaging checks without leaving a GUI process running.
