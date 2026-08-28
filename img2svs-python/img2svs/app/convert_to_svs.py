#!/usr/bin/env python3
from __future__ import annotations

import argparse
from dataclasses import dataclass
from pathlib import Path
from typing import Callable, Sequence

from img2svs.converters import (
    csp_to_svs,
    dmetrix_to_svs,
    kfb_to_svs,
    mdsx_to_svs,
    mrxs_to_svs,
    ndpi_to_svs,
    sdpc_to_svs,
)
from img2svs.core.svs_common import (
    BatchOptions,
    ConversionJob,
    add_jpeg_quality_argument,
    add_batch_arguments,
    batch_options_from_args,
    collect_inputs,
    resolve_output_path,
    run_conversion_jobs,
)

@dataclass(frozen=True)
class FormatSpec:
    """描述一个输入格式的后缀、提示文字和单文件转换入口。"""

    suffixes: frozenset[str]
    suffix_label: str
    convert_one: Callable[..., None]


FORMAT_REGISTRY: dict[str, FormatSpec] = {
    "csp": FormatSpec(frozenset(csp_to_svs.SUPPORTED_SUFFIXES), ".csp", csp_to_svs.convert_one),
    "dmetrix": FormatSpec(
        frozenset(dmetrix_to_svs.SUPPORTED_SUFFIXES), ".dmetrix", dmetrix_to_svs.convert_one
    ),
    "kfb": FormatSpec(frozenset(kfb_to_svs.SUPPORTED_SUFFIXES), ".kfb", kfb_to_svs.convert_one),
    "mdsx": FormatSpec(
        frozenset(mdsx_to_svs.SUPPORTED_SUFFIXES), ".mdsx/.msdx", mdsx_to_svs.convert_one
    ),
    "mrxs": FormatSpec(frozenset(mrxs_to_svs.SUPPORTED_SUFFIXES), ".mrxs", mrxs_to_svs.convert_one),
    "ndpi": FormatSpec(frozenset(ndpi_to_svs.SUPPORTED_SUFFIXES), ".ndpi", ndpi_to_svs.convert_one),
    "sdpc": FormatSpec(
        frozenset(sdpc_to_svs.SUPPORTED_SUFFIXES), ".sdpc/.dyqx", sdpc_to_svs.convert_one
    ),
}
ALL_SUPPORTED_SUFFIXES = set().union(*(spec.suffixes for spec in FORMAT_REGISTRY.values()))


@dataclass(frozen=True)
class CliOptions(BatchOptions):
    """统一入口额外需要的格式选择和 MDSX 校验参数。"""

    input_format: str = "auto"
    tile_size: int | None = None


def parse_args(argv: Sequence[str] | None = None) -> CliOptions:
    """解析统一入口的命令行参数。"""

    parser = argparse.ArgumentParser(
        description=(
            "Convert supported whole-slide formats "
            "(.csp, .dmetrix, .kfb, .mdsx, .msdx, .mrxs, .ndpi, .sdpc, .dyqx) to SVS."
        )
    )
    add_batch_arguments(parser, "Path to an input slide file or directory.")
    add_jpeg_quality_argument(parser)
    parser.add_argument(
        "--format",
        choices=("auto", "csp", "dmetrix", "kfb", "mdsx", "mrxs", "ndpi", "sdpc"),
        default="auto",
        help="Input format. Default: auto-detect by file suffix",
    )
    parser.add_argument(
        "--tile-size",
        type=int,
        help="Validate the embedded MDSX tile size against an expected value.",
    )
    args = parser.parse_args(argv)
    batch = batch_options_from_args(args)
    return CliOptions(
        input_path=batch.input_path,
        output_path=batch.output_path,
        output_dir=batch.output_dir,
        skip_associated=batch.skip_associated,
        overwrite=batch.overwrite,
        jpeg_quality=batch.jpeg_quality,
        input_format=args.format,
        tile_size=args.tile_size,
    )


def format_suffixes_for_selection(input_format: str) -> tuple[set[str], str]:
    """根据格式选择结果返回允许的后缀集合和提示文字。"""

    spec = FORMAT_REGISTRY.get(input_format)
    if spec is not None:
        return set(spec.suffixes), spec.suffix_label
    return set(ALL_SUPPORTED_SUFFIXES), "supported slide"


def detect_backend(input_path: Path, input_format: str) -> str:
    """根据参数或文件后缀判断应交给哪个后端处理。"""

    suffix = input_path.suffix.lower()
    if input_format != "auto":
        spec = FORMAT_REGISTRY.get(input_format)
        if spec is None:
            raise ValueError(f"Unsupported input format: {input_format}")
        if suffix not in spec.suffixes:
            raise ValueError(f"Input file is not a {spec.suffix_label} slide: {input_path}")
        return input_format
    for backend, spec in FORMAT_REGISTRY.items():
        if suffix in spec.suffixes:
            return backend
    raise ValueError(f"Unsupported input file suffix: {input_path}")


def build_jobs(options: CliOptions) -> list[ConversionJob]:
    """扫描输入路径并为统一入口构造混合格式任务列表。"""

    supported_suffixes, suffix_label = format_suffixes_for_selection(options.input_format)
    slides = collect_inputs(options.input_path, supported_suffixes, suffix_label)
    if len(slides) > 1 and options.output_path is not None:
        raise ValueError("--output can only be used with a single input file")
    if options.tile_size is not None and not any(
        slide.suffix.lower() in mdsx_to_svs.SUPPORTED_SUFFIXES for slide in slides
    ):
        raise ValueError("--tile-size only applies to .mdsx/.msdx inputs")

    input_root = options.input_path if options.input_path.is_dir() else slides[0]
    jobs: list[ConversionJob] = []
    for slide in slides:
        # 统一入口先按公共规则计算输出路径，再按后缀分发到具体后端。
        output_path = resolve_output_path(
            slide,
            input_root,
            options.output_path,
            options.output_dir,
        )
        backend = detect_backend(slide, options.input_format)
        jobs.append(
            ConversionJob(
                input_path=slide,
                output_path=output_path,
                runner=lambda slide=slide, output_path=output_path, backend=backend: run_backend_job(
                    backend,
                    slide,
                    output_path,
                    jpeg_quality=options.jpeg_quality,
                    tile_size=options.tile_size,
                    skip_associated=options.skip_associated,
                    overwrite=options.overwrite,
                ),
            )
        )
    return jobs


def run_backend_job(
    backend: str,
    input_path: Path,
    output_path: Path,
    *,
    jpeg_quality: int | None,
    tile_size: int | None,
    skip_associated: bool,
    overwrite: bool,
) -> None:
    """通过注册表执行一个格式后端，集中处理格式特有参数。"""

    spec = FORMAT_REGISTRY.get(backend)
    if spec is None:
        raise ValueError(f"Unsupported input format: {backend}")
    print(f"Format: {backend}")
    kwargs = {
        "input_path": input_path,
        "output_path": output_path,
        "jpeg_quality": jpeg_quality,
        "skip_associated": skip_associated,
        "overwrite": overwrite,
    }
    if backend == "mdsx":
        kwargs["tile_size"] = tile_size
    spec.convert_one(**kwargs)

def main() -> None:
    """程序入口：构建任务并交给公共执行器运行。"""

    run_conversion_jobs(build_jobs(parse_args()))


if __name__ == "__main__":
    main()
