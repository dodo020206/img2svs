#!/usr/bin/env python3
from __future__ import annotations

import argparse
import csv
import json
import math
import os
import subprocess
import sys
import time
from dataclasses import asdict, dataclass
from datetime import datetime
from pathlib import Path
from typing import Iterable, Sequence

import numpy
import tifffile
from PIL import Image


PROJECT_DIR = Path(__file__).resolve().parent
DEFAULT_OUTPUT_ROOT = PROJECT_DIR / "tmp" / "openslide_vs_current_bench"
DEFAULT_TILE_SIZE = 256
DEFAULT_QUALITY = 90


@dataclass(frozen=True)
class BenchmarkCase:
    name: str
    input_path: Path
    current_script: Path | None
    current_supported: bool


@dataclass(frozen=True)
class BenchmarkResult:
    case: str
    method: str
    input_path: str
    output_path: str
    supported: bool
    success: bool
    elapsed_seconds: float | None
    output_bytes: int | None
    message: str


def configure_vips_runtime() -> dict[str, str]:
    """Return an environment that can find the bundled libvips/OpenSlide DLLs."""

    env = os.environ.copy()
    search_dirs = [
        PROJECT_DIR / "vips" / "bin",
        PROJECT_DIR / "vips",
    ]
    path_entries: list[str] = []
    for directory in search_dirs:
        if not directory.exists():
            continue
        text = str(directory.resolve())
        path_entries.append(text)
        add_dll_directory = getattr(os, "add_dll_directory", None)
        if add_dll_directory is not None:
            try:
                add_dll_directory(text)
            except OSError:
                pass
    current_path = env.get("PATH", "")
    env["PATH"] = os.pathsep.join(path_entries + ([current_path] if current_path else []))
    env["VIPS_HOME"] = str((PROJECT_DIR / "vips").resolve())
    return env


def load_openslide():
    configure_vips_runtime()
    import openslide

    return openslide


def find_first(pattern: str) -> Path | None:
    matches = sorted(PROJECT_DIR.glob(pattern))
    return matches[0] if matches else None


def default_cases() -> list[BenchmarkCase]:
    return [
        BenchmarkCase(
            name="NDPI",
            input_path=PROJECT_DIR / "test_data" / "tt1.ndpi",
            current_script=PROJECT_DIR / "ndpi_to_svs.py",
            current_supported=True,
        ),
        BenchmarkCase(
            name="MRXS",
            input_path=find_first("test_data/*mrxs/*.mrxs") or PROJECT_DIR / "missing.mrxs",
            current_script=PROJECT_DIR / "mrxs_to_svs.py",
            current_supported=True,
        ),
        BenchmarkCase(
            name="CSP",
            input_path=PROJECT_DIR / "test_data" / "case3.csp",
            current_script=PROJECT_DIR / "csp_to_svs.py",
            current_supported=True,
        ),
        BenchmarkCase(
            name="SDPC",
            input_path=PROJECT_DIR / "test_data" / "20220514_145829_0.sdpc",
            current_script=PROJECT_DIR / "sdpc_to_svs.py",
            current_supported=True,
        ),
        BenchmarkCase(
            name="KFB",
            input_path=find_first("test_data/*.kfb") or PROJECT_DIR / "missing.kfb",
            current_script=PROJECT_DIR / "kfb_to_svs.py",
            current_supported=True,
        ),
        BenchmarkCase(
            name="MDSX",
            input_path=PROJECT_DIR / "test_data" / "2600394-IHC-10" / "1.mdsx",
            current_script=PROJECT_DIR / "mdsx_to_svs.py",
            current_supported=True,
        ),
    ]


def select_cases(names: Sequence[str]) -> list[BenchmarkCase]:
    cases = default_cases()
    if not names:
        return cases
    wanted = {name.upper() for name in names}
    return [case for case in cases if case.name.upper() in wanted]


def output_size(path: Path) -> int | None:
    try:
        return path.stat().st_size
    except OSError:
        return None


def should_use_bigtiff(slide, input_path: Path) -> bool:
    width, height = slide.dimensions
    raw_bytes = width * height * 3
    if raw_bytes >= 3_500_000_000:
        return True
    total = 0
    try:
        total += input_path.stat().st_size
    except OSError:
        pass
    data_dir = input_path.with_suffix("")
    if data_dir.is_dir():
        for entry in data_dir.rglob("*"):
            if entry.is_file():
                try:
                    total += entry.stat().st_size
                except OSError:
                    continue
    return total >= 3_500_000_000


def slide_description(slide, quality: int, tile_size: int) -> str:
    width, height = slide.dimensions
    mpp = slide.properties.get("openslide.mpp-x") or "0"
    app_mag = slide.properties.get("openslide.objective-power") or "0"
    return (
        "Aperio Image Library v12.4.3\n"
        f"{width}x{height} [0,0 {width}x{height}] "
        f"({tile_size}x{tile_size}) JPEG/RGB Q={quality}"
        f"|AppMag = {app_mag}|MPP = {mpp}"
    )


