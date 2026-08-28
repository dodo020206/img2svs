#!/usr/bin/env python3
from __future__ import annotations

import argparse
import math
import mmap
import struct
from dataclasses import dataclass, replace
from functools import lru_cache
from pathlib import Path
from typing import Iterator, Sequence

import numpy
import tifffile
from PIL import Image, UnidentifiedImageError

from img2svs.core.svs_common import (
    BatchOptions,
    add_jpeg_quality_argument,
    add_batch_arguments,
    aperio_main_description,
    batch_options_from_args,
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

SUPPORTED_SUFFIXES = {".kfb"}
KFB_VERSION_PREFIX = b"KFB"
KFB_LEVEL_ID_STEP = 8_388_608
KFB_ASSOCIATED_INFO_SIZE = 52


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
    downsample: int
    tile_cols: int
    tile_rows: int

    @property
    def tile_count(self) -> int:
        """返回当前层理论上的瓦片总数。"""

        return self.tile_cols * self.tile_rows


@dataclass(frozen=True)
class ByteRange:
    """表示文件中的一段字节范围。"""

    offset: int
    length: int

    @property
    def present(self) -> bool:
        """判断这段字节范围是否有效。"""

        return self.offset >= 0 and self.length > 0


@dataclass(frozen=True)
class AssociatedImageEntry:
    """描述关联图在 KFB 容器中的位置。"""

    kind: str
    width: int
    height: int
    data_range: ByteRange


@dataclass(frozen=True)
class ThumbnailEntry:
    """描述预览图在 KFB 容器中的位置。"""

    width: int
    height: int
    data_range: ByteRange


@dataclass(frozen=True)
class KfbHeader:
    """映射 KFB 文件头中转换所需的关键字段。"""

    tile_count: int
    base_width: int
    base_height: int
    zoom_levels: int
    scan_scale: float
    image_cap_res: float
    tile_size: int
    spend_time: int
    scan_time: int
    macro_info_offset: int
    label_info_offset: int
    preview_info_offset: int
    tiles_info_offset: int


@dataclass(frozen=True)
class KfbTileEntry:
    """描述单个 JPEG 瓦片在某一层中的位置和字节范围。"""

    image_index: int
    level_index: int
    x: int
    y: int
    width: int
    height: int
    data_range: ByteRange


@dataclass(frozen=True)
class KfbSlide:
    """保存 KFB 解析完成后的统一幻灯片对象。"""

    path: Path
    metadata: SlideMetadata
    tile_size: int
    levels: tuple[PyramidLevel, ...]
    level_tiles: tuple[tuple[KfbTileEntry, ...], ...]
    associated_images: tuple[AssociatedImageEntry, ...]
    thumbnail: ThumbnailEntry


class BinaryReader:
    """提供基于小端序的便捷二进制读取方法。"""

    def __init__(self, fh):
        """绑定底层文件句柄。"""

        self.fh = fh

    def seek(self, offset: int, whence: int = 0) -> int:
        """移动读取位置。"""

        return self.fh.seek(offset, whence)

    def skip(self, count: int) -> int:
        """从当前位置跳过指定字节数。"""

        return self.seek(count, 1)

    def read_exact(self, count: int) -> bytes:
        """严格读取指定字节数，不足时抛出异常。"""

        data = self.fh.read(count)
        if len(data) != count:
            raise ValueError("Unexpected end of file while reading KFB")
        return data

    def bytes(self, count: int) -> bytes:
        """读取原始字节块。"""

        return self.read_exact(count)

    def i32(self) -> int:
        """读取有符号 32 位整数。"""

        return struct.unpack("<i", self.read_exact(4))[0]

    def u32(self) -> int:
        """读取无符号 32 位整数。"""

        return struct.unpack("<I", self.read_exact(4))[0]

    def i64(self) -> int:
        """读取有符号 64 位整数。"""

        return struct.unpack("<q", self.read_exact(8))[0]

    def u64(self) -> int:
        """读取无符号 64 位整数。"""

        return struct.unpack("<Q", self.read_exact(8))[0]

    def f32(self) -> float:
        """读取 32 位浮点数。"""

        return struct.unpack("<f", self.read_exact(4))[0]


def parse_args(argv: Sequence[str] | None = None) -> BatchOptions:
    """解析 KFB 转换脚本的通用命令行参数。"""

    parser = argparse.ArgumentParser(
        description=(
            "Convert KFBio .kfb whole-slide images to Aperio SVS. "
            "Format details follow openslide/openslide PR #669."
        )
    )
    add_batch_arguments(parser, "Path to an input .kfb file or directory.")
    add_jpeg_quality_argument(parser)
    return batch_options_from_args(parser.parse_args(argv))


def print_slide_info(slide: KfbSlide) -> None:
    """打印 KFB 幻灯片的摘要信息。"""

    print(
        f"Image : {slide.metadata.width}x{slide.metadata.height}, "
        f"tile={slide.tile_size}x{slide.tile_size}, "
        f"levels={len(slide.levels)}, compression=jpeg"
    )
    print(
        f"Meta  : mpp={slide.metadata.mpp:.6f}, app_mag={slide.metadata.app_mag:g}, "
        f"jpeg_quality={slide.metadata.jpeg_quality}, "
        f"thumbnail={slide.thumbnail.width}x{slide.thumbnail.height}"
    )
    print(f"Pyr   : {format_level_summary(slide.levels)}")
    print(f"Assoc : {format_associated_summary(slide.associated_images)}")


def read_range_from_mmap(mm: mmap.mmap, data_range: ByteRange) -> bytes:
    """从内存映射中读取指定字节范围。"""

    if not data_range.present:
        return b""
    return mm[data_range.offset : data_range.offset + data_range.length]


@lru_cache(maxsize=None)
def blank_tile_array(tile_size: int) -> numpy.ndarray:
    """生成纯白 RGB 占位瓦片数组。"""

    return numpy.full((tile_size, tile_size, 3), 255, dtype=numpy.uint8)


class KfbParser:
    """负责把 KFB 文件解析成统一的幻灯片对象。"""

    def __init__(self, path: Path):
        """记录待解析的 KFB 文件路径。"""

        self.path = path

    def parse(self) -> KfbSlide:
        """顺序解析头部、关联图、预览图和所有金字塔瓦片。"""

        with self.path.open("rb") as fh:
            reader = BinaryReader(fh)
            header = self._read_header(reader)
            associated_images = tuple(
                entry
                for entry in (
                    self._read_associated_entry(reader, header.macro_info_offset, "macro"),
                    self._read_associated_entry(reader, header.label_info_offset, "label"),
                )
                if entry.data_range.present
            )
            thumbnail = self._read_thumbnail_entry(reader, header.preview_info_offset)
            levels = self._build_levels(header)
            level_tiles = self._read_tile_entries(reader, header, levels)

        metadata = SlideMetadata(
            width=header.base_width,
            height=header.base_height,
            mpp=header.image_cap_res,
            app_mag=header.scan_scale,
            jpeg_quality=75,
        )
        return KfbSlide(
            path=self.path,
            metadata=metadata,
            tile_size=header.tile_size,
            levels=levels,
            level_tiles=level_tiles,
            associated_images=associated_images,
            thumbnail=thumbnail,
        )

    def _read_header(self, reader: BinaryReader) -> KfbHeader:
        """读取 KFB 文件头中的关键字段。"""

        reader.seek(4)
        version = reader.bytes(4)
        if not version.startswith(KFB_VERSION_PREFIX):
            raise ValueError(f"Unsupported KFB signature: {version!r}")

        reader.skip(8)
        tile_count = reader.i32()
        base_height = reader.i32()
        base_width = reader.i32()
        scan_scale = float(reader.i32())
        compression = reader.bytes(4)
        if not compression.startswith(b"JP"):
            raise ValueError(f"Unsupported KFB compression: {compression!r}")

        reader.skip(4)
        spend_time = reader.i32()
        scan_time = reader.i64()
        macro_info_offset = reader.u32()
        label_info_offset = reader.u32()
        preview_info_offset = reader.u64()
        tiles_info_offset = reader.u64()
        image_cap_res = reader.f32()
        reader.skip(8)
        tile_size = reader.i32()

        if tile_count < 0:
            raise ValueError("Invalid KFB tile count")
        if base_width <= 0 or base_height <= 0:
            raise ValueError("Invalid KFB base dimensions")
        if tile_size <= 0:
            raise ValueError("Invalid KFB tile size")
        if image_cap_res <= 0:
            raise ValueError("Invalid KFB image resolution")

        zoom_levels = int(math.ceil(math.log2(max(base_width, base_height)))) + 1
        return KfbHeader(
            tile_count=tile_count,
            base_width=base_width,
            base_height=base_height,
            zoom_levels=zoom_levels,
            scan_scale=scan_scale,
            image_cap_res=image_cap_res,
            tile_size=tile_size,
            spend_time=spend_time,
            scan_time=scan_time,
            macro_info_offset=macro_info_offset,
            label_info_offset=label_info_offset,
            preview_info_offset=preview_info_offset,
            tiles_info_offset=tiles_info_offset,
        )

    def _read_associated_entry(
        self, reader: BinaryReader, offset: int, kind: str
    ) -> AssociatedImageEntry:
        """读取宏观图或标签图信息块。"""

        if offset <= 0:
            return AssociatedImageEntry(kind=kind, width=0, height=0, data_range=ByteRange(-1, 0))

        width, height, data_range = self._read_embedded_jpeg_entry(reader, offset)
        return AssociatedImageEntry(kind=kind, width=width, height=height, data_range=data_range)

    def _read_thumbnail_entry(self, reader: BinaryReader, offset: int) -> ThumbnailEntry:
        """读取预览图信息块。"""

        if offset <= 0:
            return ThumbnailEntry(width=0, height=0, data_range=ByteRange(-1, 0))

        width, height, data_range = self._read_embedded_jpeg_entry(reader, offset)
        return ThumbnailEntry(width=width, height=height, data_range=data_range)

    def _read_embedded_jpeg_entry(
        self, reader: BinaryReader, offset: int
    ) -> tuple[int, int, ByteRange]:
        """读取关联图或预览图通用的 JPEG 信息块。"""

        reader.seek(offset)
        reader.skip(8)
        height = reader.i32()
        width = reader.i32()
        reader.skip(4)
        length = reader.i32()
        reader.skip(28)
        if width <= 0 or height <= 0 or length <= 0:
            raise ValueError(f"Invalid KFB embedded image entry at offset {offset}")
        return width, height, ByteRange(offset + KFB_ASSOCIATED_INFO_SIZE, length)

    def _build_levels(self, header: KfbHeader) -> tuple[PyramidLevel, ...]:
        """根据基准尺寸推导所有 2 倍降采样层。"""

        levels: list[PyramidLevel] = []
        for level_index in range(header.zoom_levels):
            downsample = 1 << level_index
            width = max(header.base_width // downsample, 1)
            height = max(header.base_height // downsample, 1)
            levels.append(
                PyramidLevel(
                    index=level_index,
                    width=width,
                    height=height,
                    downsample=downsample,
                    tile_cols=(width + header.tile_size - 1) // header.tile_size,
                    tile_rows=(height + header.tile_size - 1) // header.tile_size,
                )
            )
        return tuple(levels)

    def _read_tile_entries(
        self,
        reader: BinaryReader,
        header: KfbHeader,
        levels: tuple[PyramidLevel, ...],
    ) -> tuple[tuple[KfbTileEntry, ...], ...]:
        """读取瓦片位置表，并按层级组织所有 JPEG 图块。"""

        reader.seek(header.tiles_info_offset)
        level_tiles: list[list[KfbTileEntry]] = [[] for _ in levels]
        base_level_id: int | None = None

        for image_index in range(header.tile_count):
            reader.skip(4)
            pos_x = reader.i32()
            pos_y = reader.i32()
            tile_width = reader.i32()
            tile_height = reader.i32()
            tile_id = reader.i32()

            if base_level_id is None:
                base_level_id = tile_id
            level_delta = base_level_id - tile_id
            if level_delta % KFB_LEVEL_ID_STEP != 0:
                raise ValueError(f"Invalid KFB level id mapping: {tile_id}")
            level_index = level_delta // KFB_LEVEL_ID_STEP
            if level_index < 0 or level_index >= len(levels):
                raise ValueError(f"Invalid KFB level index: {level_index}")

            reader.skip(8)
            length = reader.i32()
            offset_from_tiles_info = reader.i64()
            reader.skip(20)

            if tile_width <= 0 or tile_height <= 0 or length < 0:
                raise ValueError("Invalid KFB tile entry")

            # KFB 把 JPEG 数据偏移存成相对 tiles_info 区块起点的位移。
            data_offset = header.tiles_info_offset + offset_from_tiles_info
            level_tiles[level_index].append(
                KfbTileEntry(
                    image_index=image_index,
                    level_index=level_index,
                    x=pos_x,
                    y=pos_y,
                    width=tile_width,
                    height=tile_height,
                    data_range=ByteRange(data_offset, length),
                )
            )

        return tuple(tuple(entries) for entries in level_tiles)


class SvsWriter:
    """负责把解析后的 KFB 内容写成 SVS。"""

    def __init__(self, slide: KfbSlide):
        """准备写出阶段需要的分辨率、空白瓦片和格网映射。"""

        self.slide = slide
        self.resolution = pixels_per_centimeter(slide.metadata.mpp)
        self.blank_tile = blank_tile_array(slide.tile_size)
        self.level_cells = tuple(self._build_level_cells(level) for level in slide.levels)

    def write(self, output_path: Path, skip_associated: bool) -> None:
        """写出主图、缩略图、金字塔层和关联图。"""

        with self.slide.path.open("rb") as fh, mmap.mmap(
            fh.fileno(), 0, access=mmap.ACCESS_READ
        ) as mm:
            thumbnail = self._decode_thumbnail(mm)
            associated_images = {} if skip_associated else self._load_associated_images(mm)

            with tifffile.TiffWriter(
                output_path, bigtiff=should_use_bigtiff(self.slide.path)
            ) as tif:
                self._write_tiled_level(tif, mm, self.slide.levels[0], reduced=False)
                self._write_thumbnail(tif, thumbnail)
                for level in self.slide.levels[1:]:
                    self._write_tiled_level(tif, mm, level, reduced=True)
                write_associated_images(
                    tif, associated_images, jpeg_quality=self.slide.metadata.jpeg_quality
                )

    def _build_level_cells(
        self, level: PyramidLevel
    ) -> dict[tuple[int, int], tuple[KfbTileEntry, ...]]:
        """把源瓦片映射到输出格网单元，便于后续逐 tile 合成。"""

        cells: dict[tuple[int, int], list[KfbTileEntry]] = {}
        for entry in self.slide.level_tiles[level.index]:
            left = max(entry.x // self.slide.tile_size, 0)
            right = min((entry.x + entry.width - 1) // self.slide.tile_size, level.tile_cols - 1)
            top = max(entry.y // self.slide.tile_size, 0)
            bottom = min(
                (entry.y + entry.height - 1) // self.slide.tile_size,
                level.tile_rows - 1,
            )
            for row in range(top, bottom + 1):
                for col in range(left, right + 1):
                    cells.setdefault((col, row), []).append(entry)
        return {key: tuple(value) for key, value in cells.items()}

    def _write_tiled_level(
        self, tif: tifffile.TiffWriter, mm: mmap.mmap, level: PyramidLevel, *, reduced: bool
    ) -> None:
        """写出一个瓦片化的主图或降采样层。"""

        write_pyramid_level(
            tif,
            data=self._tile_iterator(mm, level),
            width=level.width,
            height=level.height,
            tile_size=self.slide.tile_size,
            jpeg_quality=self.slide.metadata.jpeg_quality,
            resolution=self.resolution / level.downsample if reduced else self.resolution,
            reduced=reduced,
            description=aperio_main_description(
                self.slide.metadata, self.slide.tile_size, level
            ),
            software="kfb_to_svs.py",
        )

    def _write_thumbnail(self, tif: tifffile.TiffWriter, thumbnail: numpy.ndarray) -> None:
        """写出缩略图页面。"""

        write_thumbnail_page(
            tif,
            thumbnail,
            resolution=self.resolution,
            jpeg_quality=self.slide.metadata.jpeg_quality,
        )

    def _tile_iterator(self, mm: mmap.mmap, level: PyramidLevel) -> Iterator[numpy.ndarray]:
        """按输出格网顺序迭代合成后的 RGB 瓦片。"""

        for row in range(level.tile_rows):
            for col in range(level.tile_cols):
                yield self._render_output_tile(mm, level, col, row)

    def _render_output_tile(
        self, mm: mmap.mmap, level: PyramidLevel, col: int, row: int
    ) -> numpy.ndarray:
        """把落在某个输出格网单元上的源瓦片合成为一个规则 tile。"""

        entries = self.level_cells[level.index].get((col, row))
        if not entries:
            return self.blank_tile

        tile_origin_x = col * self.slide.tile_size
        tile_origin_y = row * self.slide.tile_size
        tile_limit_x = tile_origin_x + self.slide.tile_size
        tile_limit_y = tile_origin_y + self.slide.tile_size
        canvas = self.blank_tile.copy()

        for entry in entries:
            tile_bytes = read_range_from_mmap(mm, entry.data_range)
            if not tile_bytes:
                continue

            image = decode_rgb_image(tile_bytes)
            source_width = min(entry.width, image.shape[1])
            source_height = min(entry.height, image.shape[0])

            overlap_left = max(tile_origin_x, entry.x)
            overlap_top = max(tile_origin_y, entry.y)
            overlap_right = min(tile_limit_x, entry.x + source_width)
            overlap_bottom = min(tile_limit_y, entry.y + source_height)
            if overlap_left >= overlap_right or overlap_top >= overlap_bottom:
                continue

            # 源瓦片可能不是严格按规则网格对齐，这里按交集区域进行拷贝。
            dest_left = overlap_left - tile_origin_x
            dest_top = overlap_top - tile_origin_y
            src_left = overlap_left - entry.x
            src_top = overlap_top - entry.y
            copy_width = overlap_right - overlap_left
            copy_height = overlap_bottom - overlap_top
            canvas[
                dest_top : dest_top + copy_height,
                dest_left : dest_left + copy_width,
            ] = image[
                src_top : src_top + copy_height,
                src_left : src_left + copy_width,
            ]

        return canvas

    def _load_associated_images(self, mm: mmap.mmap) -> dict[str, numpy.ndarray]:
        """读取所有关联图并解码成 RGB 数组。"""

        images: dict[str, numpy.ndarray] = {}
        for entry in self.slide.associated_images:
            data = read_range_from_mmap(mm, entry.data_range)
            if data:
                images[entry.kind] = decode_rgb_image(data)
        return images

    def _decode_thumbnail(self, mm: mmap.mmap) -> numpy.ndarray:
        """优先直接解码预览图，失败时回退到低分辨率层重建。"""

        data = read_range_from_mmap(mm, self.slide.thumbnail.data_range)
        if data:
            try:
                return decode_rgb_image(data)
            except UnidentifiedImageError:
                pass
        return self._render_fallback_thumbnail(mm)

    def _render_fallback_thumbnail(self, mm: mmap.mmap, max_size: int = 1024) -> numpy.ndarray:
        """当预览图不可读时，从较低分辨率层重建一张缩略图。"""

        source_level = self.slide.levels[-1]
        for level in self.slide.levels[1:]:
            if max(level.width, level.height) <= max_size * 2:
                source_level = level
                break

        image = self._render_level_image(mm, source_level)
        thumbnail = Image.fromarray(image)
        thumbnail.thumbnail((max_size, max_size), Image.Resampling.LANCZOS)
        return numpy.asarray(thumbnail)

    def _render_level_image(self, mm: mmap.mmap, level: PyramidLevel) -> numpy.ndarray:
        """把某一层的全部瓦片拼接为完整 RGB 图像。"""

        canvas = numpy.full((level.height, level.width, 3), 255, dtype=numpy.uint8)
        for entry in self.slide.level_tiles[level.index]:
            tile_bytes = read_range_from_mmap(mm, entry.data_range)
            if not tile_bytes:
                continue

            image = decode_rgb_image(tile_bytes)
            copy_width = min(entry.width, image.shape[1], max(level.width - entry.x, 0))
            copy_height = min(entry.height, image.shape[0], max(level.height - entry.y, 0))
            if copy_width <= 0 or copy_height <= 0:
                continue

            canvas[
                entry.y : entry.y + copy_height,
                entry.x : entry.x + copy_width,
            ] = image[:copy_height, :copy_width]

        return canvas


def convert_one(
    input_path: Path,
    output_path: Path,
    jpeg_quality: int | None,
    skip_associated: bool,
    overwrite: bool,
) -> None:
    """完成单个 KFB 文件到 SVS 的转换。"""

    if output_path.exists() and not overwrite:
        print(f"Skip  : {input_path} -> {output_path} (already exists)")
        return

    output_path.parent.mkdir(parents=True, exist_ok=True)

    slide = KfbParser(input_path).parse()
    slide = replace(
        slide,
        metadata=replace(
            slide.metadata,
            jpeg_quality=normalize_jpeg_quality(
                jpeg_quality,
                default=slide.metadata.jpeg_quality,
            ),
        ),
    )
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
        suffix_label=".kfb",
        output_error_message="--output can only be used with a single .kfb input file",
        runner_factory=lambda input_path, output_path: lambda: convert_one(
            input_path=input_path,
            output_path=output_path,
            jpeg_quality=options.jpeg_quality,
            skip_associated=options.skip_associated,
            overwrite=options.overwrite,
        ),
    )
    run_conversion_jobs(jobs)


if __name__ == "__main__":
    main()
