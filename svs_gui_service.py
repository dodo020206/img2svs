from __future__ import annotations

import contextlib
import io
import threading
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Callable, Sequence

import csp_to_svs
import convert_to_svs
import dmetrix_to_svs
import kfb_to_svs
import mdsx_to_svs
import mrxs_to_svs
import ndpi_to_svs
import sdpc_to_svs
from svs_common import ConversionJob, collect_inputs, format_elapsed, resolve_output_path

GUI_SUPPORTED_SUFFIXES = (
    csp_to_svs.SUPPORTED_SUFFIXES
    | dmetrix_to_svs.SUPPORTED_SUFFIXES
    | kfb_to_svs.SUPPORTED_SUFFIXES
    | mdsx_to_svs.SUPPORTED_SUFFIXES
    | mrxs_to_svs.SUPPORTED_SUFFIXES
    | ndpi_to_svs.SUPPORTED_SUFFIXES
    | sdpc_to_svs.SUPPORTED_SUFFIXES
)

ProgressCallback = Callable[[int, int, ConversionJob, str], None]
LogCallback = Callable[[str], None]


@dataclass(frozen=True)
class GuiConversionOptions:
    """桌面界面使用的批量转换配置。"""

    inputs: tuple[Path, ...]
    output_dir: Path | None = None
    input_format: str = "auto"
    tile_size: int | None = None
    jpeg_quality: int | None = None
    skip_associated: bool = False
    overwrite: bool = False


@dataclass(frozen=True)
class JobResult:
    """记录单个任务的执行结果。"""

    input_path: Path
    output_path: Path
    success: bool
    duration_seconds: float
    message: str = ""


@dataclass(frozen=True)
class ExecutionSummary:
    """记录整批任务的执行摘要。"""

    results: tuple[JobResult, ...]
    total: int
    completed: int
    succeeded: int
    failed: int
    cancelled: bool


class CallbackTextWriter(io.TextIOBase):
    """把 print 输出按行转发给界面的日志回调。"""

    def __init__(self, callback: LogCallback):
        self._callback = callback
        self._buffer = ""

    def write(self, text: str) -> int:
        self._buffer += text
        while "\n" in self._buffer:
            line, self._buffer = self._buffer.split("\n", 1)
            if line.rstrip():
                self._callback(line.rstrip())
        return len(text)

    def flush(self) -> None:
        if self._buffer.rstrip():
            self._callback(self._buffer.rstrip())
        self._buffer = ""


def normalize_inputs(inputs: Sequence[Path | str]) -> tuple[Path, ...]:
    """把界面传入的路径统一解析为绝对路径。"""

    normalized: list[Path] = []
    seen: set[Path] = set()
    for raw_path in inputs:
        path = Path(raw_path).expanduser().resolve()
        if path in seen:
            continue
        if not path.exists():
            raise FileNotFoundError(f"Input path not found: {path}")
        normalized.append(path)
        seen.add(path)
    return tuple(normalized)


def make_job_runner(
    backend: str,
    input_path: Path,
    output_path: Path,
    options: GuiConversionOptions,
) -> Callable[[], None]:
    """根据后端格式构造单个任务的实际执行器。"""

    if backend == "csp":
        return lambda: csp_to_svs.convert_one(
            input_path=input_path,
            output_path=output_path,
            jpeg_quality=options.jpeg_quality,
            skip_associated=options.skip_associated,
            overwrite=options.overwrite,
        )
    if backend == "dmetrix":
        return lambda: dmetrix_to_svs.convert_one(
            input_path=input_path,
            output_path=output_path,
            jpeg_quality=options.jpeg_quality,
            skip_associated=options.skip_associated,
            overwrite=options.overwrite,
        )
    if backend == "kfb":
        return lambda: kfb_to_svs.convert_one(
            input_path=input_path,
            output_path=output_path,
            jpeg_quality=options.jpeg_quality,
            skip_associated=options.skip_associated,
            overwrite=options.overwrite,
        )
    if backend == "mdsx":
        return lambda: mdsx_to_svs.convert_one(
            input_path=input_path,
            output_path=output_path,
            tile_size=options.tile_size,
            jpeg_quality=options.jpeg_quality,
            skip_associated=options.skip_associated,
            overwrite=options.overwrite,
        )
    if backend == "mrxs":
        return lambda: mrxs_to_svs.convert_one(
            input_path=input_path,
            output_path=output_path,
            jpeg_quality=options.jpeg_quality,
            skip_associated=options.skip_associated,
            overwrite=options.overwrite,
        )
    if backend == "ndpi":
        return lambda: ndpi_to_svs.convert_one(
            input_path=input_path,
            output_path=output_path,
            jpeg_quality=options.jpeg_quality,
            skip_associated=options.skip_associated,
            overwrite=options.overwrite,
        )
    return lambda: sdpc_to_svs.convert_one(
        input_path=input_path,
        output_path=output_path,
        jpeg_quality=options.jpeg_quality,
        skip_associated=options.skip_associated,
        overwrite=options.overwrite,
    )


