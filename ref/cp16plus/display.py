"""显示：把功率矩阵渲染成瀑布样张（PNG）。

渲染参数在协议里是"接收链路的最后一环"，所以这里的映射就是自动调参的目标形态：
全帧噪声底归一 + 固定 dB 窗 + gamma + 可选去斑。
"""

from __future__ import annotations

from pathlib import Path

import numpy as np

CMAP = np.array([
    [4, 4, 10], [10, 8, 30], [22, 14, 60], [46, 20, 100],
    [92, 26, 128], [150, 40, 122], [210, 78, 92], [245, 132, 70],
    [255, 190, 110], [255, 240, 200], [255, 255, 255],
], dtype=float)


def normalize(P: np.ndarray, lo_db: float = 6.0, hi_db: float = 18.0,
              gamma: float = 0.9) -> np.ndarray:
    """噪声底归一 + 固定 dB 窗。

    无噪（理想信道）时"关"的格子是数值 0，分位底噪会塌到 1e-30，整幅图直接饱和；
    因此底噪至少要压到峰值以下 60 dB。
    """
    pmax = float(np.max(P)) if P.size else 1.0
    floor = float(np.percentile(P, 20))
    if floor < pmax * 1e-8:
        floor = float(np.percentile(P, 40))
    floor = max(floor, pmax * 1e-6, 1e-30)
    z = 10 * np.log10(np.asarray(P, float) / floor + 1e-12)
    return np.clip((z - lo_db) / (hi_db - lo_db), 0, 1) ** gamma


def despeckle(img: np.ndarray, thr: float = 0.45) -> np.ndarray:
    """只删孤立亮点：8 邻域都没有"墨"的点判为噪声。"""
    b = (img > thr)
    pad = np.pad(b, 1)
    nb = np.zeros_like(b, dtype=int)
    h, w = b.shape
    for dy in (-1, 0, 1):
        for dx in (-1, 0, 1):
            if dy or dx:
                nb += pad[1 + dy:1 + dy + h, 1 + dx:1 + dx + w]
    out = img.copy()
    out[b & (nb == 0)] = 0.0
    return out


def colorize(img01: np.ndarray) -> np.ndarray:
    pos = np.clip(img01, 0, 1) * (len(CMAP) - 1)
    lo = np.floor(pos).astype(int)
    hi = np.clip(np.ceil(pos).astype(int), 0, len(CMAP) - 1)
    frac = (pos - lo)[..., None]
    return (CMAP[lo] * (1 - frac) + CMAP[hi] * frac).astype(np.uint8)


def render(P: np.ndarray, lo_db: float = 6.0, hi_db: float = 18.0,
           gamma: float = 0.9, px_per_tone: int = 10, px_per_row: int = 5,
           clean: bool = False, gaps: bool = True) -> np.ndarray:
    img = normalize(P, lo_db, hi_db, gamma)
    if clean:
        img = despeckle(img)
    big = np.repeat(np.repeat(img, px_per_row, axis=0), px_per_tone, axis=1)
    rgb = colorize(big)
    if gaps:
        rgb = np.concatenate([rgb, np.zeros((rgb.shape[0], 6, 3), np.uint8)], axis=1)
    return rgb


def save_png(path: str | Path, rgb: np.ndarray) -> None:
    from PIL import Image

    Image.fromarray(rgb).save(str(path))
