"""CP16+ 图标终稿：B 方案（瀑布行 + 点阵字），不要加号。

思路：
  · 左半：若干条"瀑布行"，越往下越亮、越宽 —— 这是用户看到的那张瀑布；
  · 右半：点阵字块，5×5 网格拼一个"中"字（点阵图形＝协议本体）；
  · 小尺寸（16/20/24）退化成"两三条短横 + 一个亮点方块"，保证不糊。
"""
import io
import math
import os
import struct

from PIL import Image, ImageDraw

OUT = os.path.join(os.path.dirname(__file__), '..', 'rust', 'assets')
os.makedirs(OUT, exist_ok=True)

SS = 6
BG_TOP = (10, 25, 33)
BG_BOT = (17, 48, 57)
EDGE = (34, 78, 90)
TEAL_LO = (34, 128, 144)
TEAL_HI = (122, 238, 220)
MINT = (208, 255, 245)
DIM = (28, 66, 78)

# 5×5 的"中"
GLYPH = [
    (1, 0), (2, 0), (3, 0),
    (1, 1), (3, 1),
    (1, 2), (2, 2), (3, 2),
    (1, 3), (3, 3),
    (1, 4), (2, 4), (3, 4),
]


def lerp(a, b, t):
    t = max(0.0, min(1.0, t))
    return tuple(int(round(a[i] + (b[i] - a[i]) * t)) for i in range(3))


def canvas(size):
    img = Image.new('RGBA', (size, size), (0, 0, 0, 0))
    grad = Image.new('RGBA', (size, size))
    gd = ImageDraw.Draw(grad)
    for y in range(size):
        gd.line([(0, y), (size, y)], fill=lerp(BG_TOP, BG_BOT, y / max(1, size - 1)) + (255,))
    mask = Image.new('L', (size, size), 0)
    ImageDraw.Draw(mask).rounded_rectangle([0, 0, size - 1, size - 1],
                                           radius=int(size * 0.22), fill=255)
    img.paste(grad, (0, 0), mask)
    ImageDraw.Draw(img).rounded_rectangle([0.5, 0.5, size - 1.5, size - 1.5],
                                          radius=int(size * 0.22),
                                          outline=EDGE + (150,), width=max(1, size // 90))
    return img


def make(size):
    b = size * SS
    img = canvas(b)
    d = ImageDraw.Draw(img)

    if size <= 24:
        # 小尺寸：两条短横 + 一个亮方块（方块别太高，否则会像锤子）
        for i, (w, y) in enumerate([(0.24, 0.34), (0.40, 0.55)]):
            y0 = b * y
            d.rounded_rectangle([b * .19, y0, b * .19 + b * w, y0 + b * .12],
                                radius=b * .05, fill=lerp(TEAL_LO, TEAL_HI, 0.35 + 0.5 * i) + (255,))
        d.rounded_rectangle([b * .63, b * .33, b * .84, b * .70], radius=b * .06, fill=MINT + (255,))
    else:
        # 左：瀑布行（宽度有起伏、亮度递增）
        rows = 9 if size < 48 else 11
        top, bottom = b * .20, b * .80
        step = (bottom - top) / rows
        for i in range(rows):
            y = top + i * step
            w = b * .30 * (0.52 + 0.48 * (0.5 + 0.5 * math.sin(i * 1.35 + 0.6)))
            col = lerp(TEAL_LO, TEAL_HI, i / max(1, rows - 1))
            h = step * 0.58
            d.rounded_rectangle([b * .15, y, b * .15 + w, y + h], radius=h * 0.45, fill=col + (255,))

        # 右：点阵字块
        n = 5
        if size <= 40:
            gx, gy, side = b * .56, b * .26, b * .34
        else:
            gx, gy, side = b * .55, b * .24, b * .38
        cell = side / n
        pad = cell * 0.16
        for r in range(n):
            for c in range(n):
                x = gx + c * cell
                y = gy + r * cell
                lit = (c, r) in GLYPH
                if not lit:
                    # 不画未点亮的格子：点阵的空由背景承担，加了反而像网格噪点
                    continue
                d.rounded_rectangle([x + pad, y + pad, x + cell - pad, y + cell - pad],
                                    radius=cell * .22, fill=MINT + (255,))

    return img.resize((size, size), Image.LANCZOS)


SIZES = [16, 20, 24, 32, 40, 48, 64, 128, 256]


def write_ico(path, sizes):
    imgs = [make(s) for s in sizes]
    pngs = []
    for im in imgs:
        buf = io.BytesIO()
        im.save(buf, format='PNG', optimize=True)
        pngs.append(buf.getvalue())
    header = struct.pack('<HHH', 0, 1, len(sizes))
    offset = 6 + 16 * len(sizes)
    entries = b''
    for s, data in zip(sizes, pngs):
        entries += struct.pack('<BBBBHHII', 0 if s >= 256 else s, 0 if s >= 256 else s,
                               0, 0, 1, 32, len(data), offset)
        offset += len(data)
    with open(path, 'wb') as f:
        f.write(header + entries + b''.join(pngs))
    return imgs


imgs = write_ico(os.path.join(OUT, 'cp16plus.ico'), SIZES)
print('ico 写入', os.path.getsize(os.path.join(OUT, 'cp16plus.ico')), 'bytes，尺寸', SIZES)

imgs[-1].save(os.path.join(OUT, 'cp16plus-256.png'))
imgs[5].save(os.path.join(OUT, 'cp16plus-48.png'))   # 48：任务栏
imgs[0].save(os.path.join(OUT, 'cp16plus-16.png'))

# 预览：浅底 + 深底各一排（模拟资源管理器和暗色任务栏）
pad = 18
w = sum(s + pad for s in SIZES) + pad
prev = Image.new('RGBA', (w, 2 * 290 + 30), (250, 251, 252, 255))
d = ImageDraw.Draw(prev)
d.rectangle([0, 0, w, 290], fill=(250, 251, 252, 255))
d.rectangle([0, 290, w, prev.size[1]], fill=(32, 34, 38, 255))
x = pad
for im in imgs:
    prev.alpha_composite(im, (x, 290 - im.size[0] - pad))
    prev.alpha_composite(im, (x, 290 + 290 - im.size[0] - pad))
    x += im.size[0] + pad
prev.save(os.path.join(OUT, 'cp16plus-preview.png'))

# 放大 16/24/32 看像素
zoom = Image.new('RGBA', (400, 180), (250, 251, 252, 255))
x = 12
for s, k in [(16, 6), (24, 5), (32, 4)]:
    im = make(s).resize((s * k, s * k), Image.NEAREST)
    zoom.alpha_composite(im, (x, 12))
    x += s * k + 18
zoom.save(os.path.join(OUT, 'zoom.png'))
print('预览完成')
