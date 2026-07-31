#!/usr/bin/env python3
from __future__ import annotations

import argparse
import math
import os
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import Sequence

import numpy
import tifffile
from PIL import Image

from svs_common import (
    APERIO_VERSION,
    BatchOptions,
    add_jpeg_quality_argument,
    add_batch_arguments,
    aperio_associated_description,
    batch_options_from_args,
    build_single_format_jobs,
    format_associated_summary,
    format_level_summary,
    jpeg_compressionargs,
    normalize_jpeg_quality,
    pixels_per_centimeter,
    run_conversion_jobs,
)

SUPPORTED_SUFFIXES = {".mrxs"}
DEFAULT_TILE_SIZE = 256
DEFAULT_JPEG_QUALITY = 70
BIGTIFF_THRESHOLD_BYTES = 3_500_000_000


def _looks_like_vips_root(path: Path) -> bool:
    """判断目录是否像一个可用的 libvips 根目录。"""

    if not path.exists() or not path.is_dir():
        return False
    if any(path.glob("libvips-*.dll")):
        return True
    if any((path / "bin").glob("libvips-*.dll")):
        return True
    return (
        (path / "bin").is_dir()
        and (path / "lib").is_dir()
        and (path / "share").is_dir()
    )


def configure_vips_runtime() -> None:
    """在源码运行和 PyInstaller 运行时补充 libvips 的 DLL 搜索路径。"""

    frozen = bool(getattr(sys, "frozen", False))
    module_dir = Path(__file__).resolve().parent
    exe_dir = Path(sys.executable).resolve().parent if frozen else None

    raw_candidates = (
        [
            getattr(sys, "_MEIPASS", None),
            exe_dir / "_internal" if exe_dir is not None else None,
            exe_dir,
            exe_dir / "vips" if exe_dir is not None else None,
            os.environ.get("VIPS_HOME"),
            module_dir / "vips",
        ]
        if frozen
        else [
            os.environ.get("VIPS_HOME"),
            module_dir / "vips",
        ]
    )

    candidate_roots: list[Path] = []
    seen_roots: set[Path] = set()
    for raw in raw_candidates:
        if not raw:
            continue
        root = Path(raw).expanduser().resolve()
        for candidate in (root, root / "_internal", root / "vips"):
            if candidate in seen_roots or not _looks_like_vips_root(candidate):
                continue
            seen_roots.add(candidate)
            candidate_roots.append(candidate)

    if not candidate_roots:
        return

    primary_root = candidate_roots[0]
    os.environ["VIPS_HOME"] = str(primary_root)
    os.environ["VIPSHOME"] = str(primary_root)
    os.environ["VIPS_PREFIX"] = str(primary_root)

    search_dirs: list[Path] = []
    for root in candidate_roots:
        for candidate in (root, root / "bin", root / "lib"):
            if candidate.exists() and candidate.is_dir() and candidate not in search_dirs:
                search_dirs.append(candidate)

    current_path_entries = [
        entry for entry in os.environ.get("PATH", "").split(os.pathsep) if entry
    ]
    ordered_path_entries: list[str] = []
    for directory in search_dirs:
        directory_text = str(directory)
        if directory_text not in ordered_path_entries:
            ordered_path_entries.append(directory_text)
    for entry in current_path_entries:
        if entry not in ordered_path_entries:
            ordered_path_entries.append(entry)
    os.environ["PATH"] = os.pathsep.join(ordered_path_entries)

    for directory in search_dirs:
        directory_text = str(directory)
        add_dll_directory = getattr(os, "add_dll_directory", None)
        if add_dll_directory is not None:
            try:
                add_dll_directory(directory_text)
            except OSError:
                pass


def load_pyvips():
    """延迟导入 pyvips，并在缺失依赖时给出清晰提示。"""

    configure_vips_runtime()
    try:
        import pyvips
    except ModuleNotFoundError as exc:
        raise RuntimeError(
            "MRXS conversion requires the optional 'pyvips' package. "
            "Install pyvips and make sure libvips is available before rerunning."
        ) from exc
    except OSError as exc:
        raise RuntimeError(
            "MRXS conversion requires libvips runtime libraries. "
            "Please install libvips and make sure VIPS_HOME or PATH points to it."
        ) from exc
    return pyvips


def mrxs_data_directory(path: Path) -> Path:
    """返回 .mrxs 旁边存放 Data*.dat 的同名目录。"""

    return path.with_suffix("")


