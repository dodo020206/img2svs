from __future__ import annotations

import unittest
import tempfile
from dataclasses import replace
from pathlib import Path
from types import SimpleNamespace
from unittest import mock

import numpy

from sdpc_to_svs import (
    ByteRange,
    PyramidLevel,
    SdpcSlide,
    SlideMetadata,
    SvsWriter,
    ThumbnailEntry,
    HevcTileDecoder,
)


def make_slide(tile_width: int, tile_height: int) -> SdpcSlide:
    level = PyramidLevel(
        index=0,
        width=tile_width * 3,
        height=tile_height * 2,
        downsample=1,
        tile_cols=3,
        tile_rows=2,
    )
    return SdpcSlide(
        path=Path("unused.sdpc"),
        metadata=SlideMetadata(
            width=level.width,
            height=level.height,
            mpp=0.25,
            app_mag=20,
            jpeg_quality=75,
        ),
        tile_width=tile_width,
        tile_height=tile_height,
        source_compression="jpeg",
        levels=(level,),
        level_tiles=((ByteRange(-1, 0),) * level.tile_count,),
        associated_images=(),
        thumbnail=ThumbnailEntry(1, 1, ByteRange(-1, 0)),
    )


class SvsWriterTileGeometryTests(unittest.TestCase):
    def test_non_aligned_width_merges_source_tiles_without_padding_gaps(self) -> None:
        writer = SvsWriter(make_slide(616, 880))

        self.assertEqual(writer.merge_cols, 2)
        self.assertEqual(writer.merge_rows, 1)
        self.assertEqual(writer.output_tile_width, 1232)
        self.assertEqual(writer.output_tile_height, 880)
        self.assertEqual(writer.output_tile_width % 16, 0)
        self.assertEqual(writer.output_tile_height % 16, 0)
        self.assertFalse(writer.can_reuse_jpeg_tiles)

    def test_aligned_source_tiles_remain_passthrough_compatible(self) -> None:
        writer = SvsWriter(make_slide(512, 512))

        self.assertEqual(writer.merge_cols, 1)
        self.assertEqual(writer.merge_rows, 1)
        self.assertEqual(writer.output_tile_width, 512)
        self.assertEqual(writer.output_tile_height, 512)
        self.assertTrue(writer.can_reuse_jpeg_tiles)

    def test_odd_source_column_is_merged_in_order_and_padded_on_right(self) -> None:
        class FakeWriter(SvsWriter):
            def _decode_source_tile(
                self, mm, tile_range, *, hevc_decoder=None
            ):
                return numpy.full(
                    (self.slide.tile_height, self.slide.tile_width, 3),
                    tile_range.offset,
                    dtype=numpy.uint8,
                )

        slide = make_slide(616, 880)
        slide = replace(
            slide,
            level_tiles=(
                tuple(ByteRange(index, 1) for index in range(1, 7)),
            ),
        )
        writer = FakeWriter(slide)

        tiles = list(writer._decoded_tile_iterator(None, slide.levels[0]))

        self.assertEqual(len(tiles), 4)
        self.assertTrue(numpy.all(tiles[0][:, :616] == 1))
        self.assertTrue(numpy.all(tiles[0][:, 616:] == 2))
        self.assertTrue(numpy.all(tiles[1][:, :616] == 3))
        self.assertTrue(numpy.all(tiles[1][:, 616:] == 255))
        self.assertTrue(numpy.all(tiles[2][:, :616] == 4))
        self.assertTrue(numpy.all(tiles[2][:, 616:] == 5))
        self.assertTrue(numpy.all(tiles[3][:, :616] == 6))
        self.assertTrue(numpy.all(tiles[3][:, 616:] == 255))

    def test_hevc_decoder_reuses_one_codec_context(self) -> None:
        class FakeFrame:
            def to_ndarray(self, *, format):
                self.format = format
                return numpy.zeros((4, 6, 3), dtype=numpy.uint8)

        class FakeCodec:
            def __init__(self):
                self.decode_calls = 0

            def decode(self, packet):
                self.decode_calls += 1
                return [FakeFrame()]

        codec = FakeCodec()

        class FakeCodecContext:
            create_calls = 0

            @classmethod
            def create(cls, codec_name, mode):
                cls.create_calls += 1
                self.assertEqual((codec_name, mode), ("hevc", "r"))
                return codec

        fake_av = SimpleNamespace(
            CodecContext=FakeCodecContext,
            Packet=lambda data: data,
        )
        with mock.patch.dict("sys.modules", {"av": fake_av}):
            decoder = HevcTileDecoder()
            first = decoder.decode(b"first", 6, 4)
            second = decoder.decode(b"second", 6, 4)

        self.assertEqual(FakeCodecContext.create_calls, 1)
        self.assertEqual(codec.decode_calls, 2)
        self.assertEqual(first.shape, (4, 6, 3))
        self.assertEqual(second.shape, (4, 6, 3))

    def test_failed_write_removes_temporary_output(self) -> None:
        class FailingWriter(SvsWriter):
            def _decode_thumbnail(self, mm):
                return numpy.zeros((1, 1, 3), dtype=numpy.uint8)

            def _load_associated_images(self, mm):
                return {}

            def _write_tiled_level(self, tif, mm, level, *, reduced):
                raise RuntimeError("expected failure")

        with tempfile.TemporaryDirectory() as temporary_dir:
            root = Path(temporary_dir)
            input_path = root / "input.sdpc"
            input_path.write_bytes(b"non-empty")
            output_path = root / "output.svs"
            slide = replace(make_slide(512, 512), path=input_path)

            with self.assertRaisesRegex(RuntimeError, "expected failure"):
                FailingWriter(slide).write(output_path, skip_associated=False)

            self.assertFalse(output_path.exists())
            self.assertEqual(list(root.iterdir()), [input_path])


if __name__ == "__main__":
    unittest.main()
