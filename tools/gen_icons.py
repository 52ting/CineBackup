#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
gen_icons.py — 生成 CineBackup 的占位图标（PNG / ICO / ICNS）

零依赖，只用标准库 zlib + struct 手写 PNG，方便在没有 Pillow 的机器上跑。

正式发布前请用自家 logo 覆盖：
    npm run tauri icon ./your-logo-1024.png

用法：
    python tools/gen_icons.py            # 写入 ../src-tauri/icons/
"""

import os
import struct
import sys
import zlib

# 品牌配色
BG = (0x2F, 0x6F, 0xEB, 0xFF)   # 主蓝
FG = (0xFF, 0xFF, 0xFF, 0xFF)   # 白
TRANSPARENT = (0, 0, 0, 0)


# ---------------------------------------------------------------- 图形

def in_round_rect(u: float, v: float, rad: float = 0.18, margin: float = 0.045) -> bool:
    """标准化坐标 (u, v) 是否落在圆角矩形内"""
    x0, y0, x1, y1 = margin, margin, 1.0 - margin, 1.0 - margin
    if u < x0 or u > x1 or v < y0 or v > y1:
        return False
    cx = min(max(u, x0 + rad), x1 - rad)
    cy = min(max(v, y0 + rad), y1 - rad)
    dx, dy = u - cx, v - cy
    return dx * dx + dy * dy <= rad * rad


def in_arrow(u: float, v: float) -> bool:
    """白色的「写入 / 落盘」箭头"""
    # 竖杆
    if 0.455 <= u <= 0.545 and 0.20 <= v <= 0.60:
        return True
    # 箭头三角
    if 0.56 <= v <= 0.78:
        t = (v - 0.56) / (0.78 - 0.56)
        if abs(u - 0.5) <= 0.215 * (1.0 - t):
            return True
    # 底座
    if 0.84 <= v <= 0.90 and 0.235 <= u <= 0.765:
        return True
    return False


def render_rgba(size: int) -> bytes:
    """返回 size*size*4 的原始 RGBA 数据（每行前面多一个 filter 字节由 png() 加）"""
    rows = bytearray()
    for y in range(size):
        rows.append(0)  # PNG filter type 0 (None)
        v = (y + 0.5) / size
        for x in range(size):
            u = (x + 0.5) / size
            if not in_round_rect(u, v):
                rows += bytes(TRANSPARENT)
            elif in_arrow(u, v):
                rows += bytes(FG)
            else:
                rows += bytes(BG)
    return bytes(rows)


# ---------------------------------------------------------------- PNG

def _chunk(tag: bytes, data: bytes) -> bytes:
    body = tag + data
    return struct.pack(">I", len(data)) + body + struct.pack(">I", zlib.crc32(body) & 0xFFFFFFFF)


def make_png(size: int, rgba_rows: bytes) -> bytes:
    ihdr = struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0)  # 8bit RGBA
    return (
        b"\x89PNG\r\n\x1a\n"
        + _chunk(b"IHDR", ihdr)
        + _chunk(b"IDAT", zlib.compress(rgba_rows, 9))
        + _chunk(b"IEND", b"")
    )


# ---------------------------------------------------------------- ICO

def make_ico(items) -> bytes:
    """items: [(size, png_bytes)]，PNG 内嵌式 ICO"""
    n = len(items)
    header = struct.pack("<HHH", 0, 1, n)
    offset = 6 + 16 * n
    entries = b""
    blob = b""
    for size, png in items:
        w = 0 if size >= 256 else size
        entries += struct.pack("<BBBBHHII", w, w, 0, 0, 1, 32, len(png), offset)
        offset += len(png)
        blob += png
    return header + entries + blob


# ---------------------------------------------------------------- ICNS

def make_icns(items) -> bytes:
    """items: [(type4_bytes, png_bytes)]"""
    body = b""
    for tag, png in items:
        body += tag + struct.pack(">I", len(png) + 8) + png
    return b"icns" + struct.pack(">I", len(body) + 8) + body


# ---------------------------------------------------------------- main

def main():
    here = os.path.dirname(os.path.abspath(__file__))
    out = os.path.abspath(os.path.join(here, "..", "src-tauri", "icons"))
    os.makedirs(out, exist_ok=True)

    need = [16, 32, 48, 64, 128, 256, 512]
    print("渲染图标尺寸：", need)
    pngs = {}
    for s in need:
        pngs[s] = make_png(s, render_rgba(s))
        print(f"  {s}x{s}  {len(pngs[s])} bytes")

    # ---- Tauri 默认引用的文件名 ----
    files = {
        "32x32.png": pngs[32],
        "128x128.png": pngs[128],
        "128x128@2x.png": pngs[256],
        "icon.png": pngs[512],
    }
    # Windows Store / 通用
    for s in need:
        if s in (30, 44, 71, 89, 107, 142, 150, 284, 310):
            continue
    files["Square30x30Logo.png"] = pngs[32]
    files["Square44x44Logo.png"] = pngs[48]
    files["Square71x71Logo.png"] = pngs[64]
    files["Square89x89Logo.png"] = pngs[128]
    files["Square107x107Logo.png"] = pngs[128]
    files["Square142x142Logo.png"] = pngs[128]
    files["Square150x150Logo.png"] = pngs[256]
    files["Square284x284Logo.png"] = pngs[256]
    files["Square310x310Logo.png"] = pngs[512]
    files["StoreLogo.png"] = pngs[48]

    # ---- ico ----
    files["icon.ico"] = make_ico([(s, pngs[s]) for s in (16, 32, 48, 64, 128, 256)])

    # ---- icns（macOS：128 / 256 / 512 三档足够）----
    files["icon.icns"] = make_icns([
        (b"ic07", pngs[128]),   # 128x128
        (b"ic08", pngs[256]),   # 256x256
        (b"ic09", pngs[512]),   # 512x512
        (b"ic11", pngs[32]),    # 16x16@2x
        (b"ic12", pngs[64]),    # 32x32@2x
        (b"ic13", pngs[256]),   # 128x128@2x
        (b"ic14", pngs[512]),   # 256x256@2x
    ])

    for name, data in files.items():
        with open(os.path.join(out, name), "wb") as fh:
            fh.write(data)
        print(f"写入 {name}  ({len(data)} bytes)")

    print("\n完成 →", out)
    print("正式发布前请执行：npm run tauri icon ./your-logo-1024.png")


if __name__ == "__main__":
    sys.exit(main())
