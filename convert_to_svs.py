#!/usr/bin/env python3
from __future__ import annotations

import argparse
from dataclasses import dataclass
from pathlib import Path
from typing import Sequence

import csp_to_svs
import kfb_to_svs
import mdsx_to_svs
import mrxs_to_svs
import ndpi_to_svs
import sdpc_to_svs
from svs_common import (
    BatchOptions,
    ConversionJob,
    add_jpeg_quality_argument,
    add_batch_arguments,
    batch_options_from_args,
    collect_inputs,
    resolve_output_path,
    run_conversion_jobs,
)

ALL_SUPPORTED_SUFFIXES = (
    csp_to_svs.SUPPORTED_SUFFIXES
    |
    kfb_to_svs.SUPPORTED_SUFFIXES
    | mdsx_to_svs.SUPPORTED_SUFFIXES
    | mrxs_to_svs.SUPPORTED_SUFFIXES
    | ndpi_to_svs.SUPPORTED_SUFFIXES
    | sdpc_to_svs.SUPPORTED_SUFFIXES
)


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
            "(.csp, .kfb, .mdsx, .msdx, .mrxs, .ndpi, .sdpc, .dyqx) to SVS."
        )
    )
    add_batch_arguments(parser, "Path to an input slide file or directory.")
    add_jpeg_quality_argument(parser)
    parser.add_argument(
        "--format",
        choices=("auto", "csp", "kfb", "mdsx", "mrxs", "ndpi", "sdpc"),
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

    if input_format == "csp":
        return csp_to_svs.SUPPORTED_SUFFIXES, ".csp"
    if input_format == "kfb":
        return kfb_to_svs.SUPPORTED_SUFFIXES, ".kfb"
    if input_format == "mdsx":
        return mdsx_to_svs.SUPPORTED_SUFFIXES, ".mdsx/.msdx"
    if input_format == "mrxs":
        return mrxs_to_svs.SUPPORTED_SUFFIXES, ".mrxs"
    if input_format == "ndpi":
        return ndpi_to_svs.SUPPORTED_SUFFIXES, ".ndpi"
    if input_format == "sdpc":
        return sdpc_to_svs.SUPPORTED_SUFFIXES, ".sdpc/.dyqx"
    return ALL_SUPPORTED_SUFFIXES, "supported slide"


def detect_backend(input_path: Path, input_format: str) -> str:
    """根据参数或文件后缀判断应交给哪个后端处理。"""

    suffix = input_path.suffix.lower()
    if input_format == "csp":
        if suffix not in csp_to_svs.SUPPORTED_SUFFIXES:
            raise ValueError(f"Input file is not a .csp slide: {input_path}")
        return "csp"
    if input_format == "kfb":
        if suffix not in kfb_to_svs.SUPPORTED_SUFFIXES:
            raise ValueError(f"Input file is not a .kfb slide: {input_path}")
        return "kfb"
    if input_format == "mdsx":
        if suffix not in mdsx_to_svs.SUPPORTED_SUFFIXES:
            raise ValueError(f"Input file is not an .mdsx/.msdx slide: {input_path}")
        return "mdsx"
    if input_format == "mrxs":
        if suffix not in mrxs_to_svs.SUPPORTED_SUFFIXES:
            raise ValueError(f"Input file is not an .mrxs slide: {input_path}")
        return "mrxs"
    if input_format == "ndpi":
        if suffix not in ndpi_to_svs.SUPPORTED_SUFFIXES:
            raise ValueError(f"Input file is not an .ndpi slide: {input_path}")
        return "ndpi"
    if input_format == "sdpc":
        if suffix not in sdpc_to_svs.SUPPORTED_SUFFIXES:
            raise ValueError(f"Input file is not an .sdpc/.dyqx slide: {input_path}")
        return "sdpc"
    if suffix in csp_to_svs.SUPPORTED_SUFFIXES:
        return "csp"
    if suffix in kfb_to_svs.SUPPORTED_SUFFIXES:
        return "kfb"
    if suffix in mdsx_to_svs.SUPPORTED_SUFFIXES:
        return "mdsx"
    if suffix in mrxs_to_svs.SUPPORTED_SUFFIXES:
        return "mrxs"
    if suffix in ndpi_to_svs.SUPPORTED_SUFFIXES:
        return "ndpi"
    if suffix in sdpc_to_svs.SUPPORTED_SUFFIXES:
        return "sdpc"
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
        if backend == "csp":
            jobs.append(
                ConversionJob(
                    input_path=slide,
                    output_path=output_path,
                    runner=lambda slide=slide, output_path=output_path: run_csp_job(
                        slide, output_path, options
                    ),
                )
            )
        elif backend == "kfb":
            jobs.append(
                ConversionJob(
                    input_path=slide,
                    output_path=output_path,
                    runner=lambda slide=slide, output_path=output_path: run_kfb_job(
                        slide, output_path, options
                    ),
                )
            )
        elif backend == "mdsx":
            jobs.append(
                ConversionJob(
                    input_path=slide,
                    output_path=output_path,
                    runner=lambda slide=slide, output_path=output_path: run_mdsx_job(
                        slide, output_path, options
                    ),
                )
            )
        elif backend == "ndpi":
            jobs.append(
                ConversionJob(
                    input_path=slide,
                    output_path=output_path,
                    runner=lambda slide=slide, output_path=output_path: run_ndpi_job(
                        slide, output_path, options
                    ),
                )
            )
        elif backend == "mrxs":
            jobs.append(
                ConversionJob(
                    input_path=slide,
                    output_path=output_path,
                    runner=lambda slide=slide, output_path=output_path: run_mrxs_job(
                        slide, output_path, options
                    ),
                )
            )
        else:
            jobs.append(
                ConversionJob(
                    input_path=slide,
                    output_path=output_path,
                    runner=lambda slide=slide, output_path=output_path: run_sdpc_job(
                        slide, output_path, options
                    ),
                )
            )
    return jobs


def run_kfb_job(input_path: Path, output_path: Path, options: CliOptions) -> None:
    """执行单个 KFB 输入的转换任务。"""

    print("Format: kfb")
    kfb_to_svs.convert_one(
        input_path=input_path,
        output_path=output_path,
        jpeg_quality=options.jpeg_quality,
        skip_associated=options.skip_associated,
        overwrite=options.overwrite,
    )


def run_csp_job(input_path: Path, output_path: Path, options: CliOptions) -> None:
    """执行单个 CSP 输入的转换任务。"""

    print("Format: csp")
    csp_to_svs.convert_one(
        input_path=input_path,
        output_path=output_path,
        jpeg_quality=options.jpeg_quality,
        skip_associated=options.skip_associated,
        overwrite=options.overwrite,
    )


def run_mdsx_job(input_path: Path, output_path: Path, options: CliOptions) -> None:
    """执行单个 MDSX 输入的转换任务。"""

    print("Format: mdsx")
    mdsx_to_svs.convert_one(
        input_path=input_path,
        output_path=output_path,
        tile_size=options.tile_size,
        jpeg_quality=options.jpeg_quality,
        skip_associated=options.skip_associated,
        overwrite=options.overwrite,
    )


def run_ndpi_job(input_path: Path, output_path: Path, options: CliOptions) -> None:
    """执行单个 NDPI 输入的转换任务。"""

    print("Format: ndpi")
    ndpi_to_svs.convert_one(
        input_path=input_path,
        output_path=output_path,
        jpeg_quality=options.jpeg_quality,
        skip_associated=options.skip_associated,
        overwrite=options.overwrite,
    )


def run_mrxs_job(input_path: Path, output_path: Path, options: CliOptions) -> None:
    """执行单个 MRXS 输入的转换任务。"""

    print("Format: mrxs")
    mrxs_to_svs.convert_one(
        input_path=input_path,
        output_path=output_path,
        jpeg_quality=options.jpeg_quality,
        skip_associated=options.skip_associated,
        overwrite=options.overwrite,
    )


def run_sdpc_job(input_path: Path, output_path: Path, options: CliOptions) -> None:
    """执行单个 SDPC/DYQX 输入的转换任务。"""

    print("Format: sdpc")
    sdpc_to_svs.convert_one(
        input_path=input_path,
        output_path=output_path,
        jpeg_quality=options.jpeg_quality,
        skip_associated=options.skip_associated,
        overwrite=options.overwrite,
    )

def main() -> None:
    """程序入口：构建任务并交给公共执行器运行。"""

    run_conversion_jobs(build_jobs(parse_args()))


if __name__ == "__main__":
    main()