def mrxs_total_bytes(path: Path) -> int:
    """估算 .mrxs 整个幻灯片的占用空间，包含数据目录中的全部文件。"""

    total = path.stat().st_size if path.exists() else 0
    data_dir = mrxs_data_directory(path)
    if data_dir.is_dir():
        for entry in data_dir.rglob("*"):
            if entry.is_file():
                try:
                    total += entry.stat().st_size
                except OSError:
                    continue
    return total


def should_use_bigtiff_for_mrxs(path: Path, level0_pixels: int = 0) -> bool:
    """根据原始像素体积或数据目录大小判断是否需要 BigTIFF。

    MRXS 主图分辨率往往非常大，即便 JPEG 压缩后也容易超过 32-bit TIFF
    的 4 GB 上限，所以在此估算 L0 原始 RGB 字节数（每像素 3 字节）作为
    主要依据，再叠加数据目录大小做兜底。
    """

    raw_bytes = level0_pixels * 3
    if raw_bytes >= BIGTIFF_THRESHOLD_BYTES:
        return True
    return mrxs_total_bytes(path) >= BIGTIFF_THRESHOLD_BYTES


@dataclass(frozen=True)
class SlideMetadata:
    """保存主图的核心显示元数据。"""

    width: int
    height: int
    mpp: float
    app_mag: float
    jpeg_quality: int


@dataclass(frozen=True)
class PyramidLevel:
    """描述一个金字塔层级的尺寸和瓦片布局。"""

    index: int
    width: int
    height: int
    downsample: float
    tile_cols: int
    tile_rows: int


@dataclass(frozen=True)
class AssociatedImageEntry:
    """描述 MRXS 中可读取的关联图名称。"""

    kind: str


@dataclass(frozen=True)
class MrxsSlide:
    """保存 MRXS 解析完成后的统一幻灯片对象。"""

    path: Path
    metadata: SlideMetadata
    tile_size: int
    levels: tuple[PyramidLevel, ...]
    associated_images: tuple[AssociatedImageEntry, ...]


@dataclass(frozen=True)
class CliOptions(BatchOptions):
    """MRXS 转换入口额外支持的编码参数。"""

    tile_size: int = DEFAULT_TILE_SIZE
    jpeg_quality: int = DEFAULT_JPEG_QUALITY


def parse_args(argv: Sequence[str] | None = None) -> CliOptions:
    """解析 MRXS 转换脚本的命令行参数。"""

    parser = argparse.ArgumentParser(
        description="Convert 3DHISTECH Pannoramic .mrxs whole-slide images to Aperio SVS."
    )
    add_batch_arguments(parser, "Path to an input .mrxs file or directory.")
    parser.add_argument(
        "--tile-size",
        type=int,
        default=DEFAULT_TILE_SIZE,
        help=f"Output tile size. Default: {DEFAULT_TILE_SIZE}",
    )
    add_jpeg_quality_argument(
        parser,
        default=DEFAULT_JPEG_QUALITY,
        help_text=f"JPEG quality for the output SVS. Default: {DEFAULT_JPEG_QUALITY}",
    )
    args = parser.parse_args(argv)
    batch = batch_options_from_args(args)
    return CliOptions(
        input_path=batch.input_path,
        output_path=batch.output_path,
        output_dir=batch.output_dir,
        skip_associated=batch.skip_associated,
        overwrite=batch.overwrite,
        tile_size=args.tile_size,
        jpeg_quality=batch.jpeg_quality,
    )


def maybe_float(value: object) -> float | None:
    """尝试把对象转换为浮点数，失败时返回 None。"""

    if value is None:
        return None
    try:
        return float(value)
    except (TypeError, ValueError):
        return None


def get_field_or_none(image, field: str) -> object | None:
    """安全读取 pyvips 图像字段。"""

    if image.get_typeof(field) == 0:
        return None
    return image.get(field)


def normalize_main_image(image):
    """把 pyvips 读出的主图归一化为可写入 JPEG 的 RGB 图像。"""

    if image.hasalpha():
        image = image.flatten(background=[255, 255, 255])
    if image.bands == 1:
        image = image.colourspace("srgb")
    elif image.bands > 3:
        image = image[:3]
    return image


def vips_to_numpy(image) -> numpy.ndarray:
    """把 pyvips 图像转换成 RGB numpy 数组。"""

    memory = image.write_to_memory()
    array = numpy.frombuffer(memory, dtype=numpy.uint8)
    return array.reshape(image.height, image.width, image.bands).copy()


