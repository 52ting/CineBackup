# -*- coding: utf-8 -*-
"""把用户给的方形 logo 图（白底 + 居中图案）处理成 app 图标源图。

步骤：
1. 按「非白内容包围盒」裁成正方形（自动吃掉白边，留 ~6% 呼吸空间）
2. 加 macOS Big Sur 风格的圆角（半径 ≈ 边长 22.37%），圆角外透明
3. 输出 1024×1024 `tools/_icon_src.png`（喂给 `tauri icon`）
4. 输出 `public/logo.png`（256×256，界面顶栏用）

用法：
  python tools/make_icon.py "<源图路径>"
"""
import os
import sys

from PIL import Image, ImageDraw

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
ICON_SRC = os.path.join(ROOT, "tools", "_icon_src.png")
APP_LOGO = os.path.join(ROOT, "public", "logo.png")

# macOS Big Sur+ 图标圆角比例（圆角半径 / 边长）
CORNER_RATIO = 0.2237
# 内容四周留白比例
PADDING_RATIO = 0.06


def content_bbox(im: Image.Image, white_threshold: int = 240):
    """非白内容的包围盒（先用小图找，再换算回原尺寸，省时间）"""
    W, H = im.size
    probe = 400
    g = im.convert("L").resize((probe, probe))
    mask = g.point(lambda v: 255 if v < white_threshold else 0)
    bb = mask.getbbox()
    if not bb:
        return (0, 0, W, H)
    sx, sy = W / probe, H / probe
    l, t, r, b = bb
    return (int(l * sx), int(t * sy), int(r * sx), int(b * sy))


def main():
    if len(sys.argv) < 2:
        print("用法: python tools/make_icon.py <源图>")
        return 2
    src = sys.argv[1]
    im = Image.open(src).convert("RGB")
    W, H = im.size

    l, t, r, b = content_bbox(im)
    cw, ch = r - l, b - t
    cx, cy = (l + r) / 2, (t + b) / 2
    side = max(cw, ch) * (1 + PADDING_RATIO * 2)
    side = min(side, min(W, H))
    x0 = max(0, int(cx - side / 2))
    y0 = max(0, int(cy - side / 2))
    x1 = min(W, x0 + int(side))
    y1 = min(H, y0 + int(side))
    print(f"源图 {W}x{H} → 内容盒 ({l},{t},{r},{b}) → 裁切 ({x0},{y0},{x1},{y1})")
    crop = im.crop((x0, y0, x1, y1))

    # 缩到目标尺寸后再加圆角（先大后小，边缘更干净）
    base = crop.resize((1024, 1024), Image.LANCZOS)

    # 圆角遮罩：圆角外透明
    radius = int(1024 * CORNER_RATIO)
    mask = Image.new("L", (1024, 1024), 0)
    ImageDraw.Draw(mask).rounded_rectangle([0, 0, 1023, 1023], radius=radius, fill=255)
    out = Image.new("RGBA", (1024, 1024), (0, 0, 0, 0))
    out.paste(base, (0, 0), mask)

    os.makedirs(os.path.dirname(ICON_SRC), exist_ok=True)
    out.save(ICON_SRC)
    print(f"已生成图标源图: {ICON_SRC} ({os.path.getsize(ICON_SRC)} B)")

    # 界面顶栏用的小图（同样带圆角，缩小后不会糊成白块）
    os.makedirs(os.path.dirname(APP_LOGO), exist_ok=True)
    out.resize((256, 256), Image.LANCZOS).save(APP_LOGO)
    print(f"已生成界面 logo: {APP_LOGO} ({os.path.getsize(APP_LOGO)} B)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
