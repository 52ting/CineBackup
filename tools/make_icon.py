# -*- coding: utf-8 -*-
"""把用户给的方形 logo 图（白底 + 居中图案）处理成 app 图标源图。

步骤：
1. **紧贴内容**裁出图案（按「非白内容包围盒」，阈值 240）
2. 把图案**缩放**到图标边长的 CONTENT_RATIO（默认 66%，参考微信图标 ——
   白气泡约占图标宽度 64%，四周留白明显）
3. 贴到 1024×1024 白底画布正中
4. 加 macOS Big Sur 风格圆角（半径 ≈ 边长 22.37%），圆角外透明
5. 输出 `tools/_icon_src.png`（喂给 `tauri icon`）+ `public/logo.png`（界面顶栏用）

⚠️ 早期版本是「以内容为中心向外扩边裁切」，受**原图尺寸上限**约束：
原图只有 2953px、内容最大边 2186px，想要 66% 占比需要 3323px 的方形 → 被 clamp
到整图，结果图案实际占 74%。所以改成「先裁紧、再缩放、后居中」这套不受原图尺寸限制的做法。

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
# 图案占图标边长的比例（微信图标约 0.64，取 0.66 稍宽松一点）
CONTENT_RATIO = 0.66
# 画布底色（源图是白底，保持白底）
CANVAS_BG = (255, 255, 255)
SIZE = 1024


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


def build(im: Image.Image) -> Image.Image:
    """源图 → 1024×1024 圆角图标（图案占 CONTENT_RATIO）"""
    l, t, r, b = content_bbox(im)
    crop = im.crop((l, t, r, b))
    cw, ch = crop.size
    print(f"内容盒 ({l},{t},{r},{b}) → 图案 {cw}x{ch}")

    # 缩放，使图案的**最大边**等于目标占比
    target = int(SIZE * CONTENT_RATIO)
    scale = target / max(cw, ch)
    nw, nh = max(1, round(cw * scale)), max(1, round(ch * scale))
    crop = crop.resize((nw, nh), Image.LANCZOS)

    # 白底画布居中
    base = Image.new("RGB", (SIZE, SIZE), CANVAS_BG)
    base.paste(crop, ((SIZE - nw) // 2, (SIZE - nh) // 2))
    print(f"图案缩放到 {nw}x{nh}，占边长 {max(nw, nh) / SIZE * 100:.1f}%")

    # 圆角遮罩：圆角外透明
    radius = int(SIZE * CORNER_RATIO)
    mask = Image.new("L", (SIZE, SIZE), 0)
    ImageDraw.Draw(mask).rounded_rectangle([0, 0, SIZE - 1, SIZE - 1], radius=radius, fill=255)
    out = Image.new("RGBA", (SIZE, SIZE), (0, 0, 0, 0))
    out.paste(base, (0, 0), mask)
    return out


def main():
    if len(sys.argv) < 2:
        print("用法: python tools/make_icon.py <源图>")
        return 2
    im = Image.open(sys.argv[1]).convert("RGB")
    print(f"源图 {im.size[0]}x{im.size[1]}")
    out = build(im)

    os.makedirs(os.path.dirname(ICON_SRC), exist_ok=True)
    out.save(ICON_SRC)
    print(f"已生成图标源图: {ICON_SRC} ({os.path.getsize(ICON_SRC)} B)")

    # 界面顶栏用的小图（同样构图，缩小后与 Dock 图标一致）
    os.makedirs(os.path.dirname(APP_LOGO), exist_ok=True)
    out.resize((256, 256), Image.LANCZOS).save(APP_LOGO)
    print(f"已生成界面 logo: {APP_LOGO} ({os.path.getsize(APP_LOGO)} B)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
