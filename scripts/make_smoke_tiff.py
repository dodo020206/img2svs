"""Write a small tiled JPEG TIFF, used to smoke-test the converter end to end.

The fixture has one full-resolution page of 256x256 JPEG tiles, which is the
layout the native `.tif` reader expects. Generating it here keeps the build and
CI pipelines free of any external imaging tool: the converter no longer ships
libvips, so `vips black` is no longer an option.

Requires Pillow. Run:  python scripts/make_smoke_tiff.py <out.tif> [width] [height]
"""

import io
import struct
import sys
from pathlib import Path

from PIL import Image

TILE = 256
# 96 dpi expressed as pixels per centimetre, i.e. 96 / 2.54.
RESOLUTION_PER_CM = 3780

TYPE_LONG = 4
TYPE_SHORT = 3
TYPE_RATIONAL = 5


def tile_payload(x: int, y: int) -> bytes:
    # A deterministic gradient with a checkerboard, so a tile that decodes to
    # the wrong place is visible rather than plausible.
    image = Image.new("RGB", (TILE, TILE))
    pixels = image.load()
    for row in range(TILE):
        for column in range(TILE):
            pixels[column, row] = (
                (x * TILE + column) % 256,
                (y * TILE + row) % 256,
                128 + 64 * ((column // 16 + row // 16) % 2),
            )
    buffer = io.BytesIO()
    image.save(buffer, format="JPEG", quality=80, subsampling=2)
    return buffer.getvalue()


def main():
    out = Path(sys.argv[1])
    width = int(sys.argv[2]) if len(sys.argv) > 2 else 512
    height = int(sys.argv[3]) if len(sys.argv) > 3 else 384

    columns = -(-width // TILE)
    rows = -(-height // TILE)
    payloads = [tile_payload(x, y) for y in range(rows) for x in range(columns)]

    entry_count = 15
    body_offset = 8
    offsets = []
    cursor = body_offset
    for payload in payloads:
        offsets.append(cursor)
        cursor += len(payload)
    body = b"".join(payloads)

    ifd_offset = body_offset + len(body)
    extras_offset = ifd_offset + 2 + entry_count * 12 + 4

    # Values wider than the four byte entry slot go after the IFD, in the order
    # they are placed here.
    extras = b""
    placements = {}

    def place(name: str, payload: bytes) -> int:
        nonlocal extras
        placements[name] = extras_offset + len(extras)
        extras += payload
        return placements[name]

    bits_offset = place("bits", struct.pack("<HHH", 8, 8, 8))
    offsets_at = place("offsets", struct.pack("<" + "I" * len(payloads), *offsets))
    counts_at = place(
        "counts", struct.pack("<" + "I" * len(payloads), *[len(item) for item in payloads])
    )
    x_resolution_at = place("xres", struct.pack("<II", RESOLUTION_PER_CM, 1))
    y_resolution_at = place("yres", struct.pack("<II", RESOLUTION_PER_CM, 1))

    def entry(tag: int, kind: int, count: int, value: int) -> bytes:
        return struct.pack("<HHII", tag, kind, count, value)

    entries = [
        entry(254, TYPE_LONG, 1, 0),  # NewSubfileType: full-resolution image
        entry(256, TYPE_LONG, 1, width),
        entry(257, TYPE_LONG, 1, height),
        entry(258, TYPE_SHORT, 3, bits_offset),
        entry(259, TYPE_LONG, 1, 7),  # Compression: JPEG
        entry(262, TYPE_LONG, 1, 6),  # PhotometricInterpretation: YCbCr
        entry(277, TYPE_LONG, 1, 3),
        entry(282, TYPE_RATIONAL, 1, x_resolution_at),
        entry(283, TYPE_RATIONAL, 1, y_resolution_at),
        entry(284, TYPE_LONG, 1, 1),
        entry(296, TYPE_LONG, 1, 3),  # ResolutionUnit: centimetres
        entry(322, TYPE_LONG, 1, TILE),
        entry(323, TYPE_LONG, 1, TILE),
        entry(324, TYPE_LONG, len(payloads), offsets_at),
        entry(325, TYPE_LONG, len(payloads), counts_at),
    ]
    assert len(entries) == entry_count

    header = b"II" + struct.pack("<H", 42) + struct.pack("<I", ifd_offset)
    ifd = struct.pack("<H", entry_count) + b"".join(entries) + struct.pack("<I", 0)
    out.write_bytes(header + body + ifd + extras)
    print(
        f"{out}: {width}x{height}, {columns}x{rows} tiles of {TILE}, "
        f"{out.stat().st_size:,} bytes"
    )


if __name__ == "__main__":
    main()
