#!/usr/bin/env python3
from __future__ import annotations

import argparse
import base64
import configparser
import mmap
import struct
import xml.etree.ElementTree as ET
from dataclasses import dataclass, replace
from pathlib import Path
from typing import Iterator, Sequence

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

SUPPORTED_SUFFIXES = {".mdsx", ".msdx"}


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

        return self.offset > 0 and self.length > 0


@dataclass(frozen=True)
class AssociatedImageEntry:
    """描述关联图在容器中的位置。"""

    kind: str
    data_range: ByteRange


@dataclass(frozen=True)
class MetadataOffsets:
    """保存 BKIO 元数据区里几个关键数据块的偏移。"""

    property_xml: ByteRange
    slide_xml: ByteRange
    label: ByteRange
    macro: ByteRange


@dataclass(frozen=True)
class BkioSlide:
    """保存 MDSX 解析完成后的统一幻灯片对象。"""

    path: Path
    metadata: SlideMetadata
    tile_size: int
    levels: tuple[PyramidLevel, ...]
    level_tiles: tuple[tuple[ByteRange, ...], ...]
    associated_images: tuple[AssociatedImageEntry, ...]


@dataclass(frozen=True)
class CliOptions(BatchOptions):
    """MDSX 转换入口额外支持的 tile 校验参数。"""

    tile_size: int | None = None


def parse_args(argv: Sequence[str] | None = None) -> CliOptions:
    """解析 MDSX 转换脚本的命令行参数。"""

    parser = argparse.ArgumentParser(
        description="Convert Motic EasyScan .mdsx/.msdx whole-slide images to SVS."
    )
    add_batch_arguments(parser, "Path to an input .mdsx/.msdx file or directory.")
    add_jpeg_quality_argument(parser)
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
        tile_size=args.tile_size,
    )


def read_ini(path: Path) -> configparser.ConfigParser:
    """按宽容编码方式读取 INI 文件。"""

    config = configparser.ConfigParser()
    with path.open("r", encoding="utf-8", errors="ignore") as fh:
        config.read_file(fh)
    return config


def maybe_read_ini(path: Path) -> configparser.ConfigParser | None:
    """文件存在时读取 INI，否则返回 None。"""

    return read_ini(path) if path.exists() else None


def read_int32_le(fh) -> int:
    """从文件中读取一个小端 32 位整数。"""

    data = fh.read(4)
    if len(data) != 4:
        raise ValueError("Unexpected end of file while reading MDSX index")
    return struct.unpack("<I", data)[0]


def read_range(fh, data_range: ByteRange) -> bytes:
    """从普通文件句柄中读取指定字节范围。"""

    if not data_range.present:
        return b""
    fh.seek(data_range.offset)
    return fh.read(data_range.length)


def read_range_from_mmap(mm: mmap.mmap, data_range: ByteRange) -> bytes:
    """从内存映射中读取指定字节范围。"""

    if not data_range.present:
        return b""
    return mm[data_range.offset : data_range.offset + data_range.length]


def decode_mdsx_xml(data: bytes) -> str:
    """解码 MDSX 中以 UTF-16LE 或 base64 存储的 XML 文本。"""

    if not data:
        return ""
    if data[:1] != b"<":
        data = base64.b64decode(data)
    return data.decode("utf-16le", errors="ignore").rstrip("\x00")


def parse_property_xml(xml_text: str) -> dict[str, str]:
    """把属性 XML 展平成 tag 到 value 的字典。"""

    if not xml_text:
        return {}

    root = ET.fromstring(xml_text)
    properties: dict[str, str] = {}
    for child in root.iter():
        if child is root:
            continue
        value = child.get("value")
        if value is not None:
            properties[child.tag] = value
    return properties


def xml_int(parent: ET.Element, tag: str) -> int:
    """读取 XML 节点下某个 value 属性并转为整数。"""

    element = parent.find(tag)
    if element is None:
        raise ValueError(f"Missing XML element: {tag}")
    value = element.get("value")
    if value is None:
        raise ValueError(f"Missing XML value attribute: {tag}")
    return int(value)