def plan_jobs(options: GuiConversionOptions) -> list[ConversionJob]:
    """根据界面输入规划转换任务列表。"""

    inputs = normalize_inputs(options.inputs)
    if not inputs:
        raise ValueError("Please select at least one input file or directory")

    supported_suffixes, suffix_label = convert_to_svs.format_suffixes_for_selection(
        options.input_format
    )
    slides: list[tuple[Path, Path]] = []
    seen_slides: set[Path] = set()
    for input_root in inputs:
        matched = collect_inputs(input_root, supported_suffixes, suffix_label)
        base_root = input_root if input_root.is_dir() else matched[0]
        for slide in matched:
            slide = slide.resolve()
            if slide in seen_slides:
                continue
            slides.append((slide, base_root))
            seen_slides.add(slide)

    if not slides:
        raise FileNotFoundError("No supported slide files were found in the selected inputs")

    if options.tile_size is not None and options.tile_size <= 0:
        raise ValueError("Tile size must be a positive integer")

    if options.tile_size is not None and not any(
        slide.suffix.lower() in mdsx_to_svs.SUPPORTED_SUFFIXES for slide, _ in slides
    ):
        raise ValueError("Tile size validation only applies to MDSX/MSDX inputs")

    output_dir = options.output_dir.expanduser().resolve() if options.output_dir else None
    if output_dir is not None and output_dir.exists() and not output_dir.is_dir():
        raise ValueError(f"Output directory is not a directory: {output_dir}")
    planned_outputs: dict[Path, Path] = {}
    jobs: list[ConversionJob] = []
    for slide, base_root in slides:
        output_path = resolve_output_path(slide, base_root, None, output_dir).resolve()
        previous_source = planned_outputs.get(output_path)
        if previous_source is not None and previous_source != slide:
            raise ValueError(
                f"Output path conflict: {output_path} would be generated from both "
                f"{previous_source} and {slide}"
            )
        planned_outputs[output_path] = slide
        backend = convert_to_svs.detect_backend(slide, options.input_format)
        jobs.append(
            ConversionJob(
                input_path=slide,
                output_path=output_path,
                runner=make_job_runner(backend, slide, output_path, options),
            )
        )
    return jobs


def execute_jobs(
    jobs: Sequence[ConversionJob],
    *,
    log_callback: LogCallback | None = None,
    progress_callback: ProgressCallback | None = None,
    stop_event: threading.Event | None = None,
) -> ExecutionSummary:
    """执行转换任务，并把日志和进度转发给 GUI。"""

    total = len(jobs)
    if total == 0:
        return ExecutionSummary(
            results=(),
            total=0,
            completed=0,
            succeeded=0,
            failed=0,
            cancelled=False,
        )

    noop_log = log_callback or (lambda _message: None)
    writer = CallbackTextWriter(noop_log)

    results: list[JobResult] = []
    cancelled = False
    with contextlib.redirect_stdout(writer), contextlib.redirect_stderr(writer):
        for index, job in enumerate(jobs, start=1):
            if stop_event and stop_event.is_set():
                cancelled = True
                noop_log("Stopped: remaining tasks were cancelled after the current file.")
                break

            noop_log(f"[{index}/{total}] {job.input_path}")
            if progress_callback:
                progress_callback(index - 1, total, job, "starting")

            started_at = time.perf_counter()
            try:
                job.runner()
            except Exception as exc:
                elapsed = time.perf_counter() - started_at
                noop_log(f"Failed: {job.input_path} ({exc})")
                noop_log(f"Time  : {format_elapsed(elapsed)}")
                results.append(
                    JobResult(
                        input_path=job.input_path,
                        output_path=job.output_path,
                        success=False,
                        duration_seconds=elapsed,
                        message=str(exc),
                    )
                )
                if progress_callback:
                    progress_callback(index, total, job, "failed")
            else:
                elapsed = time.perf_counter() - started_at
                noop_log(f"Time  : {format_elapsed(elapsed)}")
                results.append(
                    JobResult(
                        input_path=job.input_path,
                        output_path=job.output_path,
                        success=True,
                        duration_seconds=elapsed,
                    )
                )
                if progress_callback:
                    progress_callback(index, total, job, "succeeded")
        writer.flush()

    succeeded = sum(1 for result in results if result.success)
    failed = len(results) - succeeded
    noop_log(
        "Finished: "
        f"total={total}, success={succeeded}, failed={failed}, cancelled={int(cancelled)}"
    )
    return ExecutionSummary(
        results=tuple(results),
        total=total,
        completed=len(results),
        succeeded=succeeded,
        failed=failed,
        cancelled=cancelled,
    )
