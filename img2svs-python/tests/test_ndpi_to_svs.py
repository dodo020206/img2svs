from __future__ import annotations

import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace

from img2svs.converters.ndpi_to_svs import (
    NdpiSlide,
    PyramidLevel,
    SlideMetadata,
    SvsWriter,
)


class _FakeVipsImage:
    def __init__(self) -> None:
        self.save_kwargs: dict[str, object] | None = None
        self.fields: dict[str, object] = {}

    def copy(self):
        return self

    def set_type(self, _field_type, field: str, value: object) -> None:
        self.fields[field] = value

    def tiffsave(self, _path: str, **kwargs: object) -> None:
        self.save_kwargs = kwargs


class NdpiWriterTests(unittest.TestCase):
    def test_pyramid_write_is_streaming_and_does_not_materialize_reduced_levels(self) -> None:
        with tempfile.TemporaryDirectory() as temporary_dir:
            root = Path(temporary_dir)
            source = root / "slide.ndpi"
            source.write_bytes(b"source")
            level = PyramidLevel(
                index=0,
                width=50_000,
                height=90_000,
                downsample=1.0,
                tile_cols=196,
                tile_rows=352,
            )
            slide = NdpiSlide(
                path=source,
                metadata=SlideMetadata(
                    width=level.width,
                    height=level.height,
                    mpp=0.25,
                    app_mag=20,
                    jpeg_quality=85,
                ),
                tile_size=256,
                levels=(level,),
                associated_images=(),
            )
            image = _FakeVipsImage()
            writer = object.__new__(SvsWriter)
            writer.slide = slide
            writer.pyvips = SimpleNamespace(GValue=SimpleNamespace(gstr_type="gstr"))
            writer.resolution = 4_000.0
            writer._open_level_image = lambda _index: image

            writer._write_pyramid_with_vips(root / "slide.svs")

            assert image.save_kwargs is not None
            self.assertTrue(image.save_kwargs["pyramid"])
            self.assertTrue(image.save_kwargs["tile"])
            self.assertEqual(image.save_kwargs["tile_width"], 256)
            self.assertEqual(image.save_kwargs["tile_height"], 256)


if __name__ == "__main__":
    unittest.main()
