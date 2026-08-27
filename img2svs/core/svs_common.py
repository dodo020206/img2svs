from __future__ import annotations

import argparse
import io
import time
from dataclasses import dataclass
from functools import lru_cache
from pathlib import Path
from typing import Callable, Sequence

import numpy
from PIL import Image

APERIO_VERSION = "Aperio Image Library v12.4.3"


@dataclass(frozen=True)
class BatchOptions:
    """保存批量转换场景下共用的命令行参数。"""

    input_path: Path = Path(".")
    output_path: Path | None = None
    output_dir: Path | None = None
    skip_associated: bool = False
    overwrite: bool = False
    jpeg_quality: int | None = None


@dataclass(frozen=True)
class ConversionJob:
    """描述一个待执行的转换任务。"""

    input_path: Path
    output_path: Path
    runner: Callable[[], None]


def _pillow_jpeg_encode(data, /, level: int | None = None, **_kwargs) -> bytes:
    """在 imagecodecs 的 JPEG 编码接口缺失时，用 Pillow 兜底。"""

    quality = normalize_jpeg_quality(level, default=75)
    image = Image.fromarray(numpy.asarray(data))
    buffer = io.BytesIO()
    image.save(buffer, format="JPEG", quality=quality)
    return buffer.getvalue()


def _pillow_jpeg_decode(data, /, **_kwargs) -> numpy.ndarray:
    """在 imagecodecs 的 JPEG 解码接口缺失时，用 Pillow 兜底。"""

    return numpy.asarray(Image.open(io.BytesIO(data)).convert("RGB"))


def ensure_imagecodecs_compat() -> None:
    """为 PyInstaller 场景下可能缺失的 imagecodecs JPEG 符号补兼容别名。"""

    try:
        import imagecodecs
    except Exception:
        return

    if not hasattr(imagecodecs, "jpeg8_encode"):
        imagecodecs.jpeg8_encode = getattr(imagecodecs, "jpeg_encode", _pillow_jpeg_encode)
    if not hasattr(imagecodecs, "jpeg8_decode"):
        imagecodecs.jpeg8_decode = getattr(imagecodecs, "jpeg_decode", _pillow_jpeg_decode)


def add_batch_arguments(parser: argparse.ArgumentParser, input_help: str) -> None:
    """为命令行解析器添加所有格式通用的批处理参数。"""

    parser.add_argument(
        "input",
        nargs="?",
        type=Path,
        default=Path("."),
        help=f"{input_help} Default: current directory",
    )
    parser.add_argument(
        "-o",
        "--output",
        type=Path,
        help="Output .svs path for single-file conversion. Default: beside the input slide",
    )
    parser.add_argument(
        "--output-dir",
        type=Path,
        help=(
            "Root output directory for batch conversion. "
            "Default: write beside each input slide"
        ),
    )
    parser.add_argument(
        "--skip-associated",
        action="store_true",
        help="Do not write label/macro associated images to the output SVS",
    )
    parser.add_argument(
        "--overwrite",
        action="store_true",
        help="Overwrite existing .svs files",
    )


def add_jpeg_quality_argument(
    parser: argparse.ArgumentParser,
    *,
    default: int | None = None,
    help_text: str | None = None,
) -> None:
    """为命令行解析器添加统一的 JPEG 质量参数。"""

    if help_text is None:
        if default is None:
            help_text = "JPEG quality for the output SVS. Default: keep source/default quality"
        else:
            help_text = f"JPEG quality for the output SVS. Default: {default}"
    parser.add_argument(
        "--jpeg-quality",
        type=int,
        default=default,
        help=help_text,
    )


def batch_options_from_args(args: argparse.Namespace) -> BatchOptions:
    """把 argparse 结果归一化为统一的批处理配置对象。"""

    return BatchOptions(
        input_path=(args.input or Path(".")).expanduser().resolve(),
        output_path=args.output.expanduser().resolve() if args.output else None,
        output_dir=args.output_dir.expanduser().resolve() if args.output_dir else None,
        skip_associated=args.skip_associated,
        overwrite=args.overwrite,
        jpeg_quality=getattr(args, "jpeg_quality", None),
    )


def collect_inputs(path: Path, supported_suffixes: set[str], suffix_label: str) -> list[Path]:
    """收集单文件或目录下所有符合后缀要求的输入文件。"""

    if path.is_file():
        if path.suffix.lower() not in supported_suffixes:
            raise ValueError(f"Input file is not a supported {suffix_label}: {path}")
        return [path]

    if path.is_dir():
        slides = sorted(
            p for p in path.rglob("*") if p.is_file() and p.suffix.lower() in supported_suffixes
        )
        if not slides:
            raise FileNotFoundError(f"No {suffix_label} files found under: {path}")
        return slides

    raise FileNotFoundError(f"Input path not found: {path}")


def resolve_output_path(
    input_path: Path,
    input_root: Path,
    output: Path | None,
    output_dir: Path | None,
) -> Path:
    """根据输入路径和输出策略推导最终的 SVS 输出路径。"""

    if output is not None:
        return output.expanduser().resolve()

    if input_path == input_root:
        if output_dir is not None:
            return (output_dir / input_path.with_suffix(".svs").name).resolve()
        return input_path.with_suffix(".svs")

    relative = input_path.relative_to(input_root).with_suffix(".svs")
    if output_dir is not None:
        return (output_dir / relative).resolve()
    return input_path.with_suffix(".svs")


