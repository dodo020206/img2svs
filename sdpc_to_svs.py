#!/usr/bin/env python3
from __future__ import annotations

import argparse
import math
import mmap
import os
import struct
import tempfile
import threading
from collections import deque
from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass, replace
from functools import lru_cache
from pathlib import Path
from typing import Iterator, Sequence

import numpy
import tifffile
from PIL import Image, UnidentifiedImageError

from svs_common import (
    APERIO_VERSION,
    BatchOptions,
    add_jpeg_quality_argument,
    add_batch_arguments,
    aperio_associated_description,
    batch_options_from_args,
    blank_rgb_jpeg,
    build_single_format_jobs,
    decode_rgb_image,
    format_associated_summary,
    format_level_summary,
    jpeg_compressionargs,
    normalize_jpeg_quality,
    pixels_per_centimeter,
    run_conversion_jobs,
    should_use_bigtiff,
)

PIC_HEAD_FLAG = 0x5153
PERSON_INFO_FLAG = 0x4950
MACROGRAPH_INFO_FLAG = 0x494D
PIC_INFO_FLAG = 0x4649

COMPRESS_JPEG = 0
COMPRESS_HEVC = 4
SUPPORTED_SUFFIXES = {".sdpc", ".dyqx"}
MAX_DECODE_WORKERS = 4
DECODE_PREFETCH_FACTOR = 2


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
class AssociatedImageEntry:
    """描述关联图在源文件中的位置。"""

    kind: str
    data_range: ByteRange


@dataclass(frozen=True)
class ThumbnailEntry:
    """描述缩略图的尺寸和字节位置。"""

    width: int
    height: int
    data_range: ByteRange


@dataclass(frozen=True)
class PicHead:
    """映射 SqPicHead 结构中的关键字段。"""

    head_size: int
    macrograph_count: int
    hierarchy: int
    src_width: int
    src_height: int
    tile_width: int
    tile_height: int
    thumbnail_width: int
    thumbnail_height: int
    jpeg_quality: int
    mpp: float
    app_mag: float
    extra_offset: int
    tile_offset: int
    slice_fmt: int


@dataclass(frozen=True)
class MacrographInfo:
    """映射宏观图信息块。"""

    width: int
    height: int
    encoded_size: int
    next_layer_offset: int
    data_range: ByteRange


@dataclass(frozen=True)
class PicInfo:
    """映射缩略图或金字塔层共用的 SqPicInfo 结构。"""

    info_size: int
    layer: int
    slice_num: int
    slice_num_x: int
    slice_num_y: int
    layer_size: int
    next_layer_offset: int
    cur_scale: float
    ruler: float
    default_x: int
    default_y: int


@dataclass(frozen=True)
class SdpcSlide:
    """保存 SDPC 解析完成后的统一幻灯片对象。"""

    path: Path
    metadata: SlideMetadata
    tile_width: int
    tile_height: int
    source_compression: str
    levels: tuple[PyramidLevel, ...]
    level_tiles: tuple[tuple[ByteRange, ...], ...]
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

    def tell(self) -> int:
        """返回当前读取位置。"""

        return self.fh.tell()

    def skip(self, count: int) -> int:
        """从当前位置跳过指定字节数。"""

        return self.seek(count, 1)

    def read_exact(self, count: int) -> bytes:
        """严格读取指定字节数，不足时抛出异常。"""

        data = self.fh.read(count)
        if len(data) != count:
            raise ValueError("Unexpected end of file while reading SDPC")
        return data

    def u8(self) -> int:
        """读取无符号 8 位整数。"""

        return self.read_exact(1)[0]

    def u16(self) -> int:
        """读取无符号 16 位整数。"""

        return struct.unpack("<H", self.read_exact(2))[0]

    def u32(self) -> int:
        """读取无符号 32 位整数。"""

        return struct.unpack("<I", self.read_exact(4))[0]

    def i32(self) -> int:
        """读取有符号 32 位整数。"""

        return struct.unpack("<i", self.read_exact(4))[0]

    def u64(self) -> int:
        """读取无符号 64 位整数。"""

        return struct.unpack("<Q", self.read_exact(8))[0]

    def f32(self) -> float:
        """读取 32 位浮点数。"""

        return struct.unpack("<f", self.read_exact(4))[0]

    def f64(self) -> float:
        """读取 64 位浮点数。"""

        return struct.unpack("<d", self.read_exact(8))[0]

    def bytes(self, count: int) -> bytes:
        """读取原始字节块。"""

        return self.read_exact(count)