def aperio_main_description(metadata: SlideMetadata, tile_size: int, level: PyramidLevel) -> str:
    """生成主图页面写入 SVS 时使用的 Aperio 描述字符串。"""

    return (
        f"{APERIO_VERSION}\n"
        f"{level.width}x{level.height} [0,0 {level.width}x{level.height}] "
        f"({tile_size}x{tile_size}) JPEG/RGB Q={metadata.jpeg_quality}"
        f"|AppMag = {metadata.app_mag:g}"
        f"|MPP = {metadata.mpp:.6f}"
    )


def print_slide_info(slide: MrxsSlide) -> None:
    """打印 MRXS 幻灯片的摘要信息。"""

    print(
        f"Image : {slide.metadata.width}x{slide.metadata.height}, "
        f"tile={slide.tile_size}x{slide.tile_size}, levels={len(slide.levels)}"
    )
    print(
        f"Meta  : mpp={slide.metadata.mpp:.6f}, app_mag={slide.metadata.app_mag:g}, "
        f"jpeg_quality={slide.metadata.jpeg_quality}"
    )
    print(f"Pyr   : {format_level_summary(slide.levels)}")
    print(f"Assoc : {format_associated_summary(slide.associated_images)}")


class MrxsParser:
    """负责把 MRXS 文件解析成统一的幻灯片对象。"""

    def __init__(self, path: Path, *, tile_size: int, jpeg_quality: int):
        self.path = path
        self.tile_size = tile_size
        self.jpeg_quality = jpeg_quality
        self.pyvips = load_pyvips()

    def parse(self) -> MrxsSlide:
        """读取 openslide 暴露的元数据并构建层级信息。"""

        image = self._open_slide_image()
        level_count = self._resolve_level_count(image)
        mpp = self._resolve_mpp(image)
        app_mag = self._resolve_app_mag(image)

        metadata = SlideMetadata(
            width=image.width,
            height=image.height,
            mpp=mpp,
            app_mag=app_mag,
            jpeg_quality=self.jpeg_quality,
        )
        levels = tuple(self._build_level(image, index) for index in range(level_count))
        associated_images = tuple(
            AssociatedImageEntry(kind=name)
            for name in self._associated_names(image)
            if name in {"label", "macro"}
        )
        return MrxsSlide(
            path=self.path,
            metadata=metadata,
            tile_size=self.tile_size,
            levels=levels,
            associated_images=associated_images,
        )

    def _build_level(self, image, index: int) -> PyramidLevel:
        """从 openslide 元数据中构建单层信息。"""

        width = int(get_field_or_none(image, f"openslide.level[{index}].width") or 0)
        height = int(get_field_or_none(image, f"openslide.level[{index}].height") or 0)
        downsample = float(get_field_or_none(image, f"openslide.level[{index}].downsample") or 0.0)

        if width <= 0 or height <= 0:
            if index == 0:
                width = int(image.width)
                height = int(image.height)
            else:
                level_size = self._probe_level_size(index)
                if level_size is not None:
                    width, height = level_size

        if downsample <= 0:
            if index == 0:
                downsample = 1.0
            elif width > 0:
                downsample = image.width / width
            else:
                downsample = float(2**index)

        if width <= 0 or height <= 0:
            raise ValueError(f"Invalid MRXS level size at level {index}")
        return PyramidLevel(
            index=index,
            width=width,
            height=height,
            downsample=downsample,
            tile_cols=math.ceil(width / self.tile_size),
            tile_rows=math.ceil(height / self.tile_size),
        )

    def _resolve_level_count(self, image) -> int:
        """优先读取 openslide 层数，缺失时通过逐层探测回退。"""

        level_count = int(get_field_or_none(image, "openslide.level-count") or 0)
        if level_count > 0:
            return level_count

        count = 1
        while self._probe_level_size(count) is not None:
            count += 1
        return count

    def _probe_level_size(self, index: int) -> tuple[int, int] | None:
        """直接打开某一层，探测其实际宽高。"""

        try:
            image = self._open_slide_image(level=index)
        except Exception:
            return None

        if image.width <= 0 or image.height <= 0:
            return None
        return int(image.width), int(image.height)

    def _open_slide_image(self, level: int = 0):
        """优先显式使用 openslideload 打开 MRXS，避免被误判成普通文件。"""

        if hasattr(self.pyvips.Image, "openslideload"):
            return self.pyvips.Image.openslideload(
                str(self.path),
                level=level,
            )
        kwargs = {"access": "random"}
        if level:
            kwargs["level"] = level
        return self.pyvips.Image.new_from_file(str(self.path), **kwargs)

    def _resolve_mpp(self, image) -> float:
        """优先从 openslide 元数据解析 MPP，必要时回退到分辨率字段。"""

        candidates = [
            maybe_float(get_field_or_none(image, "openslide.mpp-x")),
            maybe_float(get_field_or_none(image, "openslide.mpp-y")),
        ]

        for field in (
            "mirax.LAYER_0_LEVEL_0_SECTION.MICROMETER_PER_PIXEL_X",
            "mirax.LAYER_0_LEVEL_0_SECTION.MICROMETER_PER_PIXEL_Y",
        ):
            candidates.append(maybe_float(get_field_or_none(image, field)))

        xres = maybe_float(get_field_or_none(image, "xres"))
        yres = maybe_float(get_field_or_none(image, "yres"))
        if xres and xres > 0:
            candidates.append(1000.0 / xres)
        if yres and yres > 0:
            candidates.append(1000.0 / yres)

        values = [value for value in candidates if value and value > 0]
        if not values:
            raise ValueError("Unable to determine MRXS MPP from metadata")
        return sum(values) / len(values)

    def _resolve_app_mag(self, image) -> float:
        """读取物镜倍率，缺失时回退到 0。"""

        for field in (
            "openslide.objective-power",
            "mirax.GENERAL.OBJECTIVE_MAGNIFICATION",
        ):
            value = maybe_float(get_field_or_none(image, field))
            if value and value > 0:
                return value
        return 0.0

    def _associated_names(self, image) -> tuple[str, ...]:
        """解析 openslide 暴露的关联图名称列表。"""

        raw = get_field_or_none(image, "slide-associated-images")
        if not raw:
            return ()
        return tuple(
            name.strip()
            for name in str(raw).split(",")
            if name.strip()
        )