def rgba_to_rgb_array(image: Image.Image) -> numpy.ndarray:
    rgba = image.convert("RGBA")
    background = Image.new("RGBA", rgba.size, (255, 255, 255, 255))
    background.alpha_composite(rgba)
    return numpy.asarray(background.convert("RGB"))


def openslide_tile_iterator(slide, level_index: int, tile_size: int) -> Iterable[numpy.ndarray]:
    width, height = slide.level_dimensions[level_index]
    cols = math.ceil(width / tile_size)
    rows = math.ceil(height / tile_size)
    downsample = slide.level_downsamples[level_index]
    for row in range(rows):
        for col in range(cols):
            location = (
                int(round(col * tile_size * downsample)),
                int(round(row * tile_size * downsample)),
            )
            image = slide.read_region(location, level_index, (tile_size, tile_size))
            yield rgba_to_rgb_array(image)


def write_openslide_baseline(
    input_path: Path,
    output_path: Path,
    *,
    tile_size: int,
    quality: int,
) -> None:
    openslide = load_openslide()
    slide = openslide.OpenSlide(str(input_path))
    compressionargs = {"level": quality}
    output_path.parent.mkdir(parents=True, exist_ok=True)
    with tifffile.TiffWriter(output_path, bigtiff=should_use_bigtiff(slide, input_path)) as tif:
        for level_index, (width, height) in enumerate(slide.level_dimensions):
            kwargs = dict(
                data=openslide_tile_iterator(slide, level_index, tile_size),
                shape=(height, width, 3),
                dtype=numpy.uint8,
                photometric="rgb",
                tile=(tile_size, tile_size),
                compression="jpeg",
                compressionargs=compressionargs,
                resolutionunit="CENTIMETER",
                metadata=None,
            )
            if level_index == 0:
                tif.write(
                    **kwargs,
                    description=slide_description(slide, quality, tile_size),
                    software="benchmark_openslide_vs_scripts.py",
                )
                thumbnail = rgba_to_rgb_array(slide.get_thumbnail((1024, 1024)))
                tif.write(
                    data=thumbnail,
                    photometric="rgb",
                    compression="jpeg",
                    compressionargs=compressionargs,
                    resolutionunit="CENTIMETER",
                    metadata=None,
                    software=False,
                )
            else:
                tif.write(
                    **kwargs,
                    subfiletype=1,
                    software=False,
                )


def run_openslide_case(
    case: BenchmarkCase,
    output_dir: Path,
    *,
    tile_size: int,
    quality: int,
) -> BenchmarkResult:
    openslide = load_openslide()
    if not case.input_path.exists():
        return BenchmarkResult(
            case=case.name,
            method="openslide-baseline",
            input_path=str(case.input_path),
            output_path="",
            supported=False,
            success=False,
            elapsed_seconds=None,
            output_bytes=None,
            message="input missing",
        )

    detected = openslide.OpenSlide.detect_format(str(case.input_path))
    if detected is None:
        return BenchmarkResult(
            case=case.name,
            method="openslide-baseline",
            input_path=str(case.input_path),
            output_path="",
            supported=False,
            success=False,
            elapsed_seconds=None,
            output_bytes=None,
            message="OpenSlide detect_format returned None",
        )

    output_path = output_dir / f"{case.name.lower()}_openslide.svs"
    start = time.perf_counter()
    try:
        write_openslide_baseline(
            case.input_path,
            output_path,
            tile_size=tile_size,
            quality=quality,
        )
    except Exception as exc:
        elapsed = time.perf_counter() - start
        return BenchmarkResult(
            case=case.name,
            method="openslide-baseline",
            input_path=str(case.input_path),
            output_path=str(output_path),
            supported=True,
            success=False,
            elapsed_seconds=elapsed,
            output_bytes=output_size(output_path),
            message=f"{type(exc).__name__}: {exc}",
        )

    elapsed = time.perf_counter() - start
    return BenchmarkResult(
        case=case.name,
        method="openslide-baseline",
        input_path=str(case.input_path),
        output_path=str(output_path),
        supported=True,
        success=True,
        elapsed_seconds=elapsed,
        output_bytes=output_size(output_path),
        message=f"detected={detected}",
    )