def config_float(
    config: configparser.ConfigParser | None, section: str, option: str
) -> float | None:
    """安全读取 INI 中的浮点配置项。"""

    if config is None or not config.has_option(section, option):
        return None
    return config.getfloat(section, option)


def config_int(
    config: configparser.ConfigParser | None, section: str, option: str
) -> int | None:
    """安全读取 INI 中的整型配置项。"""

    if config is None or not config.has_option(section, option):
        return None
    return config.getint(section, option)


def maybe_float(value: str | None) -> float | None:
    """尝试把字符串转成浮点数，失败时返回 None。"""

    if value is None:
        return None
    try:
        return float(value)
    except ValueError:
        return None


def maybe_int(value: str | None) -> int | None:
    """尝试把字符串转成整数，失败时返回 None。"""

    if value is None:
        return None
    try:
        return int(value)
    except ValueError:
        return None


def coalesce(*values):
    """返回参数列表中第一个非 None 的值。"""

    for value in values:
        if value is not None:
            return value
    return None


def aperio_main_description(
    metadata: SlideMetadata, tile_size: int, level: PyramidLevel
) -> str:
    """生成主图页面写入 SVS 时使用的 Aperio 描述字符串。"""

    return (
        f"{APERIO_VERSION}\n"
        f"{level.width}x{level.height} [0,0 {level.width}x{level.height}] "
        f"({tile_size}x{tile_size}) JPEG/RGB Q={metadata.jpeg_quality}"
        f"|AppMag = {metadata.app_mag:g}"
        f"|MPP = {metadata.mpp:.6f}"
    )


def print_slide_info(slide: BkioSlide) -> None:
    """打印 MDSX 幻灯片的摘要信息。"""

    print(
        f"Image : {slide.metadata.width}x{slide.metadata.height}, "
        f"tile={slide.tile_size}x{slide.tile_size}, "
        f"levels={len(slide.levels)}"
    )
    print(
        f"Meta  : mpp={slide.metadata.mpp:.6f}, app_mag={slide.metadata.app_mag:g}, "
        f"jpeg_quality={slide.metadata.jpeg_quality}"
    )
    print(f"Pyr   : {format_level_summary(slide.levels)}")
    print(f"Assoc : {format_associated_summary(slide.associated_images)}")