class SvsWriter:
    """负责把 MRXS 内容写成 Aperio 风格 SVS。"""

    def __init__(self, slide: MrxsSlide):
        self.slide = slide
        self.pyvips = load_pyvips()
        self.resolution = pixels_per_centimeter(slide.metadata.mpp)
        self._level_cache: dict[int, object] = {}

    def write(self, output_path: Path, skip_associated: bool) -> None:
        """写出主图、缩略图、金字塔层和关联图。"""

        thumbnail = self._build_thumbnail()
        associated_images = {} if skip_associated else self._load_associated_images()

        self._write_pyramid_with_vips(output_path)
        with tifffile.TiffWriter(output_path, append=True) as tif:
            self._write_thumbnail(tif, thumbnail)
            self._write_associated_images(tif, associated_images)

    def _open_level_image(self, index: int):
        """按层级延迟打开 pyvips 图像。"""

        image = self._level_cache.get(index)
        if image is None:
            image = self._open_slide_image(index)
            self._level_cache[index] = normalize_main_image(image)
        return self._level_cache[index]

    def _open_slide_image(self, level: int = 0):
        """优先显式使用 openslideload 打开 MRXS 的指定层。"""

        if hasattr(self.pyvips.Image, "openslideload"):
            return self.pyvips.Image.openslideload(
                str(self.slide.path),
                level=level,
            )
        kwargs = {"access": "random"}
        if level:
            kwargs["level"] = level
        return self.pyvips.Image.new_from_file(str(self.slide.path), **kwargs)

    def _write_pyramid_with_vips(self, output_path: Path) -> None:
        """用 libvips 一次性写出含降采样层的完整金字塔。

        让 libvips 在内核里多线程做降采样和 JPEG 编码，避免 Python 逐 tile
        decode/encode 的开销。生成的页数等于 ceil(log2(max_dim/tile_size))+1，
        因此与 openslide 给出的层数可能不完全一致。
        """

        image = self._open_level_image(0).copy()
        image.set_type(
            self.pyvips.GValue.gstr_type,
            "image-description",
            aperio_main_description(
                self.slide.metadata,
                self.slide.tile_size,
                self.slide.levels[0],
            ),
        )
        image.tiffsave(
            str(output_path),
            compression="jpeg",
            Q=self.slide.metadata.jpeg_quality,
            tile=True,
            tile_width=self.slide.tile_size,
            tile_height=self.slide.tile_size,
            pyramid=True,
            subifd=False,
            bigtiff=should_use_bigtiff_for_mrxs(
                self.slide.path,
                level0_pixels=self.slide.levels[0].width * self.slide.levels[0].height,
            ),
            xres=self.resolution / 10.0,
            yres=self.resolution / 10.0,
            resunit="cm",
        )

    def _write_thumbnail(self, tif: tifffile.TiffWriter, thumbnail: numpy.ndarray) -> None:
        """写出缩略图页面。"""

        tif.write(
            data=thumbnail,
            photometric="rgb",
            compression="jpeg",
            compressionargs=jpeg_compressionargs(self.slide.metadata.jpeg_quality),
            resolution=(self.resolution, self.resolution),
            resolutionunit="CENTIMETER",
            metadata=None,
            software=False,
        )

    def _write_associated_images(
        self, tif: tifffile.TiffWriter, associated_images: dict[str, numpy.ndarray]
    ) -> None:
        """按 Aperio 约定写出 label 和 macro 页面。"""

        for kind in ("label", "macro"):
            image = associated_images.get(kind)
            if image is None:
                continue
            tif.write(
                data=image,
                photometric="rgb",
                compression="jpeg",
                compressionargs=jpeg_compressionargs(self.slide.metadata.jpeg_quality),
                description=aperio_associated_description(kind),
                metadata=None,
                software=False,
            )

    def _build_thumbnail(self, max_size: int = 1024) -> numpy.ndarray:
        """从较低分辨率层构建缩略图页面。"""

        source_level = self.slide.levels[-1]
        for level in self.slide.levels[1:]:
            if max(level.width, level.height) <= max_size * 2:
                source_level = level
                break

        image = self._open_level_image(source_level.index)
        if max(image.width, image.height) > max_size:
            scale = min(max_size / image.width, max_size / image.height)
            image = image.resize(scale)
        thumbnail = Image.fromarray(vips_to_numpy(image))
        thumbnail.thumbnail((max_size, max_size), Image.Resampling.LANCZOS)
        return numpy.asarray(thumbnail)

    def _load_associated_images(self) -> dict[str, numpy.ndarray]:
        """读取 MRXS 中可访问的关联图。"""

        images: dict[str, numpy.ndarray] = {}
        for entry in self.slide.associated_images:
            try:
                if hasattr(self.pyvips.Image, "openslideload"):
                    image = self.pyvips.Image.openslideload(
                        str(self.slide.path),
                        associated=entry.kind,
                    )
                else:
                    image = self.pyvips.Image.new_from_file(
                        str(self.slide.path),
                        associated=entry.kind,
                    )
            except Exception:
                continue
            images[entry.kind] = vips_to_numpy(normalize_main_image(image))
        return images


