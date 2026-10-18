#!/usr/bin/env python3
"""Генератор иконки Stille: тёмный скруглённый квадрат, пузырь акцентом.

Пишет assets/icon.rgba (сырой RGBA 256×256 для window::Icon) и
docs/icon.png (для README). Только stdlib. Запуск: python3 tools/gen_icon.py
"""
import struct
import zlib
from pathlib import Path

SIZE = 256
SS = 4  # суперсэмплинг: SS×SS сэмплов на пиксель
BG = (0x14, 0x17, 0x1B)      # palette::BG
ACCENT = (0x4C, 0xC2, 0xFF)  # palette::ACCENT


def in_rounded(x: float, y: float, x0: float, y0: float, x1: float, y1: float, r: float) -> bool:
    if x < x0 + r and y < y0 + r:
        cx, cy = x0 + r, y0 + r
    elif x > x1 - r and y < y0 + r:
        cx, cy = x1 - r, y0 + r
    elif x < x0 + r and y > y1 - r:
        cx, cy = x0 + r, y1 - r
    elif x > x1 - r and y > y1 - r:
        cx, cy = x1 - r, y1 - r
    else:
        return x0 <= x <= x1 and y0 <= y <= y1
    return (x - cx) ** 2 + (y - cy) ** 2 <= r * r


def in_circle(x: float, y: float, cx: float, cy: float, r: float) -> bool:
    return (x - cx) ** 2 + (y - cy) ** 2 <= r * r


def in_triangle(x: float, y: float, a, b, c) -> bool:
    def sign(px, py, q1, q2):
        return (px - q2[0]) * (q1[1] - q2[1]) - (q1[0] - q2[0]) * (py - q2[1])
    d1, d2, d3 = sign(x, y, a, b), sign(x, y, b, c), sign(x, y, c, a)
    neg = d1 < 0 or d2 < 0 or d3 < 0
    pos = d1 > 0 or d2 > 0 or d3 > 0
    return not (neg and pos)


def sample(x: float, y: float):
    """Цвет точки в координатах 0..SIZE."""
    if not in_rounded(x, y, 0, 0, SIZE, SIZE, 56):
        return (0, 0, 0, 0)  # прозрачные углы
    # Пузырь с хвостиком и тремя точками («печатает»)
    if in_rounded(x, y, 46, 62, 210, 170, 30):
        return (*ACCENT, 255)
    if in_triangle(x, y, (86, 168), (124, 168), (96, 202)):
        return (*ACCENT, 255)
    for cx in (86, 128, 170):
        if in_circle(x, y, cx, 116, 11):
            return (*BG, 255)
    return (*BG, 255)


def render():
    n = SIZE * SS
    out = bytearray()
    for py in range(SIZE):
        for px in range(SIZE):
            r = g = b_ = a = 0
            for sy in range(SS):
                for sx in range(SS):
                    c = sample(px * SS + sx + 0.5, py * SS + sy + 0.5)
                    r += c[0]; g += c[1]; b_ += c[2]; a += c[3]
            k = SS * SS
            out += bytes((r // k, g // k, b_ // k, a // k))
    return bytes(out)


def png(width: int, height: int, rgba: bytes) -> bytes:
    def chunk(typ: bytes, data: bytes) -> bytes:
        return (
            struct.pack(">I", len(data)) + typ + data
            + struct.pack(">I", zlib.crc32(typ + data) & 0xFFFFFFFF)
        )
    stride = width * 4
    raw = b"".join(b"\x00" + rgba[y * stride:(y + 1) * stride] for y in range(height))
    ihdr = struct.pack(">IIBBBBB", width, height, 8, 6, 0, 0, 0)
    return b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", ihdr) + chunk(b"IDAT", zlib.compress(raw, 9)) + chunk(b"IEND", b"")


def main():
    rgba = render()
    assert len(rgba) == SIZE * SIZE * 4
    Path("assets/icon.rgba").write_bytes(rgba)
    Path("docs/icon.png").write_bytes(png(SIZE, SIZE, rgba))
    print(f"icon: {SIZE}x{SIZE}, rgba={len(rgba)} B")


if __name__ == "__main__":
    main()
