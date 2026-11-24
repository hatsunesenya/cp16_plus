"""信噪比扫描（多次试验，含随机时钟偏差与频偏）。

    python tests/test_snr_sweep.py

输出：每个 SNR 下的 锚定率 / 字数正确率 / HDR 成功率 / 平均位准确率。
这是"人眼能读 → 机器也能锁"的量化依据。
"""

from __future__ import annotations

import json
import sys
from pathlib import Path

import numpy as np

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "ref"))

from cp16plus import channel as C
from cp16plus import params as P
from cp16plus import rx as R
from cp16plus import tx as T
from cp16plus.font import HzkFont

HZK = ROOT.parent / "字库(必须复制到C盘hzk目录下)" / "1.hzk"
SNRS = [-15, -12, -9, -6, -3, 0, 3, 6]
TRIALS = 12


def bit_acc(res: R.RxResult, glyphs) -> float:
    accs = []
    for i, blk in enumerate(res.blocks):
        if i >= len(glyphs):
            break
        th = (blk.max() + np.median(blk)) / 2
        accs.append(float(((blk > th).astype(np.uint8) == glyphs[i]).mean()))
    return float(np.mean(accs)) if accs else 0.0


def main() -> int:
    font = HzkFont(HZK)
    glyphs, _ = font.rasters("频谱书写通信")
    x, _meta = T.build(glyphs, P.M2, seed=17)

    rows = []
    print(f"M2 · {len(glyphs)} 字 · 每档 {TRIALS} 次 · 随机时钟(±100 ppm)与频偏(±30 Hz)")
    print(f"{'SNR':>5} {'锚定':>6} {'字数对':>7} {'HDR':>5} {'位准':>6}")
    for snr in SNRS:
        anchors = cells_ok = hdr_ok = 0
        accs = []
        for t in range(TRIALS):
            rng = np.random.default_rng(1000 + t)
            a = 1 + rng.uniform(-1e-4, 1e-4)
            b = rng.uniform(-30, 30)
            y = C.awgn(C.shift_freq(C.resample_time(x, a), b), snr, seed=200 + t)
            res = R.decode(y, P.FS)
            if res.segments and all(s.anchor == "cc" for s in res.segments):
                anchors += 1
            if res.n_cells == len(glyphs):
                cells_ok += 1
            if res.hdr is not None:
                hdr_ok += 1
            accs.append(bit_acc(res, glyphs))
        row = {"snr": snr,
               "anchor": anchors / TRIALS,
               "cells": cells_ok / TRIALS,
               "hdr": hdr_ok / TRIALS,
               "acc": float(np.mean(accs))}
        rows.append(row)
        print(f"{snr:>5} {row['anchor']*100:>5.0f}% {row['cells']*100:>6.0f}% "
              f"{row['hdr']*100:>4.0f}% {row['acc']:>6.3f}")

    out = ROOT / "tests" / "snr_sweep.json"
    out.write_text(json.dumps({"mode": "M2", "trials": TRIALS, "rows": rows},
                              ensure_ascii=False, indent=2), encoding="utf-8")
    print(f"\n-> {out.relative_to(ROOT)}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