def convert_one(
    input_path: Path,
    output_path: Path,
    skip_associated: bool,
    overwrite: bool,
    tile_size: int = DEFAULT_TILE_SIZE,
    jpeg_quality: int = DEFAULT_JPEG_QUALITY,
) -> None:
    """完成单个 MRXS 文件到 SVS 的转换。"""

    if output_path.exists() and not overwrite:
        print(f"Skip  : {input_path} -> {output_path} (already exists)")
        return

    if tile_size <= 0:
        raise ValueError("--tile-size must be a positive integer")
    jpeg_quality = normalize_jpeg_quality(
        jpeg_quality,
        default=DEFAULT_JPEG_QUALITY,
        field_name="--jpeg-quality",
    )

    data_dir = mrxs_data_directory(input_path)
    if not data_dir.is_dir():
        raise FileNotFoundError(
            f"MRXS data directory missing next to slide file: expected {data_dir}"
        )

    output_path.parent.mkdir(parents=True, exist_ok=True)

    slide = MrxsParser(
        input_path,
        tile_size=tile_size,
        jpeg_quality=jpeg_quality,
    ).parse()
    writer = SvsWriter(slide)

    print(f"Input : {input_path}")
    print(f"Output: {output_path}")
    print_slide_info(slide)

    writer.write(output_path=output_path, skip_associated=skip_associated)
    print("Conversion completed.")


def main() -> None:
    """程序入口：构建任务并批量执行。"""

    options = parse_args()
    jobs = build_single_format_jobs(
        options,
        supported_suffixes=SUPPORTED_SUFFIXES,
        suffix_label=".mrxs",
        output_error_message="--output can only be used with a single .mrxs input file",
        runner_factory=lambda input_path, output_path: lambda: convert_one(
            input_path=input_path,
            output_path=output_path,
            skip_associated=options.skip_associated,
            overwrite=options.overwrite,
            tile_size=options.tile_size,
            jpeg_quality=options.jpeg_quality,
        ),
    )
    run_conversion_jobs(jobs)


if __name__ == "__main__":
    main()
