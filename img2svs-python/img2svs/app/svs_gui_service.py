from __future__ import annotations

import contextlib
import io
import queue
import subprocess
import sys
import threading
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Callable, Sequence

from img2svs.app import convert_to_svs
from img2svs.core.svs_common import (
    ConversionJob,
    collect_inputs,
    format_elapsed,
    resolve_output_path,
)

GUI_SUPPORTED_SUFFIXES = set().union(
    *(spec.suffixes for spec in convert_to_svs.FORMAT_REGISTRY.values())
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

    return lambda: convert_to_svs.run_backend_job(
        backend,
        input_path,
        output_path,
        jpeg_quality=options.jpeg_quality,
        tile_size=options.tile_size,
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
        slide.suffix.lower() in convert_to_svs.FORMAT_REGISTRY["mdsx"].suffixes
        for slide, _ in slides
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


def _worker_command(
    job: ConversionJob,
    options: GuiConversionOptions,
) -> list[str]:
    """构造独立转换进程命令，兼容源码运行和 PyInstaller EXE。"""

    backend = convert_to_svs.detect_backend(job.input_path, options.input_format)
    if getattr(sys, "frozen", False):
        command = [sys.executable, "--worker"]
    else:
        entrypoint = Path(__file__).resolve().parents[2] / "svs_gui.py"
        command = [sys.executable, "-u", str(entrypoint), "--worker"]
    command.extend(
        [
            str(job.input_path),
            "-o",
            str(job.output_path),
            "--format",
            backend,
        ]
    )
    if options.jpeg_quality is not None:
        command.extend(["--jpeg-quality", str(options.jpeg_quality)])
    if options.skip_associated:
        command.append("--skip-associated")
    if options.overwrite:
        command.append("--overwrite")
    return command


def execute_jobs_subprocess(
    jobs: Sequence[ConversionJob],
    options: GuiConversionOptions,
    *,
    log_callback: LogCallback | None = None,
    progress_callback: ProgressCallback | None = None,
    stop_event: threading.Event | None = None,
) -> ExecutionSummary:
    """在独立进程中执行任务，避免大文件解析阻塞 Tk 主线程和 GIL。"""

    total = len(jobs)
    noop_log = log_callback or (lambda _message: None)
    results: list[JobResult] = []
    cancelled = False

    for index, job in enumerate(jobs, start=1):
        if stop_event and stop_event.is_set():
            cancelled = True
            noop_log("Stopped: remaining tasks were cancelled before the next file.")
            break

        noop_log(f"[{index}/{total}] {job.input_path}")
        if progress_callback:
            progress_callback(index - 1, total, job, "starting")

        started_at = time.perf_counter()
        process: subprocess.Popen[str] | None = None
        worker_output: list[str] = []
        try:
            process = subprocess.Popen(
                _worker_command(job, options),
                cwd=(
                    str(Path(sys.executable).resolve().parent)
                    if getattr(sys, "frozen", False)
                    else str(Path(__file__).resolve().parents[2])
                ),
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                text=True,
                bufsize=1,
                creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0),
            )
            output_queue: queue.Queue[str | None] = queue.Queue()

            def read_output() -> None:
                assert process is not None and process.stdout is not None
                try:
                    for line in process.stdout:
                        output_queue.put(line.rstrip())
                finally:
                    process.stdout.close()
                    output_queue.put(None)

            reader = threading.Thread(target=read_output, daemon=True)
            reader.start()
            reader_finished = False
            last_heartbeat = started_at
            while process.poll() is None or not reader_finished or not output_queue.empty():
                try:
                    message = output_queue.get(timeout=0.5)
                except queue.Empty:
                    now = time.perf_counter()
                    if process.poll() is None and now - last_heartbeat >= 5:
                        elapsed = format_elapsed(now - started_at)
                        noop_log(f"仍在处理：{job.input_path.name}（已用时 {elapsed}）")
                        last_heartbeat = now
                    continue
                if message is None:
                    reader_finished = True
                elif message:
                    worker_output.append(message)
                    noop_log(message)
            return_code = process.wait()
            reader.join(timeout=1)
            if return_code != 0:
                error_prefix = f"{job.input_path}: "
                detail = next(
                    (
                        line[len(error_prefix) :]
                        for line in reversed(worker_output)
                        if line.startswith(error_prefix)
                    ),
                    None,
                )
                raise RuntimeError(
                    detail or f"worker exited with code {return_code}"
                )
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
            continue

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
