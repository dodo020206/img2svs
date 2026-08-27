#!/usr/bin/env python3
from __future__ import annotations

import argparse
import io
import mmap
import struct
import tempfile
from array import array
from dataclasses import dataclass, replace
from pathlib import Path
from typing import Callable, Iterator, Sequence

import numpy
import tifffile
from PIL import Image

from img2svs.core.svs_common import (
    APERIO_VERSION,
    BatchOptions,
    add_batch_arguments,
    add_jpeg_quality_argument,
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

SUPPORTED_SUFFIXES = {".dmetrix"}


@dataclass(frozen=True)
class ByteRange:
    offset: int
    length: int

    @property
    def present(self) -> bool:
        return self.offset > 0 and self.length > 0


class PackedTileRanges:
    """紧凑保存一个层级的瓦片偏移和长度，避免每块瓦片创建字典对象。"""

    __slots__ = ("_offsets", "_lengths")

    def __init__(self, count: int):
        self._offsets = array("Q", [0]) * count
        self._lengths = array("I", [0]) * count

    def __len__(self) -> int:
        return len(self._offsets)

    def __getitem__(self, index: int) -> ByteRange:
        if index < 0:
            index += len(self)
        if index < 0 or index >= len(self):
            raise IndexError(index)
        return ByteRange(self._offsets[index], self._lengths[index])

    def __iter__(self) -> Iterator[ByteRange]:
        for offset, length in zip(self._offsets, self._lengths):
            yield ByteRange(offset, length)

    def set(self, index: int, data_range: ByteRange) -> None:
        self._offsets[index] = data_range.offset
        self._lengths[index] = data_range.length


@dataclass(frozen=True)
class AssociatedImageEntry:
    kind: str
    data_range: ByteRange


@dataclass(frozen=True)
class SlideMetadata:
    width: int
    height: int
    mpp: float
    app_mag: float
    jpeg_quality: int


@dataclass(frozen=True)
class PyramidLevel:
    index: int
    source_id: int
    width: int
    height: int
    tile_cols: int
    tile_rows: int

    @property
    def tile_count(self) -> int:
        return self.tile_cols * self.tile_rows


@dataclass(frozen=True)
class DmetrixSlide:
    path: Path
    metadata: SlideMetadata
    tile_size: int
    levels: tuple[PyramidLevel, ...]
    level_tiles: tuple[PackedTileRanges, ...]
    associated_images: tuple[AssociatedImageEntry, ...]


@dataclass(frozen=True)
class CliOptions(BatchOptions):
    pass


@dataclass(frozen=True)
class _LevelDescriptor:
    source_id: int
    max_x: int
    max_y: int
    index_offset: int

    @property
    def tile_cols(self) -> int:
        return self.max_x + 1

    @property
    def tile_rows(self) -> int:
        return self.max_y + 1


def parse_args(argv: Sequence[str] | None = None) -> CliOptions:
    parser = argparse.ArgumentParser(
        description="Convert DMetrix .dmetrix whole-slide images to SVS."
    )
    add_batch_arguments(parser, "Path to an input .dmetrix file or directory.")
    add_jpeg_quality_argument(parser)
    args = parser.parse_args(argv)
    batch = batch_options_from_args(args)
    return CliOptions(**vars(batch))


def _read_exact(fh, size: int, context: str) -> bytes:
    data = fh.read(size)
    if len(data) != size:
        raise ValueError(f"Unexpected end of DMetrix file while reading {context}")
    return data


def _image_size(fh, data_range: ByteRange) -> tuple[int, int]:
    fh.seek(data_range.offset)
    data = _read_exact(fh, data_range.length, "image data")
    with Image.open(io.BytesIO(data)) as image:
        return image.size


_JPEG_LUMA_TABLE = (
    16, 11, 10, 16, 24, 40, 51, 61, 12, 12, 14, 19, 26, 58, 60, 55,
    14, 13, 16, 24, 40, 57, 69, 56, 14, 17, 22, 29, 51, 87, 80, 62,
    18, 22, 37, 56, 68, 109, 103, 77, 24, 35, 55, 64, 81, 104, 113, 92,
    49, 64, 78, 87, 103, 121, 120, 101, 72, 92, 95, 98, 112, 100, 103, 99,
)
_JPEG_CHROMA_TABLE = (
    17, 18, 24, 47, 99, 99, 99, 99, 18, 21, 26, 66, 99, 99, 99, 99,
    24, 26, 56, 99, 99, 99, 99, 99, 47, 66, 99, 99, 99, 99, 99, 99,
    99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99,
    99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99,
)


def _scaled_quantization_table(base: tuple[int, ...], quality: int) -> tuple[int, ...]:
    scale = 5000 // quality if quality < 50 else 200 - quality * 2
    return tuple(max(1, min(255, (value * scale + 50) // 100)) for value in base)


def estimate_jpeg_quality(data: bytes, default: int = 75) -> int:
    """Estimate the libjpeg quality represented by an embedded JPEG quantization table."""

    try:
        with Image.open(io.BytesIO(data)) as image:
            tables = image.quantization
        source_tables = [tuple(tables[key]) for key in sorted(tables)[:2]]
    except Exception:
        return default
    if not source_tables:
        return default

    best_quality = default
    best_score: int | None = None
    for quality in range(1, 101):
        candidates = [_scaled_quantization_table(_JPEG_LUMA_TABLE, quality)]
        if len(source_tables) > 1:
            candidates.append(_scaled_quantization_table(_JPEG_CHROMA_TABLE, quality))
        score = sum(
            abs(actual - expected)
            for source, candidate in zip(source_tables, candidates)
            for actual, expected in zip(source, candidate)
        )
        if best_score is None or score < best_score:
            best_quality = quality
            best_score = score
    return best_quality


class DmetrixParser:
    MAGIC = b"DmetrixN"
    MPP_X_OFFSET = 0x30
    MPP_Y_OFFSET = 0x38
    APP_MAG_OFFSET = 0x40
    LEVEL_DESCRIPTOR_OFFSET = 0xC2
    LEVEL_DESCRIPTOR = struct.Struct("<HIII")
    TILE_RECORD = struct.Struct("<HIIQI")
    LABEL_ID = 0xFFFF
    MACRO_ID = 0xFFFE
    ASSOCIATED_COUNT = 2

    def __init__(self, path: Path, progress_callback: Callable[[str], None] | None = None):
        self.path = path
        self.progress_callback = progress_callback

    def parse(self) -> DmetrixSlide:
        file_size = self.path.stat().st_size
        with self.path.open("rb") as fh:
            if _read_exact(fh, len(self.MAGIC), "magic") != self.MAGIC:
                raise ValueError(f"Unsupported DMetrix container: {self.path}")
            mpp_x = self._read_number(fh, self.MPP_X_OFFSET, "<d")
            mpp_y = self._read_number(fh, self.MPP_Y_OFFSET, "<d")
            app_mag = self._read_number(fh, self.APP_MAG_OFFSET, "<I")
            if not (0 < mpp_x < 100 and 0 < mpp_y < 100 and 0 < app_mag <= 200):
                raise ValueError("Invalid DMetrix scan metadata")

            descriptors = self._read_descriptors(fh)
            associated = self._read_associated(fh, descriptors[0].index_offset, file_size)
            raw_tiles = self._read_tile_indexes(fh, descriptors, file_size)
            tile_size = self._discover_tile_size(fh, raw_tiles[-1])

            output_levels: list[PyramidLevel] = []
            output_tiles: list[PackedTileRanges] = []
            for index, (descriptor, tiles_by_coordinate) in enumerate(
                zip(reversed(descriptors), reversed(raw_tiles))
            ):
                edge = tiles_by_coordinate[descriptor.max_y * descriptor.tile_cols + descriptor.max_x]
                edge_width, edge_height = _image_size(fh, edge)
                if not (1 <= edge_width <= tile_size and 1 <= edge_height <= tile_size):
                    raise ValueError(
                        f"Invalid edge tile size at DMetrix level {descriptor.source_id}: "
                        f"{edge_width}x{edge_height}"
                    )
                width = descriptor.max_x * tile_size + edge_width
                height = descriptor.max_y * tile_size + edge_height
                output_levels.append(
                    PyramidLevel(
                        index=index,
                        source_id=descriptor.source_id,
                        width=width,
                        height=height,
                        tile_cols=descriptor.tile_cols,
                        tile_rows=descriptor.tile_rows,
                    )
                )
                output_tiles.append(tiles_by_coordinate)

            first_tile = output_tiles[0][0]
            fh.seek(first_tile.offset)
            jpeg_quality = estimate_jpeg_quality(
                _read_exact(fh, first_tile.length, "first JPEG tile")
            )

        levels = tuple(output_levels)
        return DmetrixSlide(
            path=self.path,
            metadata=SlideMetadata(
                width=levels[0].width,
                height=levels[0].height,
                mpp=(mpp_x + mpp_y) / 2.0,
                app_mag=float(app_mag),
                jpeg_quality=jpeg_quality,
            ),
            tile_size=tile_size,
            levels=levels,
            level_tiles=tuple(output_tiles),
            associated_images=associated,
        )

    @staticmethod
    def _read_number(fh, offset: int, fmt: str):
        fh.seek(offset)
        size = struct.calcsize(fmt)
        return struct.unpack(fmt, _read_exact(fh, size, f"header field at {offset}"))[0]

    def _read_descriptors(self, fh) -> tuple[_LevelDescriptor, ...]:
        fh.seek(self.LEVEL_DESCRIPTOR_OFFSET)
        descriptors: list[_LevelDescriptor] = []
        for _ in range(64):
            source_id, max_x, max_y, index_offset = self.LEVEL_DESCRIPTOR.unpack(
                _read_exact(fh, self.LEVEL_DESCRIPTOR.size, "level descriptor")
            )
            if index_offset == 0:
                break
            if max_x > 100_000 or max_y > 100_000:
                raise ValueError("Invalid DMetrix level grid")
            descriptors.append(_LevelDescriptor(source_id, max_x, max_y, index_offset))
        if not descriptors:
            raise ValueError("DMetrix file contains no pyramid levels")
        if any(right.source_id <= left.source_id for left, right in zip(descriptors, descriptors[1:])):
            raise ValueError("DMetrix pyramid level identifiers are not increasing")
        return tuple(descriptors)

    def _read_associated(
        self, fh, first_index_offset: int, file_size: int
    ) -> tuple[AssociatedImageEntry, ...]:
        start = first_index_offset - self.ASSOCIATED_COUNT * self.TILE_RECORD.size
        if start < 0:
            raise ValueError("Invalid DMetrix associated-image index")
        fh.seek(start)
        by_id: dict[int, ByteRange] = {}
        for _ in range(self.ASSOCIATED_COUNT):
            source_id, _x, _y, offset, length = self.TILE_RECORD.unpack(
                _read_exact(fh, self.TILE_RECORD.size, "associated-image record")
            )
            data_range = ByteRange(offset, length)
            self._validate_range(data_range, file_size, "associated image")
            by_id[source_id] = data_range
        entries: list[AssociatedImageEntry] = []
        for source_id, kind in ((self.LABEL_ID, "label"), (self.MACRO_ID, "macro")):
            data_range = by_id.get(source_id)
            if data_range is not None:
                entries.append(AssociatedImageEntry(kind, data_range))
        return tuple(entries)

    def _read_tile_indexes(
        self, fh, descriptors: tuple[_LevelDescriptor, ...], file_size: int
    ) -> tuple[PackedTileRanges, ...]:
        result: list[PackedTileRanges] = []
        for descriptor in descriptors:
            fh.seek(descriptor.index_offset)
            expected = descriptor.tile_cols * descriptor.tile_rows
            coordinates = PackedTileRanges(expected)
            seen = bytearray(expected)
            self._report_progress(
                f"level {descriptor.source_id}: parsing 0/{expected} tile indexes"
            )
            for record_index in range(expected):
                source_id, x, y, offset, length = self.TILE_RECORD.unpack(
                    _read_exact(fh, self.TILE_RECORD.size, "tile record")
                )
                if source_id != descriptor.source_id:
                    raise ValueError(
                        f"Unexpected DMetrix level id {source_id} in level {descriptor.source_id}"
                    )
                if x > descriptor.max_x or y > descriptor.max_y:
                    raise ValueError(
                        f"Invalid or duplicate tile coordinate ({x}, {y}) "
                        f"at DMetrix level {source_id}"
                    )
                data_range = ByteRange(offset, length)
                self._validate_range(data_range, file_size, f"level {source_id} tile")
                slot = y * descriptor.tile_cols + x
                if seen[slot]:
                    raise ValueError(
                        f"Invalid or duplicate tile coordinate ({x}, {y}) "
                        f"at DMetrix level {source_id}"
                    )
                seen[slot] = 1
                coordinates.set(slot, data_range)
                if record_index == expected - 1 or (record_index + 1) % 100_000 == 0:
                    self._report_progress(
                        f"level {descriptor.source_id}: parsed {record_index + 1}/{expected} tile indexes"
                    )
            if not all(seen):
                raise ValueError(
                    f"Tile count mismatch at DMetrix level {descriptor.source_id}: "
                    f"expected {expected}, got {sum(seen)}"
                )
            result.append(coordinates)
        return tuple(result)

    def _report_progress(self, message: str) -> None:
        if self.progress_callback is not None:
            self.progress_callback(message)

    @staticmethod
    def _validate_range(data_range: ByteRange, file_size: int, context: str) -> None:
        if (
            not data_range.present
            or data_range.offset >= file_size
            or data_range.length > file_size - data_range.offset
        ):
            raise ValueError(f"Invalid byte range for DMetrix {context}")

    @staticmethod
    def _discover_tile_size(
        fh,
        tile_ranges: PackedTileRanges,
    ) -> int:
        width, height = _image_size(fh, tile_ranges[0])
        if width != height or width < 16 or width > 4096:
            raise ValueError(f"Unsupported DMetrix tile size: {width}x{height}")
        return width


def aperio_main_description(
    metadata: SlideMetadata, tile_size: int, level: PyramidLevel
) -> str:
    return (
        f"{APERIO_VERSION}\n"
        f"{level.width}x{level.height} [0,0 {level.width}x{level.height}] "
        f"({tile_size}x{tile_size}) JPEG/RGB Q={metadata.jpeg_quality}"
        f"|AppMag = {metadata.app_mag:g}"
        f"|MPP = {metadata.mpp:.6f}"
    )


def _read_range(mm: mmap.mmap, data_range: ByteRange) -> bytes:
    return mm[data_range.offset : data_range.offset + data_range.length]


def _encode_jpeg(image: numpy.ndarray, quality: int) -> bytes:
    buffer = io.BytesIO()
    Image.fromarray(image).save(buffer, format="JPEG", quality=quality)
    return buffer.getvalue()


class SvsWriter:
    def __init__(self, slide: DmetrixSlide, *, source_jpeg_quality: int | None = None):
        self.slide = slide
        self.resolution = pixels_per_centimeter(slide.metadata.mpp)
        self.source_jpeg_quality = (
            slide.metadata.jpeg_quality
            if source_jpeg_quality is None
            else source_jpeg_quality
        )
        self.reencode_jpeg_tiles = self.source_jpeg_quality != slide.metadata.jpeg_quality
        self.blank_tile = blank_rgb_jpeg(
            slide.tile_size, slide.tile_size, self.source_jpeg_quality
        )
        self.blank_array_tile = numpy.full(
            (slide.tile_size, slide.tile_size, 3), 255, dtype=numpy.uint8
        )

    def write(self, output_path: Path, skip_associated: bool) -> None:
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
                thumbnail = self._render_level(mm, self.slide.levels[-1])
                thumbnail_image = Image.fromarray(thumbnail)
                thumbnail_image.thumbnail((1024, 1024), Image.Resampling.LANCZOS)
                thumbnail = numpy.asarray(thumbnail_image)
                associated = {} if skip_associated else self._load_associated(mm)
                with tifffile.TiffWriter(
                    temporary_path, bigtiff=should_use_bigtiff(self.slide.path)
                ) as tif:
                    self._write_level(tif, mm, self.slide.levels[0], reduced=False)
                    self._write_thumbnail(tif, thumbnail)
                    for level in self.slide.levels[1:]:
                        self._write_level(tif, mm, level, reduced=True)
                    self._write_associated(tif, associated)
            temporary_path.replace(output_path)
        except BaseException:
            temporary_path.unlink(missing_ok=True)
            raise

    def _write_level(
        self, tif: tifffile.TiffWriter, mm: mmap.mmap, level: PyramidLevel, *, reduced: bool
    ) -> None:
        kwargs = dict(
            shape=(level.height, level.width, 3),
            dtype=numpy.uint8,
            photometric="rgb",
            tile=(self.slide.tile_size, self.slide.tile_size),
            compression="jpeg",
            resolutionunit="CENTIMETER",
            metadata=None,
        )
        if self.reencode_jpeg_tiles:
            kwargs["data"] = self._decoded_tile_iterator(mm, level)
            kwargs["compressionargs"] = jpeg_compressionargs(
                self.slide.metadata.jpeg_quality
            )
        else:
            kwargs["data"] = self._tile_iterator(mm, level)
        downsample = self.slide.levels[0].width / level.width
        resolution = self.resolution / downsample
        if reduced:
            tif.write(
                **kwargs,
                subfiletype=1,
                resolution=(resolution, resolution),
                software=False,
            )
        else:
            tif.write(
                **kwargs,
                description=aperio_main_description(
                    self.slide.metadata, self.slide.tile_size, level
                ),
                resolution=(resolution, resolution),
                software="dmetrix_to_svs.py",
            )

    def _write_thumbnail(self, tif: tifffile.TiffWriter, thumbnail: numpy.ndarray) -> None:
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

    def _write_associated(
        self, tif: tifffile.TiffWriter, images: dict[str, numpy.ndarray]
    ) -> None:
        for kind in ("label", "macro"):
            image = images.get(kind)
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

    def _tile_iterator(self, mm: mmap.mmap, level: PyramidLevel) -> Iterator[bytes]:
        for tile_index, data_range in enumerate(self.slide.level_tiles[level.index]):
            data = _read_range(mm, data_range)
            if not data:
                yield self.blank_tile
                continue
            row, col = divmod(tile_index, level.tile_cols)
            if row == level.tile_rows - 1 or col == level.tile_cols - 1:
                tile = decode_rgb_image(data)
                if tile.shape[:2] != (self.slide.tile_size, self.slide.tile_size):
                    yield _encode_jpeg(
                        self._pad_tile(tile), self.slide.metadata.jpeg_quality
                    )
                    continue
            yield data

    def _decoded_tile_iterator(
        self, mm: mmap.mmap, level: PyramidLevel
    ) -> Iterator[numpy.ndarray]:
        for data_range in self.slide.level_tiles[level.index]:
            data = _read_range(mm, data_range)
            yield self._pad_tile(decode_rgb_image(data)) if data else self.blank_array_tile

    def _pad_tile(self, tile: numpy.ndarray) -> numpy.ndarray:
        if tile.shape[:2] == (self.slide.tile_size, self.slide.tile_size):
            return tile
        if tile.shape[0] > self.slide.tile_size or tile.shape[1] > self.slide.tile_size:
            raise ValueError(f"DMetrix tile exceeds {self.slide.tile_size}x{self.slide.tile_size}")
        padded = self.blank_array_tile.copy()
        padded[: tile.shape[0], : tile.shape[1]] = tile
        return padded

    def _render_level(self, mm: mmap.mmap, level: PyramidLevel) -> numpy.ndarray:
        canvas = numpy.full((level.height, level.width, 3), 255, dtype=numpy.uint8)
        for tile_index, data_range in enumerate(self.slide.level_tiles[level.index]):
            tile = decode_rgb_image(_read_range(mm, data_range))
            row, col = divmod(tile_index, level.tile_cols)
            top, left = row * self.slide.tile_size, col * self.slide.tile_size
            bottom = min(top + tile.shape[0], level.height)
            right = min(left + tile.shape[1], level.width)
            canvas[top:bottom, left:right] = tile[: bottom - top, : right - left]
        return canvas

    def _load_associated(self, mm: mmap.mmap) -> dict[str, numpy.ndarray]:
        return {
            entry.kind: decode_rgb_image(_read_range(mm, entry.data_range))
            for entry in self.slide.associated_images
        }


def print_slide_info(slide: DmetrixSlide) -> None:
    print(
        f"Image : {slide.metadata.width}x{slide.metadata.height}, "
        f"tile={slide.tile_size}x{slide.tile_size}, levels={len(slide.levels)}, "
        "compression=jpeg"
    )
    print(
        f"Meta  : mpp={slide.metadata.mpp:.6f}, app_mag={slide.metadata.app_mag:g}, "
        f"jpeg_quality={slide.metadata.jpeg_quality}"
    )
    print(f"Pyr   : {format_level_summary(slide.levels)}")
    print(f"Assoc : {format_associated_summary(slide.associated_images)}")


def convert_one(
    input_path: Path,
    output_path: Path,
    jpeg_quality: int | None,
    skip_associated: bool,
    overwrite: bool,
) -> None:
    if output_path.exists() and not overwrite:
        print(f"Skip  : {input_path} -> {output_path} (already exists)")
        return
    output_path.parent.mkdir(parents=True, exist_ok=True)
    slide = DmetrixParser(
        input_path,
        progress_callback=lambda message: print(f"Index : {message}"),
    ).parse()
    source_jpeg_quality = slide.metadata.jpeg_quality
    slide = replace(
        slide,
        metadata=replace(
            slide.metadata,
            jpeg_quality=normalize_jpeg_quality(
                jpeg_quality, default=slide.metadata.jpeg_quality
            ),
        ),
    )
    print(f"Input : {input_path}")
    print(f"Output: {output_path}")
    print_slide_info(slide)
    if source_jpeg_quality == slide.metadata.jpeg_quality:
        print("Mode  : JPEG passthrough; only partial edge tiles are padded")
    else:
        print(f"Mode  : JPEG re-encode at quality {slide.metadata.jpeg_quality}")
    SvsWriter(slide, source_jpeg_quality=source_jpeg_quality).write(
        output_path, skip_associated
    )
    print("Conversion completed.")


def main() -> None:
    options = parse_args()
    jobs = build_single_format_jobs(
        options,
        supported_suffixes=SUPPORTED_SUFFIXES,
        suffix_label=".dmetrix",
        output_error_message="--output can only be used with a single .dmetrix input file",
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