class BkioParser:
    """负责把 BKIO 容器中的 MDSX 内容解析成统一对象。"""

    MAGIC = b"BKIO"
    HEADER_OFFSET = 84
    METADATA_BLOCK_COUNT = 5
    LEVEL_INFO_OFFSET = 164

    def __init__(self, path: Path):
        """记录待解析文件和其所在目录。"""

        self.path = path
        self.base_dir = path.parent

    def parse(self) -> BkioSlide:
        """解析 BKIO 头、元数据、层级信息和关联图。"""

        with self.path.open("rb") as fh:
            self._require_magic(fh)
            offsets = self._read_metadata_offsets(fh)
            properties = self._read_properties(fh, offsets.property_xml)
            width, height, tile_size, levels = self._read_levels(fh, offsets.slide_xml)
            level_tiles = self._read_level_tiles(fh, levels)

        # MDSX 会把部分元数据拆在 XML、info.ini 和 meta 中，这里统一归并。
        metadata = self._load_metadata(properties, width, height)
        associated_images = tuple(
            entry
            for entry in (
                AssociatedImageEntry(kind="label", data_range=offsets.label),
                AssociatedImageEntry(kind="macro", data_range=offsets.macro),
            )
            if entry.data_range.present
        )
        return BkioSlide(
            path=self.path,
            metadata=metadata,
            tile_size=tile_size,
            levels=levels,
            level_tiles=level_tiles,
            associated_images=associated_images,
        )

    def _require_magic(self, fh) -> None:
        """校验文件是否为 BKIO 容器。"""

        if fh.read(4) != self.MAGIC:
            raise ValueError(f"Unsupported MDSX container: {self.path}")

    def _read_metadata_offsets(self, fh) -> MetadataOffsets:
        """读取头部索引并定位属性 XML、关联图和 slide XML。"""

        fh.seek(self.HEADER_OFFSET)
        block_offsets: list[int] = []
        for _ in range(self.METADATA_BLOCK_COUNT):
            read_int32_le(fh)
            read_int32_le(fh)
            block_offsets.append(read_int32_le(fh))
            read_int32_le(fh)

        fh.seek(block_offsets[0])
        fh.seek(20, 1)
        property_xml = ByteRange(offset=read_int32_le(fh), length=read_int32_le(fh))
        macro = self._read_tagged_range(fh)
        label = self._read_tagged_range(fh)
        slide_xml = self._read_tagged_range(fh)
        return MetadataOffsets(
            property_xml=property_xml,
            slide_xml=slide_xml,
            label=label,
            macro=macro,
        )

    def _read_tagged_range(self, fh) -> ByteRange:
        """读取带 6 字节标签前缀的数据段偏移。"""

        fh.seek(6, 1)
        return ByteRange(offset=read_int32_le(fh), length=read_int32_le(fh))

    def _read_properties(self, fh, data_range: ByteRange) -> dict[str, str]:
        """读取属性 XML 并转换为字典。"""

        return parse_property_xml(decode_mdsx_xml(read_range(fh, data_range)))

    def _read_levels(
        self, fh, data_range: ByteRange
    ) -> tuple[int, int, int, tuple[PyramidLevel, ...]]:
        """从 slide XML 中读取主图尺寸、tile 大小和各层级布局。"""

        root = ET.fromstring(decode_mdsx_xml(read_range(fh, data_range)))
        image_matrix = root.find("ImageMatrix")
        if image_matrix is None:
            raise ValueError("Missing ImageMatrix section in MDSX slide XML")

        width = xml_int(image_matrix, "Width")
        height = xml_int(image_matrix, "Height")
        tile_width = xml_int(image_matrix, "CellWidth")
        tile_height = xml_int(image_matrix, "CellHeight")
        layer_count = xml_int(image_matrix, "LayerCount")

        if tile_width != tile_height:
            raise ValueError(
                f"Unsupported non-square MDSX tile size: {tile_width}x{tile_height}"
            )

        levels: list[PyramidLevel] = []
        current_width = width
        current_height = height
        for level_index in range(layer_count):
            layer = image_matrix.find(f"Layer{level_index}")
            if layer is None:
                raise ValueError(f"Missing Layer{level_index} in MDSX slide XML")
            levels.append(
                PyramidLevel(
                    index=level_index,
                    width=current_width,
                    height=current_height,
                    tile_cols=xml_int(layer, "Cols"),
                    tile_rows=xml_int(layer, "Rows"),
                )
            )
            # MDSX 金字塔层默认按 2 倍降采样递减。
            current_width = (current_width + 1) // 2
            current_height = (current_height + 1) // 2

        return width, height, tile_width, tuple(levels)

    def _read_level_tiles(
        self, fh, levels: tuple[PyramidLevel, ...]
    ) -> tuple[tuple[ByteRange, ...], ...]:
        """读取每一层的瓦片索引表。"""

        level_tiles: list[tuple[ByteRange, ...]] = []
        for level in levels:
            fh.seek(self.LEVEL_INFO_OFFSET + level.index * 16)
            read_int32_le(fh)
            read_int32_le(fh)
            tiles_offset = read_int32_le(fh)
            tiles_length = read_int32_le(fh)

            tile_count = max(tiles_length - 4, 0) // 10
            if tile_count != level.tile_count:
                raise ValueError(
                    f"Tile count mismatch at level {level.index}: "
                    f"xml={level.tile_count}, index={tile_count}"
                )

            fh.seek(tiles_offset + 4)
            tiles: list[ByteRange] = []
            for _ in range(tile_count):
                # 每条索引前有 2 字节保留字段，需要跳过。
                fh.seek(2, 1)
                tiles.append(ByteRange(offset=read_int32_le(fh), length=read_int32_le(fh)))
            level_tiles.append(tuple(tiles))

        return tuple(level_tiles)

    def _load_metadata(
        self, properties: dict[str, str], width: int, height: int
    ) -> SlideMetadata:
        """从多个来源合并 mpp、物镜倍率和 JPEG 质量等元数据。"""

        info = maybe_read_ini(self.base_dir / "info.ini")
        meta = maybe_read_ini(self.base_dir / "meta")

        mpp = coalesce(
            config_float(meta, "Property", "Scale"),
            config_float(info, "info", "scale"),
            maybe_float(properties.get("Scale")),
        )
        app_mag = coalesce(
            config_float(meta, "Property", "ScanObjective"),
            config_float(info, "info", "scanlens"),
            maybe_float(properties.get("ScanObjective")),
        )
        jpeg_quality = coalesce(
            config_int(meta, "Property", "CompressQuality"),
            maybe_int(properties.get("CompressQuality")),
            75,
        )

        if mpp is None or app_mag is None:
            raise ValueError(f"Missing slide metadata for {self.path}")

        return SlideMetadata(
            width=width,
            height=height,
            mpp=mpp,
            app_mag=app_mag,
            jpeg_quality=jpeg_quality,
        )


