from __future__ import annotations

import io
import struct
import tempfile
import unittest
from pathlib import Path

import numpy
import tifffile
from PIL import Image

from img2svs.app import convert_to_svs
from img2svs.app.svs_gui_service import (
    GuiConversionOptions,
    execute_jobs_subprocess,
    plan_jobs,
)
from img2svs.converters.dmetrix_to_svs import DmetrixParser, convert_one


def _encoded_image(
    size: tuple[int, int], color: tuple[int, int, int], image_format: str
) -> bytes:
    buffer = io.BytesIO()
    Image.new("RGB", size, color).save(
        buffer,
        format=image_format,
        quality=75 if image_format == "JPEG" else None,
    )
    return buffer.getvalue()


def make_dmetrix(path: Path) -> None:
    low_tiles = [((0, 0), _encoded_image((12, 11), (120, 120, 120), "JPEG"))]
    high_tiles = [
        ((0, 0), _encoded_image((16, 16), (240, 10, 10), "JPEG")),
        ((0, 1), _encoded_image((16, 5), (10, 240, 10), "JPEG")),
        ((1, 0), _encoded_image((7, 16), (10, 10, 240), "JPEG")),
        ((1, 1), _encoded_image((7, 5), (240, 240, 10), "JPEG")),
    ]
    label = _encoded_image((9, 13), (220, 80, 150), "BMP")
    macro = _encoded_image((11, 8), (80, 150, 220), "BMP")

    first_index_offset = 320
    second_index_offset = first_index_offset + 22
    data_offset = 512
    blobs: list[tuple[int, bytes]] = []

    def allocate(data: bytes) -> tuple[int, int]:
        nonlocal data_offset
        result = (data_offset, len(data))
        blobs.append((data_offset, data))
        data_offset += len(data)
        return result

    label_range = allocate(label)
    macro_range = allocate(macro)
    low_ranges = [(xy, allocate(data)) for xy, data in low_tiles]
    high_ranges = [(xy, allocate(data)) for xy, data in high_tiles]

    payload = bytearray(data_offset)
    payload[:8] = b"DmetrixN"
    struct.pack_into("<d", payload, 0x30, 0.25)
    struct.pack_into("<d", payload, 0x38, 0.26)
    struct.pack_into("<I", payload, 0x40, 20)
    struct.pack_into("<HIII", payload, 0xC2, 9, 0, 0, first_index_offset)
    struct.pack_into("<HIII", payload, 0xC2 + 14, 10, 1, 1, second_index_offset)
    struct.pack_into("<HIII", payload, 0xC2 + 28, 11, 0, 0, 0)

    associated_offset = first_index_offset - 44
    struct.pack_into("<HIIQI", payload, associated_offset, 0xFFFF, 0, 0, *label_range)
    struct.pack_into("<HIIQI", payload, associated_offset + 22, 0xFFFE, 0, 0, *macro_range)

    for source_id, index_offset, records in (
        (9, first_index_offset, low_ranges),
        # Deliberately column-major to verify the parser normalizes to row-major.
        (10, second_index_offset, high_ranges),
    ):
        for record_index, ((x, y), (offset, length)) in enumerate(records):
            struct.pack_into(
                "<HIIQI",
                payload,
                index_offset + record_index * 22,
                source_id,
                x,
                y,
                offset,
                length,
            )
    for offset, data in blobs:
        payload[offset : offset + len(data)] = data
    path.write_bytes(payload)


class DmetrixConversionTests(unittest.TestCase):
    def test_parser_reads_geometry_metadata_and_row_major_tile_order(self) -> None:
        with tempfile.TemporaryDirectory() as temporary_dir:
            source = Path(temporary_dir) / "slide.dmetrix"
            make_dmetrix(source)
            slide = DmetrixParser(source).parse()

            self.assertEqual(slide.tile_size, 16)
            self.assertEqual(slide.metadata.width, 23)
            self.assertEqual(slide.metadata.height, 21)
            self.assertAlmostEqual(slide.metadata.mpp, 0.255)
            self.assertEqual(slide.metadata.app_mag, 20)
            self.assertEqual(slide.metadata.jpeg_quality, 75)
            self.assertEqual(
                [(level.source_id, level.width, level.height) for level in slide.levels],
                [(10, 23, 21), (9, 12, 11)],
            )
            self.assertEqual(
                [entry.kind for entry in slide.associated_images], ["label", "macro"]
            )

            source_ranges = slide.level_tiles[0]
            with source.open("rb") as fh:
                colors = []
                for data_range in source_ranges:
                    fh.seek(data_range.offset)
                    colors.append(
                        numpy.asarray(
                            Image.open(io.BytesIO(fh.read(data_range.length))).convert("RGB")
                        )[0, 0]
                    )
            self.assertGreater(colors[0][0], colors[0][1])
            self.assertGreater(colors[1][2], colors[1][0])
            self.assertGreater(colors[2][1], colors[2][0])

    def test_unified_dispatch_and_svs_output(self) -> None:
        with tempfile.TemporaryDirectory() as temporary_dir:
            root = Path(temporary_dir)
            source = root / "slide.dmetrix"
            output = root / "slide.svs"
            make_dmetrix(source)

            self.assertEqual(convert_to_svs.detect_backend(source, "auto"), "dmetrix")
            convert_one(source, output, None, skip_associated=False, overwrite=False)

            self.assertTrue(output.exists())
            with tifffile.TiffFile(output) as tif:
                self.assertEqual(len(tif.pages), 5)
                self.assertEqual(tif.pages[0].shape, (21, 23, 3))
                self.assertEqual(tif.pages[2].shape, (11, 12, 3))
                self.assertIn("AppMag = 20", tif.pages[0].description)
                self.assertEqual(tif.pages[3].description, "label")
                self.assertEqual(tif.pages[4].description, "macro")
                image = tif.pages[0].asarray()
            self.assertGreater(image[2, 2, 0], image[2, 2, 1])
            self.assertGreater(image[2, 18, 2], image[2, 18, 0])
            self.assertGreater(image[18, 2, 1], image[18, 2, 0])

    def test_gui_worker_subprocess_converts_without_in_process_runner(self) -> None:
        with tempfile.TemporaryDirectory() as temporary_dir:
            root = Path(temporary_dir)
            source = root / "slide.dmetrix"
            output_dir = root / "out"
            make_dmetrix(source)
            options = GuiConversionOptions(
                inputs=(source,), output_dir=output_dir, overwrite=True
            )
            jobs = plan_jobs(options)
            logs: list[str] = []
            summary = execute_jobs_subprocess(jobs, options, log_callback=logs.append)

            self.assertEqual(summary.succeeded, 1)
            self.assertFalse(summary.failed)
            self.assertTrue((output_dir / "slide.svs").exists())
            self.assertTrue(any("Conversion completed." in message for message in logs))


if __name__ == "__main__":
    unittest.main()