class HevcTileDecoder:
    """使用 PyAV 解码 HEVC 压缩瓦片。"""

    def __init__(self):
        """延迟导入 av，避免 JPEG 场景强依赖该包。"""

        try:
            import av
        except ModuleNotFoundError as exc:
            raise RuntimeError(
                "HEVC-compressed SDPC requires the optional 'av' package. "
                "Install it with 'pip install av' and rerun."
            ) from exc
        self._av = av
        self._codec = av.CodecContext.create("hevc", "r")

    def decode(self, data: bytes, width: int, height: int) -> numpy.ndarray:
        """把单个 HEVC 瓦片解码成指定尺寸的 RGB 数组。"""

        frames = self._codec.decode(self._av.Packet(data))
        if not frames:
            raise ValueError("HEVC tile did not decode to a frame")
        image = frames[0].to_ndarray(format="rgb24")
        if image.shape[:2] == (height, width):
            return image

        padded = numpy.full((height, width, 3), 255, dtype=numpy.uint8)
        copy_height = min(height, image.shape[0])
        copy_width = min(width, image.shape[1])
        padded[:copy_height, :copy_width] = image[:copy_height, :copy_width, :3]
        return padded


def parse_args(argv: Sequence[str] | None = None) -> BatchOptions:
    """解析 SDPC 转换脚本的通用命令行参数。"""

    parser = argparse.ArgumentParser(
        description=(
            "Convert TeksqRay .sdpc/.dyqx whole-slide images to Aperio SVS. "
            "Format details follow openslide/openslide PR #672."
        )
    )
    add_batch_arguments(parser, "Path to an input .sdpc/.dyqx file or directory.")
    add_jpeg_quality_argument(parser)
    return batch_options_from_args(parser.parse_args(argv))


def aperio_main_description(
    metadata: SlideMetadata, tile_width: int, tile_height: int, level: PyramidLevel
) -> str:
    """生成主图页面写入 SVS 时使用的 Aperio 描述字符串。"""

    return (
        f"{APERIO_VERSION}\n"
        f"{level.width}x{level.height} [0,0 {level.width}x{level.height}] "
        f"({tile_width}x{tile_height}) JPEG/RGB Q={metadata.jpeg_quality}"
        f"|AppMag = {metadata.app_mag:g}"
        f"|MPP = {metadata.mpp:.6f}"
    )


def print_slide_info(slide: SdpcSlide) -> None:
    """打印 SDPC 幻灯片的摘要信息。"""

    print(
        f"Image : {slide.metadata.width}x{slide.metadata.height}, "
        f"tile={slide.tile_width}x{slide.tile_height}, "
        f"levels={len(slide.levels)}, compression={slide.source_compression}"
    )
    print(
        f"Meta  : mpp={slide.metadata.mpp:.6f}, app_mag={slide.metadata.app_mag:g}, "
        f"jpeg_quality={slide.metadata.jpeg_quality}, "
        f"thumbnail={slide.thumbnail.width}x{slide.thumbnail.height}"
    )
    print(f"Pyr   : {format_level_summary(slide.levels)}")
    print(f"Assoc : {format_associated_summary(slide.associated_images)}")


def read_range_from_mmap(mm: mmap.mmap, data_range: ByteRange) -> bytes:
    """从内存映射中读取一段字节范围。"""

    if not data_range.present:
        return b""
    return mm[data_range.offset : data_range.offset + data_range.length]