class SvsWriter:
    """负责把解析后的 MDSX 内容写成 SVS。"""

    def __init__(
        self,
        slide: BkioSlide,
        expected_tile_size: int | None = None,
        *,
        source_jpeg_quality: int | None = None,
    ):
        """准备写出阶段的分辨率和空白瓦片缓存。"""

        if expected_tile_size is not None and expected_tile_size != slide.tile_size:
            raise ValueError(
                f"--tile-size={expected_tile_size} does not match "
                f"MDSX tile size {slide.tile_size}"
            )

        self.slide = slide
        self.tile_size = slide.tile_size
        self.resolution = pixels_per_centimeter(slide.metadata.mpp)
        self.source_jpeg_quality = (
            slide.metadata.jpeg_quality
            if source_jpeg_quality is None
            else source_jpeg_quality
        )
        self.reencode_jpeg_tiles = self.source_jpeg_quality != slide.metadata.jpeg_quality
        self.blank_tile = blank_rgb_jpeg(
            self.tile_size,
            self.tile_size,
            self.source_jpeg_quality,
        )
        self.blank_array_tile = numpy.full(
            (self.tile_size, self.tile_size, 3),
            255,
            dtype=numpy.uint8,
        )

    def write(self, output_path: Path, skip_associated: bool) -> None:
        """写出主图、缩略图、金字塔层和关联图。"""

        with self.slide.path.open("rb") as fh, mmap.mmap(
            fh.fileno(), 0, access=mmap.ACCESS_READ
        ) as mm:
            thumbnail = self._build_thumbnail(mm)
            associated_images = (
                {} if skip_associated else self._load_associated_images(mm)
            )

            with tifffile.TiffWriter(
                output_path, bigtiff=should_use_bigtiff(self.slide.path)
            ) as tif:
                self._write_tiled_level(tif, mm, self.slide.levels[0], reduced=False)
                self._write_thumbnail(tif, thumbnail)
                for level in self.slide.levels[1:]:
                    self._write_tiled_level(tif, mm, level, reduced=True)
                self._write_associated_images(tif, associated_images)

    def _write_tiled_level(
        self, tif: tifffile.TiffWriter, mm: mmap.mmap, level: PyramidLevel, *, reduced: bool
    ) -> None:
        """写出一个瓦片化的主图或降采样层。"""

        common_kwargs = dict(
            shape=(level.height, level.width, 3),
            dtype=numpy.uint8,
            photometric="rgb",
            tile=(self.tile_size, self.tile_size),
            compression="jpeg",
            resolutionunit="CENTIMETER",
            metadata=None,
        )
        if self.reencode_jpeg_tiles:
            common_kwargs["data"] = self._decoded_tile_iterator(mm, level)
            common_kwargs["compressionargs"] = jpeg_compressionargs(
                self.slide.metadata.jpeg_quality
            )
        else:
            common_kwargs["data"] = self._tile_iterator(mm, level)
        if reduced:
            tif.write(
                **common_kwargs,
                subfiletype=1,
                resolution=(
                    self.resolution / (2**level.index),
                    self.resolution / (2**level.index),
                ),
                software=False,
            )
        else:
            tif.write(
                **common_kwargs,
                description=aperio_main_description(
                    self.slide.metadata, self.tile_size, level
                ),
                resolution=(self.resolution, self.resolution),
                software="mdsx_to_svs.py",
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

    def _build_thumbnail(self, mm: mmap.mmap, max_size: int = 1024) -> numpy.ndarray:
        """从较低分辨率层重建缩略图。"""

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
            tile_bytes = read_range_from_mmap(mm, tile_range)
            if not tile_bytes:
                continue

            row, col = divmod(tile_index, level.tile_cols)
            top = row * self.tile_size
            left = col * self.tile_size
            bottom = min(top + self.tile_size, level.height)
            right = min(left + self.tile_size, level.width)

            # 末边界瓦片可能比标准 tile 小，因此这里按有效区域裁切。
            tile = decode_rgb_image(tile_bytes)
            canvas[top:bottom, left:right] = tile[: bottom - top, : right - left]

        return canvas

    def _tile_iterator(self, mm: mmap.mmap, level: PyramidLevel) -> Iterator[bytes]:
        """按层级顺序迭代 JPEG 瓦片字节。"""

        for tile_range in self.slide.level_tiles[level.index]:
            yield read_range_from_mmap(mm, tile_range) or self.blank_tile

    def _decoded_tile_iterator(
        self, mm: mmap.mmap, level: PyramidLevel
    ) -> Iterator[numpy.ndarray]:
        """按层级顺序迭代解码后的 RGB 瓦片。"""

        for tile_range in self.slide.level_tiles[level.index]:
            tile_bytes = read_range_from_mmap(mm, tile_range)
            if not tile_bytes:
                yield self.blank_array_tile
                continue
            yield decode_rgb_image(tile_bytes)

    def _load_associated_images(self, mm: mmap.mmap) -> dict[str, numpy.ndarray]:
        """读取所有关联图并解码成 RGB 数组。"""

        images: dict[str, numpy.ndarray] = {}
        for entry in self.slide.associated_images:
            data = read_range_from_mmap(mm, entry.data_range)
            if data:
                images[entry.kind] = decode_rgb_image(data)
        return images


def convert_one(
    input_path: Path,
    output_path: Path,
    tile_size: int | None,
    jpeg_quality: int | None,
    skip_associated: bool,
    overwrite: bool,
) -> None:
    """完成单个 MDSX 文件到 SVS 的转换。"""

    if output_path.exists() and not overwrite:
        print(f"Skip  : {input_path} -> {output_path} (already exists)")
        return

    output_path.parent.mkdir(parents=True, exist_ok=True)

    slide = BkioParser(input_path).parse()
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
    writer = SvsWriter(
        slide,
        expected_tile_size=tile_size,
        source_jpeg_quality=source_jpeg_quality,
    )

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
        suffix_label=".mdsx/.msdx",
        output_error_message="--output can only be used with a single .mdsx/.msdx input file",
        runner_factory=lambda input_path, output_path: lambda: convert_one(
            input_path=input_path,
            output_path=output_path,
            tile_size=options.tile_size,
            jpeg_quality=options.jpeg_quality,
            skip_associated=options.skip_associated,
            overwrite=options.overwrite,
        ),
    )
    run_conversion_jobs(jobs)


if __name__ == "__main__":
    main()
