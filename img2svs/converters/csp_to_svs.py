#!/usr/bin/env python3
from __future__ import annotations

import argparse
import io
import mmap
import os
import re
import struct
from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass
from pathlib import Path
from typing import Iterator, Sequence

import numpy
import tifffile
import imagecodecs
from PIL import Image

from img2svs.core.svs_common import (
    BatchOptions,
    add_batch_arguments,
    add_jpeg_quality_argument,
    aperio_main_description,
    batch_options_from_args,
    blank_rgb_jpeg,
    build_single_format_jobs,
    decode_rgb_image,
    format_associated_summary,
    format_level_summary,
    normalize_jpeg_quality,
    pixels_per_centimeter,
    run_conversion_jobs,
    should_use_bigtiff,
    write_associated_images,
    write_pyramid_level,
    write_thumbnail_page,
)

SUPPORTED_SUFFIXES = {".csp"}
ITEM25_HEADER = bytes.fromhex("020025000F0007000000000000002400000000000000")
STREAM_HEADER_MARKER = b"\xff\xd8\xff\xe0"
DEFAULT_JPEG_QUALITY = 75


@dataclass(frozen=True)
class SlideMetadata:
    """保存主图核心显示元数据。"""

    width: int
    height: int
    mpp: float
    app_mag: float
    jpeg_quality: int


@dataclass(frozen=True)
class ByteRange:
    """表示文件中的一段字节范围。"""

    offset: int
    length: int

    @property
    def present(self) -> bool:
        """判断当前范围是否有效。"""

        return self.offset >= 0 and self.length > 0


@dataclass(frozen=True)
class PyramidLevel:
    """描述一个金字塔层级的尺寸和瓦片布局。"""

    index: int
    width: int
    height: int
    downsample: float
    tile_cols: int
    tile_rows: int

    @property
    def tile_count(self) -> int:
        """返回当前层理论上的瓦片总数。"""

        return self.tile_cols * self.tile_rows


@dataclass(frozen=True)
class TileEntry:
    """描述主图瓦片的位置和字节范围。"""

    x: int
    y: int
    width: int
    height: int
    data_range: ByteRange


@dataclass(frozen=True)
class AssociatedImageEntry:
    """描述关联图在 CSP 中的位置。"""

    kind: str
    width: int
    height: int
    data_range: ByteRange


@dataclass(frozen=True)
class CspSlide:
    """保存 CSP 解析完成后的统一幻灯片对象。"""

    path: Path
    metadata: SlideMetadata
    tile_size: int
    levels: tuple[PyramidLevel, ...]
    level_tiles: tuple[tuple[TileEntry | None, ...], ...]
    associated_images: tuple[AssociatedImageEntry, ...]


def parse_args(argv: Sequence[str] | None = None) -> BatchOptions:
    """解析 CSP 转换脚本的通用命令行参数。"""

    parser = argparse.ArgumentParser(
        description="Convert .csp whole-slide images to Aperio SVS."
    )
    add_batch_arguments(parser, "Path to an input .csp file or directory.")
    add_jpeg_quality_argument(parser)
    return batch_options_from_args(parser.parse_args(argv))


def print_slide_info(slide: CspSlide) -> None:
    """打印 CSP 幻灯片的摘要信息。"""

    thumbnail_text = (
        f"{slide.levels[-1].width}x{slide.levels[-1].height} (from smallest pyramid level)"
    )
    print(
        f"Image : {slide.metadata.width}x{slide.metadata.height}, "
        f"tile={slide.tile_size}x{slide.tile_size}, "
        f"levels={len(slide.levels)}, compression=jpeg"
    )
    print(
        f"Meta  : mpp={slide.metadata.mpp:.6f}, app_mag={slide.metadata.app_mag:g}, "
        f"jpeg_quality={slide.metadata.jpeg_quality}, thumbnail={thumbnail_text}"
    )
    print(f"Pyr   : {format_level_summary(slide.levels)}")
    print(f"Assoc : {format_associated_summary(slide.associated_images)}")


