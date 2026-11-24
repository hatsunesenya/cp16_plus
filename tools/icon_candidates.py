"""CP16+ 图标方案候选：A–F 六版，每版 256/48/16 三档。

全部程序化生成（无外部素材），每档独立绘制而非缩放，保证小尺寸不糊。
输出：
  rust/assets/candidates/<X>-256.png   单版大图
  rust/assets/candidates/sheet.png     对比总表
"""
import io
import math
import os

from PIL import Image, ImageDraw, ImageFont

OUT = os.path.join(os.path.dirname(__file__), '..', 'rust', 'assets', 'candidates')
os.makedirs(OUT, exist_ok=True)

SS = 6  # 超采样
BG_TOP = (10, 25, 33)
BG_BOT = (16, 46, 55)
EDGE = (34, 78, 90)
TEAL_LO = (40, 176, 190)
TEAL_HI = (122, 238, 220)
MINT = (210, 255, 246)
INK = (11, 22, 28)


def lerp(a, b, t):
    t = max(0.0, min(1.0, t))
    return tuple(int(round(a[i] + (b[i] - a[i]) * t)) for i in range(3))


def canvas(size, radius_frac=0.22, top=BG_TOP, bot=BG_BOT):
    img = Image.new('RGBA', (size, size), (0, 0, 0, 0))
    grad = Image.new('RGBA', (size, size))
    gd = ImageDraw.Draw(grad)
    for y in range(size):
        gd.line([(0, y), (size, y)], fill=lerp(top, bot, y / max(1, size - 1)) + (255,))
    mask = Image.new('L', (size, size), 0)
    ImageDraw.Draw(mask).rounded_rectangle([0, 0, size - 1, size - 1],
                                           radius=int(size * radius_frac), fill=255)
    img.paste(grad, (0, 0), mask)
    ImageDraw.Draw(img).rounded_rectangle(
        [0.5, 0.5, size - 1.5, size - 1.5], radius=int(size * radius_frac),
        outline=EDGE + (150,), width=max(1, size // 90))
    return img


def bars(img, fracs, left, right, top, bottom, widths=2.0, rf=0.5, colors=True):
    d = ImageDraw.Draw(img)
    s = img.size[0]
    n = len(fracs)
    usable = s - left - right
    unit = usable / (n * widths + (n - 1))
    bw, gap = unit * widths, unit
    base = s - bottom
    span = s - top - bottom
    for i, f in enumerate(fracs):
        x0 = left + i * (bw + gap)
        y0 = base - span * f
        col = lerp(TEAL_LO, TEAL_HI, i / max(1, n - 1)) if colors else TEAL_HI
        d.rounded_rectangle([x0, y0, x0 + bw, base], radius=max(1, int(bw * rf)), fill=col + (255,))


def plus(img, cx, cy, arm, thick, color=MINT):
    d = ImageDraw.Draw(img)
    d.rounded_rectangle([cx - arm, cy - thick / 2, cx + arm, cy + thick / 2], radius=thick / 2, fill=color + (255,))
    d.rounded_rectangle([cx - thick / 2, cy - arm, cx + thick / 2, cy + arm], radius=thick / 2, fill=color + (255,))


# ---------------------------------------------------------------- A 音调条 + 加号
def design_a(size):
    b = size * SS
    img = canvas(b)
    if size <= 20:
        bars(img, [0.46, 0.74, 1.0], b * .24, b * .20, b * .20, b * .22, widths=2.4)
    elif size <= 32:
        bars(img, [0.42, 0.60, 0.80, 1.0], b * .20, b * .18, b * .20, b * .22, widths=2.0)
    elif size <= 40:
        bars(img, [0.38, 0.54, 0.70, 0.86, 1.0], b * .18, b * .16, b * .20, b * .22, widths=1.8, rf=.45)
        plus(img, b * .80, b * .22, b * .085, b * .055)
    else:
        env = [min(1.0, (0.42 + 0.58 * (0.5 + 0.5 * math.sin(math.pi * (0.15 + 0.9 * i / 15)))) * (0.72 + 0.42 * i / 15))
               for i in range(16)]
        bars(img, env, b * .13, b * .12, b * .26, b * .30, widths=1.0, rf=.4)
        plus(img, b * .83, b * .22, b * .075, b * .048)
    return img.resize((size, size), Image.LANCZOS)


# ---------------------------------------------------------------- B 瀑布 + 字形块
def design_b(size):
    b = size * SS
    img = canvas(b)
    d = ImageDraw.Draw(img)
    if size <= 24:
        # 小尺寸：左三条短横线（瀑布）+ 右一个亮点块
        for i, f in enumerate([0.55, 0.75, 1.0]):
            y = b * (0.30 + i * 0.18)
            d.rounded_rectangle([b * .20, y, b * .20 + b * .34 * f, y + b * .10],
                                radius=b * .05, fill=lerp(TEAL_LO, TEAL_HI, i / 2) + (255,))
        d.rounded_rectangle([b * .62, b * .32, b * .84, b * .70], radius=b * .06, fill=MINT + (255,))
    else:
        # 左侧：瀑布行（越往下越亮）
        rows = 9
        for i in range(rows):
            y = b * .20 + i * b * .066
            w = b * .30 * (0.55 + 0.45 * math.sin(i * 1.1) ** 2)
            col = lerp((18, 60, 72), TEAL_HI, i / (rows - 1))
            d.rounded_rectangle([b * .14, y, b * .14 + w, y + b * .046], radius=b * .022, fill=col + (255,))
        # 右侧：点阵字块（4×4 亮点）
        gx, gy, cell, gap = b * .55, b * .26, b * .085, b * .028
        on = {(1, 0), (2, 0), (0, 1), (1, 1), (2, 1), (3, 1), (1, 2), (2, 2), (1, 3), (2, 3)}
        for r in range(4):
            for c in range(4):
                x = gx + c * (cell + gap)
                y = gy + r * (cell + gap)
                lit = (c, r) in on
                d.rounded_rectangle([x, y, x + cell, y + cell], radius=cell * .22,
                                    fill=(MINT if lit else (30, 70, 82)) + (255,))
        if size >= 48:
            plus(img, b * .30, b * .80, b * .075, b * .048)
    return img.resize((size, size), Image.LANCZOS)


# ---------------------------------------------------------------- C 点阵字（"十"）
def design_c(size):
    b = size * SS
    img = canvas(b)
    d = ImageDraw.Draw(img)
    n = 5
    pad = b * (0.16 if size > 24 else 0.14)
    cell = (b - 2 * pad) / n
    on = {(2, 0), (2, 1), (0, 2), (1, 2), (2, 2), (3, 2), (4, 2), (2, 3), (2, 4)}
    for r in range(n):
        for c in range(n):
            x, y = pad + c * cell, pad + r * cell
            gap = cell * (0.16 if size > 24 else 0.22)
            lit = (c, r) in on
            col = MINT if lit else (26, 62, 74)
            d.rounded_rectangle([x + gap / 2, y + gap / 2, x + cell - gap / 2, y + cell - gap / 2],
                                radius=cell * .2, fill=col + (255,))
    return img.resize((size, size), Image.LANCZOS)


# ---------------------------------------------------------------- D 频谱刻度尺 + 光标
def design_d(size):
    b = size * SS
    img = canvas(b)
    d = ImageDraw.Draw(img)
    n = 16 if size > 40 else (7 if size > 20 else 5)
    left, right = b * .14, b * .14
    base = b * .74
    usable = b - left - right
    unit = usable / (n * 1.6 + (n - 1))
    bw, gap = unit * 1.6, unit
    for i in range(n):
        t = i / max(1, n - 1)
        h = (b * .40) * (0.35 + 0.65 * abs(math.sin(math.pi * (0.12 + 0.8 * t))))
        x0 = left + i * (bw + gap)
        col = lerp(TEAL_LO, TEAL_HI, t)
        d.rounded_rectangle([x0, base - h, x0 + bw, base], radius=bw * .35, fill=col + (255,))
    # 基线
    d.rounded_rectangle([left * .9, base + b * .02, b - right * .9, base + b * .045],
                        radius=b * .01, fill=(120, 220, 214, 120))
    # 光标光柱（指到第 12 根）
    cx = left + int(n * 0.72) * (bw + gap) + bw / 2
    d.rounded_rectangle([cx - b * .012, b * .16, cx + b * .012, base], radius=b * .012, fill=MINT + (200,))
    return img.resize((size, size), Image.LANCZOS)


# ---------------------------------------------------------------- E "16" 字样 + 波
def design_e(size):
    b = size * SS
    img = canvas(b)
    d = ImageDraw.Draw(img)
    font_path = r'C:\Windows\Fonts\msyhbd.ttc'
    if size <= 24:
        try:
            f = ImageFont.truetype(font_path, int(b * 0.72))
            d.text((b / 2, b * 0.50), '16', font=f, anchor='mm', fill=MINT + (255,))
        except Exception:
            bars(img, [0.5, 0.8, 1.0], b * .22, b * .2, b * .2, b * .22, widths=2.2)
    else:
        try:
            f = ImageFont.truetype(font_path, int(b * 0.52))
            d.text((b / 2, b * 0.42), '16', font=f, anchor='mm', fill=MINT + (255,))
        except Exception:
            pass
        # 下面一条正弦波
        pts = []
        for i in range(0, 101):
            t = i / 100
            x = b * .18 + t * b * .64
            y = b * .74 + math.sin(t * math.pi * 2) * b * .055
            pts.append((x, y))
        d.line(pts, fill=lerp(TEAL_LO, TEAL_HI, 0.6) + (255,), width=max(2, int(b * .028)), joint='curve')
    return img.resize((size, size), Image.LANCZOS)


# ---------------------------------------------------------------- F 发射波纹
def design_f(size):
    b = size * SS
    img = canvas(b)
    d = ImageDraw.Draw(img)
    cx, cy = b * .5, b * .55
    for i, r in enumerate([0.16, 0.27, 0.38]):
        w = max(2, int(b * (0.030 - i * 0.004)))
        d.arc([cx - b * r, cy - b * r, cx + b * r, cy + b * r], start=205, end=335,
              fill=lerp(TEAL_HI, TEAL_LO, i / 2) + (255 - i * 40,), width=w)
    # 发射源：一个小方块
    h = b * .07
    d.rounded_rectangle([cx - h, cy - h, cx + h, cy + h], radius=h * .3, fill=MINT + (255,))
    # 顶部 16 个音的小刻度（大尺寸才加）
    if size >= 48:
        for i in range(16):
            x = b * .18 + i * b * .0427
            hh = b * (.020 + .014 * (i % 3))
            d.rounded_rectangle([x, b * .16, x + b * .012, b * .16 + hh], radius=b * .006,
                                fill=lerp(TEAL_LO, TEAL_HI, i / 15) + (220,))
    return img.resize((size, size), Image.LANCZOS)


DESIGNS = {'A': design_a, 'B': design_b, 'C': design_c, 'D': design_d, 'E': design_e, 'F': design_f}
NAMES = {
    'A': '音调条 + 加号',
    'B': '瀑布 + 点阵字',
    'C': '点阵字（十）',
    'D': '频谱刻度 + 光标',
    'E': '"16" + 波形',
    'F': '发射波纹',
}

for k, fn in DESIGNS.items():
    fn(256).save(os.path.join(OUT, f'{k}-256.png'))
    fn(48).save(os.path.join(OUT, f'{k}-48.png'))
    fn(16).save(os.path.join(OUT, f'{k}-16.png'))

# 总表：每行一版，列 = 256 / 128 / 48 / 32 / 16(×6)
try:
    font = ImageFont.truetype(r'C:\Windows\Fonts\msyh.ttc', 20)
    small = ImageFont.truetype(r'C:\Windows\Fonts\msyh.ttc', 15)
except Exception:
    font = small = ImageFont.load_default()

W, RH = 760, 200
sheet = Image.new('RGBA', (W, RH * len(DESIGNS) + 20), (247, 249, 250, 255))
d = ImageDraw.Draw(sheet)
for row, (k, fn) in enumerate(DESIGNS.items()):
    y = 10 + row * RH
    d.text((16, y + 78), k, font=font, fill=(20, 40, 48, 255))
    d.text((16, y + 108), NAMES[k], font=small, fill=(60, 90, 100, 255))
    x = 200
    for s, scale in [(128, 1), (48, 1), (32, 1), (16, 6)]:
        im = fn(s).resize((s * scale, s * scale), Image.NEAREST if scale > 1 else Image.LANCZOS)
        sheet.alpha_composite(im.convert('RGBA'), (x, y + (160 - im.size[1]) // 2))
        d.text((x, y + 168), f'{s}px' + ('×6' if scale > 1 else ''), font=small, fill=(120, 140, 148, 255))
        x += max(im.size[0], 60) + 24
    d.line([(200, y + RH - 8), (W - 20, y + RH - 8)], fill=(224, 232, 236, 255), width=1)

sheet.save(os.path.join(OUT, 'sheet.png'))
print('方案已生成:', os.path.join(OUT, 'sheet.png'))