@lru_cache(maxsize=None)
def blank_tile_array(tile_width: int, tile_height: int) -> numpy.ndarray:
    """生成纯白 RGB 占位瓦片数组。"""

    return numpy.full((tile_height, tile_width, 3), 255, dtype=numpy.uint8)


class SdpcParser:
    """负责把 SDPC 文件解析成统一的幻灯片对象。"""

    MACROGRAPH_INFO_SIZE = 123

    def __init__(self, path: Path):
        """记录待解析的 SDPC 文件路径。"""

        self.path = path

    def parse(self) -> SdpcSlide:
        """顺序解析头部、关联图、缩略图和金字塔层级。"""

        with self.path.open("rb") as fh:
            reader = BinaryReader(fh)
            pic_head = self._read_pic_head(reader)
            person_next_offset = self._read_person_info_next_offset(reader, pic_head.head_size)
            associated_images, thumbnail_info_offset = self._read_macrographs(
                reader, person_next_offset, pic_head.macrograph_count
            )
            thumbnail, first_level_offset = self._read_thumbnail(
                reader, thumbnail_info_offset, pic_head.thumbnail_width, pic_head.thumbnail_height
            )
            levels, level_tiles = self._read_levels(reader, pic_head, first_level_offset)

        # 先把底层结构体字段整理成统一元数据，再识别瓦片压缩类型。
        metadata = SlideMetadata(
            width=pic_head.src_width,
            height=pic_head.src_height,
            mpp=pic_head.mpp,
            app_mag=pic_head.app_mag,
            jpeg_quality=pic_head.jpeg_quality,
        )
        source_compression = {
            COMPRESS_JPEG: "jpeg",
            COMPRESS_HEVC: "hevc",
        }.get(pic_head.slice_fmt)
        if source_compression is None:
            raise ValueError(f"Unsupported SDPC tile compression mode: {pic_head.slice_fmt}")

        return SdpcSlide(
            path=self.path,
            metadata=metadata,
            tile_width=pic_head.tile_width,
            tile_height=pic_head.tile_height,
            source_compression=source_compression,
            levels=levels,
            level_tiles=level_tiles,
            associated_images=associated_images,
            thumbnail=thumbnail,
        )

    def _read_pic_head(self, reader: BinaryReader) -> PicHead:
        """读取文件开头的 SqPicHead 主头结构。"""

        flag = reader.u16()
        if flag != PIC_HEAD_FLAG:
            raise ValueError(f"Unsupported SqPicHead flag: 0x{flag:04x}")

        reader.bytes(16)  # version
        head_size = reader.u32()
        reader.u64()  # file size
        macrograph_count = reader.u32()
        reader.u32()  # personInfor
        hierarchy = reader.u32()
        src_width = reader.u32()
        src_height = reader.u32()
        tile_width = reader.u32()
        tile_height = reader.u32()
        thumbnail_width = reader.u32()
        thumbnail_height = reader.u32()
        reader.u8()  # bpp
        jpeg_quality = reader.u8()
        reader.u8()  # color space
        reader.skip(3)
        scale = reader.f32()
        ruler = reader.f64()
        rate = reader.u32()
        extra_offset = reader.u64()
        tile_offset = reader.u64()
        slice_fmt = reader.u8()

        if scale <= 0 or ruler <= 0 or rate <= 0:
            raise ValueError("Invalid dimensions or metadata in SqPicHead")
        if tile_width <= 0 or tile_height <= 0:
            raise ValueError("Invalid tile size in SqPicHead")

        return PicHead(
            head_size=head_size,
            macrograph_count=macrograph_count,
            hierarchy=hierarchy,
            src_width=src_width,
            src_height=src_height,
            tile_width=tile_width,
            tile_height=tile_height,
            thumbnail_width=thumbnail_width,
            thumbnail_height=thumbnail_height,
            jpeg_quality=jpeg_quality,
            mpp=ruler,
            app_mag=float(rate),
            extra_offset=extra_offset,
            tile_offset=tile_offset,
            slice_fmt=slice_fmt,
        )

    def _read_person_info_next_offset(self, reader: BinaryReader, offset: int) -> int:
        """读取病人信息块，并返回下一个信息块偏移。"""

        reader.seek(offset)
        flag = reader.u16()
        if flag != PERSON_INFO_FLAG:
            raise ValueError(f"Unsupported SqPersonInfo flag: 0x{flag:04x}")

        reader.u32()  # infoSize
        reader.skip(64 + 64 + 1 + 1 + 64 + 64 + 1024 + 2048 + 2048 + 64 + 64 + 1024)
        next_offset = reader.u64()
        reader.skip(4 + 4 + 256)
        return next_offset

    def _read_macrographs(
        self, reader: BinaryReader, offset: int, count: int
    ) -> tuple[tuple[AssociatedImageEntry, ...], int]:
        """读取关联图信息链，并返回缩略图信息块偏移。"""

        associated_images: list[AssociatedImageEntry] = []
        current_offset = offset

        for index in range(count):
            macrograph = self._read_macrograph_info(reader, current_offset)
            if index == 0:
                kind = "label"
            elif index == 1:
                kind = "macro"
            else:
                kind = f"macro_{index}"
            associated_images.append(AssociatedImageEntry(kind=kind, data_range=macrograph.data_range))
            current_offset = macrograph.next_layer_offset

        return tuple(associated_images), current_offset

    def _read_macrograph_info(self, reader: BinaryReader, offset: int) -> MacrographInfo:
        """读取单个宏观图信息块。"""

        reader.seek(offset)
        flag = reader.u16()
        if flag != MACROGRAPH_INFO_FLAG:
            raise ValueError(f"Unsupported SqMacrographInfo flag: 0x{flag:04x}")

        reader.u64()  # rgb
        width = reader.u32()
        height = reader.u32()
        reader.u32()  # chance
        reader.u32()  # step
        reader.u64()  # rgbSize
        encoded_size = reader.u64()
        reader.u8()  # quality
        next_layer_offset = reader.u64()
        reader.u32()
        reader.u32()
        reader.bytes(64)

        return MacrographInfo(
            width=width,
            height=height,
            encoded_size=encoded_size,
            next_layer_offset=next_layer_offset,
            data_range=ByteRange(offset + self.MACROGRAPH_INFO_SIZE, encoded_size),
        )

    def _read_thumbnail(
        self, reader: BinaryReader, offset: int, width: int, height: int
    ) -> tuple[ThumbnailEntry, int]:
        """读取缩略图信息块，并返回首个金字塔层偏移。"""

        pic_info = self._read_pic_info(reader, offset)
        if pic_info.slice_num != 1 or pic_info.slice_num_x != 1 or pic_info.slice_num_y != 1:
            raise ValueError("Unsupported thumbnail layout in SDPC")

        return (
            ThumbnailEntry(
                width=width,
                height=height,
                data_range=ByteRange(offset + pic_info.info_size, pic_info.layer_size),
            ),
            pic_info.next_layer_offset,
        )

    def _read_levels(
        self, reader: BinaryReader, pic_head: PicHead, offset: int
    ) -> tuple[tuple[PyramidLevel, ...], tuple[tuple[ByteRange, ...], ...]]:
        """读取所有金字塔层的几何信息和瓦片字节范围。"""

        levels: list[PyramidLevel] = []
        level_tiles: list[tuple[ByteRange, ...]] = []
        current_offset = offset

        for index in range(pic_head.hierarchy):
            pic_info = self._read_pic_info(reader, current_offset)
            if pic_info.slice_num != pic_info.slice_num_x * pic_info.slice_num_y:
                raise ValueError(
                    f"Tile count mismatch at level {index}: "
                    f"{pic_info.slice_num_x}x{pic_info.slice_num_y} != {pic_info.slice_num}"
                )

            # SDPC 用 cur_scale 表示缩放比，这里换算成常见的 downsample。
            downsample = self._downsample_from_scale(pic_info.cur_scale)
            levels.append(
                PyramidLevel(
                    index=index,
                    width=max(pic_head.src_width // downsample, 1),
                    height=max(pic_head.src_height // downsample, 1),
                    downsample=downsample,
                    tile_cols=pic_info.slice_num_x,
                    tile_rows=pic_info.slice_num_y,
                )
            )
            level_tiles.append(
                self._read_tile_ranges(reader, current_offset + pic_info.info_size, pic_info.slice_num)
            )
            current_offset = pic_info.next_layer_offset

        return tuple(levels), tuple(level_tiles)

    def _read_pic_info(self, reader: BinaryReader, offset: int) -> PicInfo:
        """读取通用的 SqPicInfo 结构。"""

        reader.seek(offset)
        flag = reader.u16()
        if flag != PIC_INFO_FLAG:
            raise ValueError(f"Unsupported SqPicInfo flag: 0x{flag:04x}")

        info_size = reader.u32()
        layer = reader.u32()
        slice_num = reader.u32()
        slice_num_x = reader.u32()
        slice_num_y = reader.u32()
        layer_size = reader.u64()
        next_layer_offset = reader.u64()
        cur_scale = reader.f32()
        ruler = reader.f64()
        default_x = reader.u32()
        default_y = reader.u32()
        reader.u8()  # bmpFlag
        reader.bytes(63)

        return PicInfo(
            info_size=info_size,
            layer=layer,
            slice_num=slice_num,
            slice_num_x=slice_num_x,
            slice_num_y=slice_num_y,
            layer_size=layer_size,
            next_layer_offset=next_layer_offset,
            cur_scale=cur_scale,
            ruler=ruler,
            default_x=default_x,
            default_y=default_y,
        )

    def _read_tile_ranges(
        self, reader: BinaryReader, lengths_offset: int, tile_count: int
    ) -> tuple[ByteRange, ...]:
        """根据长度表推导每个瓦片在文件中的实际偏移。"""

        reader.seek(lengths_offset)
        lengths = [reader.i32() for _ in range(tile_count)]
        if any(length < 0 for length in lengths):
            raise ValueError("Negative tile length found in SDPC")

        current_offset = lengths_offset + tile_count * 4
        tiles: list[ByteRange] = []
        for length in lengths:
            tiles.append(ByteRange(current_offset, length))
            current_offset += length
        return tuple(tiles)

    @staticmethod
    def _downsample_from_scale(cur_scale: float) -> int:
        """把层级中的缩放比换算为整数 downsample。"""

        if cur_scale <= 0:
            raise ValueError(f"Invalid level scale: {cur_scale}")
        downsample = int(round(1.0 / cur_scale))
        if downsample <= 0:
            raise ValueError(f"Invalid level downsample: {downsample}")
        if not math.isclose(cur_scale * downsample, 1.0, rel_tol=1e-5, abs_tol=1e-5):
            raise ValueError(f"Unsupported non-integral level scale: {cur_scale}")
        return downsample


class SvsWriter:
    """负责把解析后的 SDPC 内容写成 SVS。"""

    def __init__(
        self,
        slide: SdpcSlide,
        *,
        source_jpeg_quality: int | None = None,
        decode_workers: int | None = None,
    ):
        """准备写出阶段需要的分辨率和空白瓦片缓存。"""

        self.slide = slide
        self.resolution = pixels_per_centimeter(slide.metadata.mpp)
        self.source_jpeg_quality = (
            slide.metadata.jpeg_quality
            if source_jpeg_quality is None
            else source_jpeg_quality
        )
        self.merge_cols = 16 // math.gcd(slide.tile_width, 16)
        self.merge_rows = 16 // math.gcd(slide.tile_height, 16)
        self.output_tile_width = slide.tile_width * self.merge_cols
        self.output_tile_height = slide.tile_height * self.merge_rows
        self.can_reuse_jpeg_tiles = (
            slide.source_compression == "jpeg"
            and self.source_jpeg_quality == slide.metadata.jpeg_quality
            and self.merge_cols == 1
            and self.merge_rows == 1
        )
        self.blank_jpeg_tile = blank_rgb_jpeg(
            self.output_tile_width,
            self.output_tile_height,
            self.source_jpeg_quality,
        )
        self.blank_array_tile = blank_tile_array(slide.tile_width, slide.tile_height)
        self.decode_workers = max(
            1,
            min(
                MAX_DECODE_WORKERS,
                os.cpu_count() or 1,
                MAX_DECODE_WORKERS if decode_workers is None else decode_workers,
            ),
        )
        self._decoder_local = threading.local()
        self.hevc_decoder: HevcTileDecoder | None = None
        if slide.source_compression == "hevc":
            self.hevc_decoder = HevcTileDecoder()

    def write(self, output_path: Path, skip_associated: bool) -> None:
        """写出主图、缩略图、金字塔层和关联图。"""

        with tempfile.NamedTemporaryFile(
            prefix=f".{output_path.name}.",
            suffix=".tmp",
            dir=output_path.parent,
            delete=False,
        ) as temporary_file:
            temporary_path = Path(temporary_file.name)

        try:
            with self.slide.path.open("rb") as fh, mmap.mmap(
                fh.fileno(), 0, access=mmap.ACCESS_READ
            ) as mm:
                thumbnail = self._decode_thumbnail(mm)
                associated_images = (
                    {} if skip_associated else self._load_associated_images(mm)
                )

                with tifffile.TiffWriter(
                    temporary_path, bigtiff=should_use_bigtiff(self.slide.path)
                ) as tif:
                    self._write_tiled_level(tif, mm, self.slide.levels[0], reduced=False)
                    self._write_thumbnail(tif, thumbnail)
                    for level in self.slide.levels[1:]:
                        self._write_tiled_level(tif, mm, level, reduced=True)
                    self._write_associated_images(tif, associated_images)
            temporary_path.replace(output_path)
        except BaseException:
            temporary_path.unlink(missing_ok=True)
            raise

    def _write_tiled_level(
        self, tif: tifffile.TiffWriter, mm: mmap.mmap, level: PyramidLevel, *, reduced: bool
    ) -> None:
        """写出一个瓦片化的主图或降采样层。"""

        common_kwargs = dict(
            shape=(level.height, level.width, 3),
            dtype=numpy.uint8,
            photometric="rgb",
            tile=(self.output_tile_height, self.output_tile_width),
            compression="jpeg",
            resolutionunit="CENTIMETER",
            metadata=None,
        )

        # 同质量 JPEG 可直接透传；HEVC 或变更质量时需要重新编码。
        if self.can_reuse_jpeg_tiles:
            data = self._jpeg_tile_iterator(mm, level)
        else:
            data = self._decoded_tile_iterator(mm, level)
            common_kwargs["compressionargs"] = jpeg_compressionargs(
                self.slide.metadata.jpeg_quality
            )

        if reduced:
            tif.write(
                data=data,
                **common_kwargs,
                subfiletype=1,
                resolution=(
                    self.resolution / level.downsample,
                    self.resolution / level.downsample,
                ),
                software=False,
            )
        else:
            tif.write(
                data=data,
                **common_kwargs,
                description=aperio_main_description(
                    self.slide.metadata,
                    self.output_tile_width,
                    self.output_tile_height,
                    level,
                ),
                resolution=(self.resolution, self.resolution),
                software="sdpc_to_svs.py",
            )

    def _write_thumbnail(
        self, tif: tifffile.TiffWriter, thumbnail: numpy.ndarray
    ) -> None:
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

    def _jpeg_tile_iterator(self, mm: mmap.mmap, level: PyramidLevel) -> Iterator[bytes]:
        """按层级顺序迭代 JPEG 瓦片字节。"""

        for tile_range in self.slide.level_tiles[level.index]:
            yield read_range_from_mmap(mm, tile_range) or self.blank_jpeg_tile

    def _decoded_tile_iterator(
        self, mm: mmap.mmap, level: PyramidLevel
    ) -> Iterator[numpy.ndarray]:
        """按输出 TIFF 瓦片布局迭代解码和合并后的 RGB 数组。"""

        source_tiles = self.slide.level_tiles[level.index]
        expected_source_tiles = level.tile_cols * level.tile_rows
        if len(source_tiles) != expected_source_tiles:
            raise ValueError(
                f"Level {level.index} tile count mismatch: "
                f"expected {expected_source_tiles}, got {len(source_tiles)}"
            )

        output_cols = math.ceil(level.tile_cols / self.merge_cols)
        output_rows = math.ceil(level.tile_rows / self.merge_rows)
        positions = (
            (output_row, output_col)
            for output_row in range(output_rows)
            for output_col in range(output_cols)
        )
        compose = lambda position: self._compose_output_tile(
            mm,
            level,
            source_tiles,
            *position,
        )

        if self.decode_workers == 1:
            for position in positions:
                yield compose(position)
            return

        max_pending = self.decode_workers * DECODE_PREFETCH_FACTOR
        with ThreadPoolExecutor(
            max_workers=self.decode_workers,
            thread_name_prefix="sdpc-decode",
        ) as executor:
            pending = deque()
            for position in positions:
                pending.append(executor.submit(compose, position))
                if len(pending) >= max_pending:
                    yield pending.popleft().result()
            while pending:
                yield pending.popleft().result()

    def _compose_output_tile(
        self,
        mm: mmap.mmap,
        level: PyramidLevel,
        source_tiles: tuple[ByteRange, ...],
        output_row: int,
        output_col: int,
    ) -> numpy.ndarray:
        """解码并合并一个输出 TIFF 瓦片覆盖的源瓦片。"""

        output_tile = numpy.full(
            (self.output_tile_height, self.output_tile_width, 3),
            255,
            dtype=numpy.uint8,
        )
        decoder = self._worker_hevc_decoder()
        for inner_row in range(self.merge_rows):
            source_row = output_row * self.merge_rows + inner_row
            if source_row >= level.tile_rows:
                continue
            for inner_col in range(self.merge_cols):
                source_col = output_col * self.merge_cols + inner_col
                if source_col >= level.tile_cols:
                    continue
                tile_index = source_row * level.tile_cols + source_col
                source_tile = self._decode_source_tile(
                    mm,
                    source_tiles[tile_index],
                    hevc_decoder=decoder,
                )
                top = inner_row * self.slide.tile_height
                left = inner_col * self.slide.tile_width
                output_tile[
                    top : top + self.slide.tile_height,
                    left : left + self.slide.tile_width,
                ] = source_tile
        return output_tile

    def _worker_hevc_decoder(self) -> HevcTileDecoder | None:
        """为当前工作线程惰性创建并复用独立的 HEVC 解码器。"""

        if self.slide.source_compression != "hevc":
            return None
        decoder = getattr(self._decoder_local, "hevc_decoder", None)
        if decoder is None:
            decoder = HevcTileDecoder()
            self._decoder_local.hevc_decoder = decoder
        return decoder

    def _decode_source_tile(
        self,
        mm: mmap.mmap,
        tile_range: ByteRange,
        *,
        hevc_decoder: HevcTileDecoder | None = None,
    ) -> numpy.ndarray:
        """解码单个源瓦片，并规范为 SDPC 头声明的瓦片尺寸。"""

        tile_data = read_range_from_mmap(mm, tile_range)
        if not tile_data:
            return self.blank_array_tile

        if self.slide.source_compression == "jpeg":
            decoded = decode_rgb_image(tile_data)
            if decoded.shape[:2] == (
                self.slide.tile_height,
                self.slide.tile_width,
            ):
                return decoded
            normalized = self.blank_array_tile.copy()
            copy_height = min(self.slide.tile_height, decoded.shape[0])
            copy_width = min(self.slide.tile_width, decoded.shape[1])
            normalized[:copy_height, :copy_width] = decoded[
                :copy_height, :copy_width, :3
            ]
            return normalized

        decoder = self.hevc_decoder if hevc_decoder is None else hevc_decoder
        assert decoder is not None
        return decoder.decode(
            tile_data, self.slide.tile_width, self.slide.tile_height
        )

    def _load_associated_images(self, mm: mmap.mmap) -> dict[str, numpy.ndarray]:
        """读取所有关联图并解码成 RGB 数组。"""

        images: dict[str, numpy.ndarray] = {}
        for entry in self.slide.associated_images:
            data = read_range_from_mmap(mm, entry.data_range)
            if data:
                images[entry.kind] = decode_rgb_image(data)
        return images

    def _decode_thumbnail(self, mm: mmap.mmap) -> numpy.ndarray:
        """优先直接解码缩略图，失败时回退到 BMP 或重建逻辑。"""

        data = read_range_from_mmap(mm, self.slide.thumbnail.data_range)
        if data:
            try:
                return decode_rgb_image(data)
            except UnidentifiedImageError:
                # 某些文件的缩略图不是标准图片格式，继续尝试 BMP 原始像素路径。
                pass

            if len(data) >= 54:
                raw = data[54:]
                expected = self.slide.thumbnail.width * self.slide.thumbnail.height * 4
                if len(raw) >= expected:
                    # BMP 缩略图通常是 BGRA 排列，这里转换为 RGB。
                    bgra = numpy.frombuffer(raw[:expected], dtype=numpy.uint8).reshape(
                        self.slide.thumbnail.height,
                        self.slide.thumbnail.width,
                        4,
                    )
                    return bgra[:, :, [2, 1, 0]].copy()

        return self._render_fallback_thumbnail(mm)

    def _render_fallback_thumbnail(self, mm: mmap.mmap, max_size: int = 1024) -> numpy.ndarray:
        """当缩略图不可读时，从较低分辨率层重建一张缩略图。"""

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

        tiles = self.slide.level_tiles[level.index]
        if len(tiles) != level.tile_count:
            raise ValueError(
                f"Level {level.index} tile count mismatch: "
                f"expected {level.tile_count}, got {len(tiles)}"
            )

        canvas = numpy.full((level.height, level.width, 3), 255, dtype=numpy.uint8)
        for tile_index, tile_range in enumerate(tiles):
            tile_data = read_range_from_mmap(mm, tile_range)
            if not tile_data:
                continue

            # 末边界瓦片可能比标准 tile 小，因此后面需要按实际可用范围裁切。
            tile = self._decode_source_tile(mm, tile_range)

            row, col = divmod(tile_index, level.tile_cols)
            top = row * self.slide.tile_height
            left = col * self.slide.tile_width
            bottom = min(top + self.slide.tile_height, level.height)
            right = min(left + self.slide.tile_width, level.width)
            canvas[top:bottom, left:right] = tile[: bottom - top, : right - left]

        return canvas


def convert_one(
    input_path: Path,
    output_path: Path,
    jpeg_quality: int | None,
    skip_associated: bool,
    overwrite: bool,
) -> None:
    """完成单个 SDPC 文件到 SVS 的转换。"""

    if output_path.exists() and not overwrite:
        print(f"Skip  : {input_path} -> {output_path} (already exists)")
        return

    output_path.parent.mkdir(parents=True, exist_ok=True)

    slide = SdpcParser(input_path).parse()
    source_jpeg_quality = slide.metadata.jpeg_quality
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
    writer = SvsWriter(slide, source_jpeg_quality=source_jpeg_quality)

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
        suffix_label=".sdpc/.dyqx",
        output_error_message="--output can only be used with a single input file",
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