def read_range_from_mmap(mm: mmap.mmap, data_range: ByteRange) -> bytes:
    """从内存映射中读取指定字节范围。"""

    if not data_range.present:
        return b""
    return mm[data_range.offset : data_range.offset + data_range.length]


class CspParser:
    """负责把 CSP 文件解析成统一的幻灯片对象。"""

    def __init__(self, path: Path):
        self.path = path

    def parse(self) -> CspSlide:
        """解析头部、金字塔索引和关联图信息。"""

        with self.path.open("rb") as fh:
            stream_start, header = self._read_header(fh)
            tail_start = self._read_tail_start(header)
            tail = self._read_tail(fh, tail_start)
            app_mag = self._read_float_payload(tail[:860], 4, 9, default=40.0)
            mpp = self._read_float_payload(tail[:860], 4, 10, default=0.25)
            associated = self._read_associated_images(header, stream_start)
            remaining_associated = self._classify_associated_images(associated)
            levels, level_tiles = self._read_pyramid(tail, stream_start)

        if not levels:
            raise ValueError("No CSP pyramid levels were found")

        metadata = SlideMetadata(
            width=levels[0].width,
            height=levels[0].height,
            mpp=mpp,
            app_mag=app_mag,
            jpeg_quality=DEFAULT_JPEG_QUALITY,
        )
        return CspSlide(
            path=self.path,
            metadata=metadata,
            tile_size=256,
            levels=levels,
            level_tiles=level_tiles,
            associated_images=remaining_associated,
        )

    def _read_header(self, fh) -> tuple[int, bytes]:
        """读取文件头并定位 JPEG 数据起始偏移。"""

        header = fh.read(4096)
        if not header.startswith(b"MEDIC"):
            raise ValueError("Unsupported CSP signature")

        stream_start = header.find(STREAM_HEADER_MARKER)
        if stream_start < 0:
            raise ValueError("Could not locate CSP JPEG stream start")
        return stream_start, header[:stream_start]

    def _read_tail_start(self, header: bytes) -> int:
        """读取尾部索引区的绝对偏移。"""

        if len(header) < 0x26:
            raise ValueError("CSP header is too small")
        tail_start = struct.unpack_from("<Q", header, 0x1E)[0]
        if tail_start <= 0 or tail_start >= self.path.stat().st_size:
            raise ValueError("Invalid CSP tail offset")
        return tail_start

    def _read_tail(self, fh, tail_start: int) -> bytes:
        """读取整个尾部索引区。"""

        fh.seek(tail_start)
        return fh.read()

    def _read_float_payload(
        self, data: bytes, group: int, item: int, *, default: float
    ) -> float:
        """读取 tail 前缀里某个 4 字节浮点 payload。"""

        pattern = self._build_pattern(group, item, type_code=9, payload_size=4)
        position = data.find(pattern)
        if position < 0:
            return default
        return struct.unpack_from("<f", data, position + 22)[0]

    def _read_associated_images(
        self, header: bytes, stream_start: int
    ) -> tuple[AssociatedImageEntry, ...]:
        """从头部读取 3 张关联图的偏移和尺寸。"""

        starts = self._find_all(header, bytes.fromhex("020001000E00"))
        descriptors: list[AssociatedImageEntry] = []
        for start in starts:
            width = self._find_scalar_payload(
                header,
                self._build_pattern(2, 3, type_code=5, payload_size=4),
                payload_size=4,
                search_start=start,
                search_end=min(start + 180, len(header)),
            )
            height = self._find_scalar_payload(
                header,
                self._build_pattern(2, 4, type_code=5, payload_size=4),
                payload_size=4,
                search_start=start,
                search_end=min(start + 180, len(header)),
            )
            offset_rel = self._find_scalar_payload(
                header,
                self._build_pattern(2, 5, type_code=7, payload_size=8),
                payload_size=8,
                search_start=start,
                search_end=min(start + 220, len(header)),
            )
            length = self._find_scalar_payload(
                header,
                self._build_pattern(2, 6, type_code=7, payload_size=8),
                payload_size=8,
                search_start=start,
                search_end=min(start + 220, len(header)),
            )
            if width <= 0 or height <= 0 or length <= 0:
                continue
            descriptors.append(
                AssociatedImageEntry(
                    kind="unknown",
                    width=width,
                    height=height,
                    data_range=ByteRange(stream_start + offset_rel, length),
                )
            )
        if len(descriptors) < 3:
            return tuple(descriptors)
        return tuple(descriptors[:3])

    def _classify_associated_images(
        self, entries: tuple[AssociatedImageEntry, ...]
    ) -> tuple[AssociatedImageEntry, ...]:
        """按尺寸启发式保留 label / macro，忽略中间那张 overview 图。"""

        if not entries:
            return ()

        by_area = sorted(entries, key=lambda entry: entry.width * entry.height)
        if len(by_area) == 1:
            entry = by_area[0]
            return (
                AssociatedImageEntry(
                    kind="label",
                    width=entry.width,
                    height=entry.height,
                    data_range=entry.data_range,
                ),
            )

        label = by_area[0]
        macro = by_area[-1]
        associated = [
            AssociatedImageEntry(
                kind="label",
                width=label.width,
                height=label.height,
                data_range=label.data_range,
            )
        ]
        if macro != label:
            associated.append(
                AssociatedImageEntry(
                    kind="macro",
                    width=macro.width,
                    height=macro.height,
                    data_range=macro.data_range,
                )
            )
        return tuple(associated)

    def _read_pyramid(
        self, tail: bytes, stream_start: int
    ) -> tuple[tuple[PyramidLevel, ...], tuple[tuple[TileEntry | None, ...], ...]]:
        """解析 tail 中所有 item=25 记录并按 level 分组。"""

        positions = self._find_all(tail, ITEM25_HEADER)
        if not positions:
            raise ValueError("No CSP tile records were found in tail metadata")

        boundaries = [0]
        for index in range(len(positions) - 1):
            if positions[index + 1] - positions[index] != 58:
                boundaries.append(index + 1)
        boundaries.append(len(positions))

        level_entries: list[tuple[PyramidLevel, tuple[TileEntry | None, ...]]] = []
        full_width = 0
        for block_index in range(len(boundaries) - 1):
            lo = boundaries[block_index]
            hi = boundaries[block_index + 1]
            by_coord: dict[tuple[int, int], TileEntry] = {}
            width = 0
            height = 0
            for record_index in range(lo, hi):
                payload = struct.unpack_from("<9I", tail, positions[record_index] + 22)
                entry = TileEntry(
                    width=payload[0],
                    height=payload[1],
                    x=payload[6],
                    y=payload[7],
                    data_range=ByteRange(stream_start + payload[2], payload[4]),
                )
                by_coord.setdefault((entry.x, entry.y), entry)
                width = max(width, entry.x + entry.width)
                height = max(height, entry.y + entry.height)

            if width <= 0 or height <= 0:
                raise ValueError(f"Invalid CSP level dimensions at block {block_index}")

            tile_cols = (width + 255) // 256
            tile_rows = (height + 255) // 256
            ordered_tiles: list[TileEntry | None] = []
            for row in range(tile_rows):
                for col in range(tile_cols):
                    ordered_tiles.append(by_coord.get((col * 256, row * 256)))

            if block_index == 0:
                full_width = width
            if full_width <= 0:
                raise ValueError("Invalid CSP full-resolution width")

            level = PyramidLevel(
                index=block_index,
                width=width,
                height=height,
                downsample=full_width / width,
                tile_cols=tile_cols,
                tile_rows=tile_rows,
            )
            level_entries.append((level, tuple(ordered_tiles)))

        levels, ordered_tiles = zip(*level_entries, strict=True)
        return tuple(levels), tuple(ordered_tiles)

    def _build_pattern(
        self, group: int, item: int, *, type_code: int, payload_size: int
    ) -> bytes:
        """构造类型固定、payload 为 4/8 字节的元数据头匹配模式。"""

        return (
            struct.pack("<HHHH", group, item, type_code, 1)
            + b"\x00" * 6
            + struct.pack("<I", payload_size)
            + b"\x00" * 4
        )

    def _find_scalar_payload(
        self,
        data: bytes,
        pattern: bytes,
        *,
        payload_size: int,
        search_start: int,
        search_end: int,
    ) -> int:
        """在指定范围内定位 metadata 头并读取后续标量 payload。"""

        position = data.find(pattern, search_start, search_end)
        if position < 0:
            raise ValueError("Required CSP metadata entry was not found")
        if payload_size == 4:
            return struct.unpack_from("<I", data, position + 22)[0]
        return struct.unpack_from("<Q", data, position + 22)[0]

    def _find_all(self, data: bytes, needle: bytes) -> list[int]:
        """返回某个字节模式在数据中的所有位置。"""

        return [match.start() for match in re.finditer(re.escape(needle), data)]