def build_single_format_jobs(
    options: BatchOptions,
    *,
    supported_suffixes: set[str],
    suffix_label: str,
    output_error_message: str,
    runner_factory: Callable[[Path, Path], Callable[[], None]],
) -> list[ConversionJob]:
    """为单一输入格式构建一组可执行的转换任务。"""

    slides = collect_inputs(options.input_path, supported_suffixes, suffix_label)
    if len(slides) > 1 and options.output_path is not None:
        raise ValueError(output_error_message)

    input_root = options.input_path if options.input_path.is_dir() else slides[0]
    jobs: list[ConversionJob] = []
    for slide in slides:
        # 这里先统一计算输出路径，再把具体转换动作封装成延迟执行的 runner。
        output_path = resolve_output_path(
            slide,
            input_root,
            options.output_path,
            options.output_dir,
        )
        jobs.append(
            ConversionJob(
                input_path=slide,
                output_path=output_path,
                runner=runner_factory(slide, output_path),
            )
        )
    return jobs


def format_elapsed(seconds: float) -> str:
    """把秒数格式化为统一的人类可读字符串。"""

    return f"{seconds:.2f} s"


def normalize_jpeg_quality(
    value: int | None,
    *,
    default: int,
    field_name: str = "JPEG quality",
) -> int:
    """把 JPEG 质量参数规范化为 1-100 之间的整数。"""

    quality = default if value is None else value
    if not 1 <= quality <= 100:
        raise ValueError(f"{field_name} must be between 1 and 100")
    return quality


def jpeg_compressionargs(quality: int) -> dict[str, int]:
    """生成 tifffile/imagecodecs 使用的 JPEG 压缩参数。"""

    return {"level": normalize_jpeg_quality(quality, default=quality)}


ensure_imagecodecs_compat()


def run_conversion_jobs(jobs: Sequence[ConversionJob]) -> None:
    """顺序执行转换任务，并统一打印耗时和失败汇总。"""

    failures: list[tuple[Path, str]] = []
    for index, job in enumerate(jobs, start=1):
        print(f"[{index}/{len(jobs)}]")
        started_at = time.perf_counter()
        try:
            job.runner()
        except Exception as exc:
            elapsed = time.perf_counter() - started_at
            failures.append((job.input_path, str(exc)))
            print(f"Failed: {job.input_path} ({exc})")
            print(f"Time  : {format_elapsed(elapsed)}")
        else:
            elapsed = time.perf_counter() - started_at
            print(f"Time  : {format_elapsed(elapsed)}")

    print(
        f"Finished: total={len(jobs)}, success={len(jobs) - len(failures)}, "
        f"failed={len(failures)}"
    )
    if failures:
        raise SystemExit(
            "\n".join(["Batch conversion completed with errors:"] + [
                f"{path}: {message}" for path, message in failures
            ])
        )


def format_level_summary(levels: Sequence[object]) -> str:
    """把金字塔层级对象列表格式化为一行摘要。"""

    return ", ".join(
        (
            f"L{getattr(level, 'index')}="
            f"{getattr(level, 'width')}x{getattr(level, 'height')} "
            f"({getattr(level, 'tile_cols')}x{getattr(level, 'tile_rows')} tiles)"
        )
        for level in levels
    )


def format_associated_summary(entries: Sequence[object]) -> str:
    """把关联图列表格式化为逗号分隔的名称字符串。"""

    if not entries:
        return "none"
    return ", ".join(str(getattr(entry, "kind")) for entry in entries)


def aperio_associated_description(kind: str) -> str:
    """生成 Aperio 关联图页需要的 description 字段。"""

    return f"{kind}\r"


def pixels_per_centimeter(mpp: float) -> float:
    """把每像素微米数转换为 TIFF 需要的每厘米像素数。"""

    return 10_000.0 / mpp


def should_use_bigtiff(input_path: Path) -> bool:
    """根据输入文件大小判断输出是否可能需要 BigTIFF。"""

    # 大多数病理查看器对传统 Aperio TIFF 的兼容性最好。超过这个阈值时，
    # 输出才有较大概率接近传统 TIFF 的 4 GB 限制，再切换到 BigTIFF。
    return input_path.stat().st_size >= 3_500_000_000


def decode_rgb_image(data: bytes) -> numpy.ndarray:
    """把编码后的图像字节解码为 RGB numpy 数组。"""

    return numpy.asarray(Image.open(io.BytesIO(data)).convert("RGB"))


@lru_cache(maxsize=None)
def blank_rgb_jpeg(width: int, height: int, quality: int) -> bytes:
    """生成指定尺寸和质量的纯白 JPEG 占位图块。"""

    image = Image.new("RGB", (width, height), (255, 255, 255))
    buffer = io.BytesIO()
    image.save(buffer, format="JPEG", quality=quality)
    return buffer.getvalue()