def run_current_case(
    case: BenchmarkCase,
    output_dir: Path,
    *,
    quality: int,
) -> BenchmarkResult:
    if not case.current_supported or case.current_script is None:
        return BenchmarkResult(
            case=case.name,
            method="current-script",
            input_path=str(case.input_path),
            output_path="",
            supported=False,
            success=False,
            elapsed_seconds=None,
            output_bytes=None,
            message="current script unavailable",
        )
    if not case.input_path.exists():
        return BenchmarkResult(
            case=case.name,
            method="current-script",
            input_path=str(case.input_path),
            output_path="",
            supported=False,
            success=False,
            elapsed_seconds=None,
            output_bytes=None,
            message="input missing",
        )

    output_path = output_dir / f"{case.name.lower()}_current.svs"
    command = [
        sys.executable,
        str(case.current_script),
        str(case.input_path),
        "-o",
        str(output_path),
        "--overwrite",
        "--skip-associated",
        "--jpeg-quality",
        str(quality),
    ]
    start = time.perf_counter()
    completed = subprocess.run(
        command,
        cwd=PROJECT_DIR,
        env=configure_vips_runtime(),
        text=True,
        capture_output=True,
        check=False,
    )
    elapsed = time.perf_counter() - start
    message = (completed.stdout + "\n" + completed.stderr).strip()
    if len(message) > 1200:
        message = message[-1200:]
    return BenchmarkResult(
        case=case.name,
        method="current-script",
        input_path=str(case.input_path),
        output_path=str(output_path),
        supported=True,
        success=completed.returncode == 0,
        elapsed_seconds=elapsed,
        output_bytes=output_size(output_path),
        message=message or f"returncode={completed.returncode}",
    )


def write_results(results: Sequence[BenchmarkResult], output_dir: Path) -> None:
    json_path = output_dir / "results.json"
    csv_path = output_dir / "results.csv"
    with json_path.open("w", encoding="utf-8") as fh:
        json.dump([asdict(result) for result in results], fh, ensure_ascii=False, indent=2)
    with csv_path.open("w", encoding="utf-8", newline="") as fh:
        writer = csv.DictWriter(fh, fieldnames=list(asdict(results[0]).keys()) if results else [])
        if results:
            writer.writeheader()
            for result in results:
                writer.writerow(asdict(result))


def print_summary(results: Sequence[BenchmarkResult], output_dir: Path) -> None:
    print(f"OUTPUT_DIR={output_dir}")
    for result in results:
        elapsed = (
            f"{result.elapsed_seconds:.2f}s"
            if result.elapsed_seconds is not None
            else "-"
        )
        size = (
            f"{result.output_bytes / (1024 ** 3):.2f}GiB"
            if result.output_bytes is not None
            else "-"
        )
        status = "ok" if result.success else ("unsupported" if not result.supported else "failed")
        print(
            f"{result.case:5s} {result.method:18s} {status:11s} "
            f"time={elapsed:>10s} size={size:>8s} {result.message.splitlines()[-1][:140]}"
        )

    by_case: dict[str, dict[str, BenchmarkResult]] = {}
    for result in results:
        by_case.setdefault(result.case, {})[result.method] = result
    print("SPEEDUP_TABLE")
    for case, methods in by_case.items():
        current = methods.get("current-script")
        baseline = methods.get("openslide-baseline")
        if (
            current is not None
            and baseline is not None
            and current.success
            and baseline.success
            and current.elapsed_seconds
            and baseline.elapsed_seconds
        ):
            speedup = baseline.elapsed_seconds / current.elapsed_seconds
            print(
                f"{case}: OpenSlide {baseline.elapsed_seconds:.2f}s, "
                f"current {current.elapsed_seconds:.2f}s, speedup {speedup:.2f}x"
            )


def parse_args(argv: Sequence[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Benchmark current converters against an OpenSlide read_region baseline."
    )
    parser.add_argument(
        "--cases",
        nargs="*",
        default=["NDPI", "MRXS", "CSP", "SDPC", "KFB", "MDSX"],
        help="Case names to run. Default: all built-in cases.",
    )
    parser.add_argument(
        "--methods",
        nargs="*",
        choices=("current", "openslide"),
        default=["current", "openslide"],
        help="Methods to run.",
    )
    parser.add_argument("--output-root", type=Path, default=DEFAULT_OUTPUT_ROOT)
    parser.add_argument("--tile-size", type=int, default=DEFAULT_TILE_SIZE)
    parser.add_argument("--jpeg-quality", type=int, default=DEFAULT_QUALITY)
    return parser.parse_args(argv)


def main(argv: Sequence[str] | None = None) -> int:
    args = parse_args(argv)
    stamp = datetime.now().strftime("%Y%m%d_%H%M%S")
    output_dir = args.output_root.expanduser().resolve() / stamp
    output_dir.mkdir(parents=True, exist_ok=True)

    cases = select_cases(args.cases)
    results: list[BenchmarkResult] = []
    for case in cases:
        if "current" in args.methods:
            results.append(
                run_current_case(
                    case,
                    output_dir,
                    quality=args.jpeg_quality,
                )
            )
            write_results(results, output_dir)
            print_summary(results[-1:], output_dir)
        if "openslide" in args.methods:
            results.append(
                run_openslide_case(
                    case,
                    output_dir,
                    tile_size=args.tile_size,
                    quality=args.jpeg_quality,
                )
            )
            write_results(results, output_dir)
            print_summary(results[-1:], output_dir)

    write_results(results, output_dir)
    print_summary(results, output_dir)
    return 0 if all(result.success or not result.supported for result in results) else 1


if __name__ == "__main__":
    raise SystemExit(main())