class SvsWriter:
    """负责把解析后的 CSP 内容写成 SVS。"""

    def __init__(self, slide: CspSlide, *, source_jpeg_quality: int = DEFAULT_JPEG_QUALITY):
        self.slide = slide
        self.source_jpeg_quality = source_jpeg_quality
        # 部分 Windows 病理查看器无法读取 CSP 中直接复用的 JPEG 瓦片，
        # 且直接复用会让 case3 这类文件超过 2 GB。大 CSP 统一写成标准
        # JPEG 瓦片，通常可把输出控制在兼容范围内。
        self.reencode_jpeg_tiles = (
            self.source_jpeg_quality != slide.metadata.jpeg_quality
            or slide.path.stat().st_size >= 2_000_000_000
        )
        self.resolution = pixels_per_centimeter(slide.metadata.mpp)
        self.blank_tile = blank_rgb_jpeg(
            slide.tile_size,
            slide.tile_size,
            self.source_jpeg_quality,
        )
        self.blank_array_tile = numpy.full(
            (slide.tile_size, slide.tile_size, 3),
            255,
            dtype=numpy.uint8,
        )

    def write(self, output_path: Path, skip_associated: bool) -> None:
        """写出主图、缩略图、金字塔层和关联图。"""

        with self.slide.path.open("rb") as fh, mmap.mmap(
            fh.fileno(), 0, access=mmap.ACCESS_READ
        ) as mm:
            associated = {} if skip_associated else self._load_associated_images(mm)

            with tifffile.TiffWriter(
                output_path, bigtiff=should_use_bigtiff(self.slide.path)
            ) as tif:
                self._write_level(tif, mm, self.slide.levels[0], reduced=False)
                self._write_thumbnail(tif, mm)
                for level in self.slide.levels[1:]:
                    self._write_level(tif, mm, level, reduced=True)
                write_associated_images(
                    tif, associated, jpeg_quality=self.slide.metadata.jpeg_quality
                )

    def _write_level(
        self, tif: tifffile.TiffWriter, mm: mmap.mmap, level: PyramidLevel, *, reduced: bool
    ) -> None:
        """写出一个瓦片化主图或降采样层。"""

        if self.reencode_jpeg_tiles:
            # 预先并行编码为 JPEG 字节，避免 tifffile 在主线程逐块编码。
            data = self._standard_jpeg_tile_iterator(mm, level)
        else:
            data = self._tiff_jpeg_tile_iterator(mm, level)
        write_pyramid_level(
            tif,
            data=data,
            width=level.width,
            height=level.height,
            tile_size=self.slide.tile_size,
            jpeg_quality=self.slide.metadata.jpeg_quality,
            resolution=self.resolution / level.downsample if reduced else self.resolution,
            reduced=reduced,
            description=aperio_main_description(
                self.slide.metadata, self.slide.tile_size, level
            ),
            software="csp_to_svs.py",
            compressionargs=False,
        )

    def _write_thumbnail(self, tif: tifffile.TiffWriter, mm: mmap.mmap) -> None:
        """写出缩略图页面，优先直通最小层的原始 JPEG。"""

        level = self.slide.levels[-1]
        entries = self.slide.level_tiles[level.index]
        if not self.reencode_jpeg_tiles and len(entries) == 1 and entries[0] is not None:
            data = read_range_from_mmap(mm, entries[0].data_range)
            if data:
                tif.write(
                    data=iter([data]),
                    shape=(level.height, level.width, 3),
                    dtype=numpy.uint8,
                    photometric="rgb",
                    compression="jpeg",
                    rowsperstrip=level.height,
                    resolution=(self.resolution, self.resolution),
                    resolutionunit="CENTIMETER",
                    metadata=None,
                    software=False,
                )
                return

        thumbnail = self._build_thumbnail(mm)
        write_thumbnail_page(
            tif,
            thumbnail,
            resolution=self.resolution,
            jpeg_quality=self.slide.metadata.jpeg_quality,
        )

    def _build_thumbnail(self, mm: mmap.mmap) -> numpy.ndarray:
        """使用最小金字塔层构造 SVS thumbnail，避免写入 CSP 自带 overview 图。"""

        return self._render_level_image(mm, self.slide.levels[-1])

    def _render_level_image(
        self, mm: mmap.mmap, level: PyramidLevel
    ) -> numpy.ndarray:
        """把指定层完整渲染为 RGB 图像。"""

        entries = self.slide.level_tiles[level.index]
        if len(entries) == 1 and entries[0] is not None:
            data = read_range_from_mmap(mm, entries[0].data_range)
            if data:
                image = Image.open(io.BytesIO(data)).convert("RGB")
                return numpy.asarray(image)

        canvas = numpy.full((level.height, level.width, 3), 255, dtype=numpy.uint8)
        has_tile = False
        for entry in entries:
            if entry is None:
                continue
            data = read_range_from_mmap(mm, entry.data_range)
            if not data:
                continue
            tile = decode_rgb_image(data)
            tile_h = min(tile.shape[0], level.height - entry.y)
            tile_w = min(tile.shape[1], level.width - entry.x)
            canvas[entry.y : entry.y + tile_h, entry.x : entry.x + tile_w] = tile[
                :tile_h, :tile_w
            ]
            has_tile = True
        if has_tile:
            return canvas
        raise ValueError("CSP thumbnail could not be constructed from smallest pyramid level")

    def _tiff_jpeg_tile_iterator(self, mm: mmap.mmap, level: PyramidLevel) -> Iterator[bytes]:
        """按 row-major 产出可安全写入 TIFF tile 的 JPEG 字节。"""

        for entry in self.slide.level_tiles[level.index]:
            if entry is None:
                yield self.blank_tile
                continue
            data = read_range_from_mmap(mm, entry.data_range)
            if not data:
                yield self.blank_tile
                continue
            if entry.width == self.slide.tile_size and entry.height == self.slide.tile_size:
                yield data
                continue
            yield self._encode_padded_tile(decode_rgb_image(data))

    def _decoded_tile_iterator(
        self, mm: mmap.mmap, level: PyramidLevel
    ) -> Iterator[numpy.ndarray]:
        """按 row-major 顺序迭代解码后的 RGB 瓦片。"""

        for entry in self.slide.level_tiles[level.index]:
            if entry is None:
                yield self.blank_array_tile
                continue
            data = read_range_from_mmap(mm, entry.data_range)
            if not data:
                yield self.blank_array_tile
                continue
            yield decode_rgb_image(data)

    def _standard_jpeg_tile_iterator(
        self, mm: mmap.mmap, level: PyramidLevel
    ) -> Iterator[bytes]:
        """并行把 CSP 瓦片编码为查看器兼容的标准 JPEG。"""

        worker_count = min(8, max(2, os.cpu_count() or 1))
        pending = []
        with ThreadPoolExecutor(max_workers=worker_count) as executor:
            for entry in self.slide.level_tiles[level.index]:
                if entry is None:
                    pending.append(self.blank_tile)
                else:
                    data = read_range_from_mmap(mm, entry.data_range)
                    pending.append(
                        executor.submit(self._encode_standard_tile, data)
                        if data
                        else self.blank_tile
                    )

                if len(pending) >= worker_count * 2:
                    item = pending.pop(0)
                    yield item.result() if hasattr(item, "result") else item

            while pending:
                item = pending.pop(0)
                yield item.result() if hasattr(item, "result") else item

    def _encode_standard_tile(self, data: bytes) -> bytes:
        """解码并快速重新编码一个完整的标准 JPEG 瓦片。"""

        tile = imagecodecs.jpeg_decode(data)
        if tile.shape[:2] != (self.slide.tile_size, self.slide.tile_size):
            padded = self.blank_array_tile.copy()
            tile_h = min(tile.shape[0], self.slide.tile_size)
            tile_w = min(tile.shape[1], self.slide.tile_size)
            padded[:tile_h, :tile_w] = tile[:tile_h, :tile_w]
            tile = padded
        return imagecodecs.jpeg_encode(tile, level=self.slide.metadata.jpeg_quality)

    def _encode_padded_tile(self, tile: numpy.ndarray) -> bytes:
        """把边缘小瓦片补成完整 tile 后重新编码为 JPEG。"""

        padded = self.blank_array_tile.copy()
        tile_h = min(tile.shape[0], self.slide.tile_size)
        tile_w = min(tile.shape[1], self.slide.tile_size)
        padded[:tile_h, :tile_w] = tile[:tile_h, :tile_w]
        image = Image.fromarray(padded, mode="RGB")
        return imagecodecs.jpeg_encode(
            numpy.asarray(image), level=self.slide.metadata.jpeg_quality
        )

    def _load_associated_images(self, mm: mmap.mmap) -> dict[str, numpy.ndarray]:
        """读取所有关联图并解码为 RGB 数组。"""

        images: dict[str, numpy.ndarray] = {}
        for entry in self.slide.associated_images:
            data = read_range_from_mmap(mm, entry.data_range)
            if data:
                images[entry.kind] = decode_rgb_image(data)
        return images


