#!/usr/bin/env python3
"""Generate AsterFiles' Windows ICO deterministically from its PNG source."""

from __future__ import annotations

import argparse
import io
import json
import struct
from pathlib import Path

from PIL import Image

SIZES = (16, 20, 24, 32, 40, 48, 64, 96, 128, 256)
PNG_SIGNATURE = b"\x89PNG\r\n\x1a\n"


def transparent_corners(image: Image.Image) -> bool:
    width, height = image.size
    return all(
        image.getpixel(point)[3] == 0
        for point in ((0, 0), (width - 1, 0), (0, height - 1), (width - 1, height - 1))
    )


def make_png_frame(source: Image.Image, size: int) -> bytes:
    frame = source.resize((size, size), Image.Resampling.LANCZOS)
    red, green, blue, alpha = frame.split()
    # Lanczos can introduce nearly invisible alpha at transparent outer corners.
    alpha = alpha.point(lambda value: 0 if value <= 3 else value)
    frame = Image.merge("RGBA", (red, green, blue, alpha))
    output = io.BytesIO()
    frame.save(output, format="PNG", optimize=True)
    return output.getvalue()


def build_ico(source: Image.Image) -> bytes:
    frames = [make_png_frame(source, size) for size in SIZES]
    offset = 6 + 16 * len(frames)
    entries = []
    for size, payload in zip(SIZES, frames, strict=True):
        dimension = 0 if size == 256 else size
        entries.append(
            struct.pack(
                "<BBBBHHII",
                dimension,
                dimension,
                0,
                0,
                1,
                32,
                len(payload),
                offset,
            )
        )
        offset += len(payload)
    return struct.pack("<HHH", 0, 1, len(frames)) + b"".join(entries) + b"".join(frames)


def validate_ico(data: bytes) -> list[dict[str, object]]:
    reserved, image_type, count = struct.unpack_from("<HHH", data)
    if (reserved, image_type, count) != (0, 1, len(SIZES)):
        raise ValueError("ICO header or image count is invalid")

    result = []
    for index, expected_size in enumerate(SIZES):
        entry = 6 + 16 * index
        width, height, _, _, planes, depth, length, offset = struct.unpack_from(
            "<BBBBHHII", data, entry
        )
        actual_size = 256 if width == 0 else width
        actual_height = 256 if height == 0 else height
        payload = data[offset : offset + length]
        if actual_size != expected_size or actual_height != expected_size:
            raise ValueError(f"ICO entry {index} has an unexpected size")
        if planes != 1 or depth != 32 or not payload.startswith(PNG_SIGNATURE):
            raise ValueError(f"ICO entry {expected_size}px is not a 32-bit PNG")
        frame = Image.open(io.BytesIO(payload)).convert("RGBA")
        if not transparent_corners(frame):
            raise ValueError(f"ICO entry {expected_size}px has a non-transparent corner")
        result.append({"size": expected_size, "encoding": "PNG", "corner_alpha": 0})
    return result


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--source", type=Path, default=Path("assets/app-icon.png"))
    parser.add_argument("--output", type=Path, default=Path("assets/windows/asterfiles.ico"))
    args = parser.parse_args()

    with Image.open(args.source) as opened:
        has_alpha = "A" in opened.getbands() or "transparency" in opened.info
        source = opened.convert("RGBA")
    if source.width != source.height or source.width < 256:
        raise ValueError("source icon must be square and at least 256x256")
    if not has_alpha:
        raise ValueError("source icon must contain an alpha channel")
    if not transparent_corners(source):
        raise ValueError("source icon must have fully transparent corners")

    ico = build_ico(source)
    entries = validate_ico(ico)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_bytes(ico)
    print(
        json.dumps(
            {
                "source": str(args.source),
                "output": str(args.output),
                "source_size": list(source.size),
                "output_bytes": len(ico),
                "entries": entries,
            },
            ensure_ascii=False,
            indent=2,
        )
    )


if __name__ == "__main__":
    main()