def convert_one(
    input_path: Path,
    output_path: Path,
    *,
    jpeg_quality: int | None,
    skip_associated: bool,
    overwrite: bool,
) -> None:
    """把单个 CSP 文件转换为 SVS。"""

    if output_path.exists() and not overwrite:
        print(f"Skip  : {input_path} -> {output_path} (already exists)")
        return

    slide = CspParser(input_path).parse()
    slide = CspSlide(
        path=slide.path,
        metadata=SlideMetadata(
            width=slide.metadata.width,
            height=slide.metadata.height,
            mpp=slide.metadata.mpp,
            app_mag=slide.metadata.app_mag,
            jpeg_quality=normalize_jpeg_quality(
                jpeg_quality,
                default=slide.metadata.jpeg_quality,
                field_name="--jpeg-quality",
            ),
        ),
        tile_size=slide.tile_size,
        levels=slide.levels,
        level_tiles=slide.level_tiles,
        associated_images=slide.associated_images,
    )
    print_slide_info(slide)
    output_path.parent.mkdir(parents=True, exist_ok=True)
    writer = SvsWriter(slide, source_jpeg_quality=DEFAULT_JPEG_QUALITY)
    writer.write(output_path=output_path, skip_associated=skip_associated)
    print(f"Wrote : {output_path}")


def build_jobs(options: BatchOptions) -> list:
    """为 CSP 构建批量转换任务。"""

    return build_single_format_jobs(
        options,
        supported_suffixes=SUPPORTED_SUFFIXES,
        suffix_label=".csp slide",
        output_error_message="--output can only be used with a single .csp input",
        runner_factory=lambda slide, output_path: lambda: convert_one(
            input_path=slide,
            output_path=output_path,
            jpeg_quality=options.jpeg_quality,
            skip_associated=options.skip_associated,
            overwrite=options.overwrite,
        ),
    )


def main() -> None:
    """程序入口。"""

    run_conversion_jobs(build_jobs(parse_args()))


if __name__ == "__main__":
    main()
