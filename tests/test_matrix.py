"""档位 × 重复 × 分段 的矩阵测试。

    python tests/test_matrix.py

每个用例：合成 -> (可选加噪/加失真) -> 解码 -> 断言
  · 字数一致
  · 锚定方式为 cc
  · 干净信号位准确率 1.000；6 dB ≥ 0.95
"""

from __future__ import annotations

import sys
import time
from dataclasses import replace
from pathlib import Path

import numpy as np

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "ref"))

from cp16plus import channel as C
from cp16plus import params as P
from cp16plus import rx as R
from cp16plus import tx as T
from cp16plus.font import HzkFont, SystemFont

HZK = ROOT.parent / "字库(必须复制到C盘hzk目录下)" / "1.hzk"

FAILS: list[str] = []


def acc_of(res: R.RxResult, glyphs, repeat: int) -> float:
    accs = []
    for i, blk in enumerate(res.blocks):
        if i >= len(glyphs):
            break
        g = np.repeat(glyphs[i], repeat, axis=0)
        th = (blk.max() + np.median(blk)) / 2
        accs.append(float(((blk > th).astype(np.uint8) == g).mean()))
    return float(np.mean(accs)) if accs else 0.0


def run_case(font16, font24, name: str, mode: P.Mode, repeat: int, n_chars: int,
             seg: int, snr: float | None) -> tuple[bool, str]:
    font = font24 if mode.n == 24 else font16
    glyphs, _ = font.rasters("频谱书写通信试验测试频率"[:n_chars])
    t0 = time.perf_counter()
    mode_eff = replace(mode, repeat=repeat)
    x, meta = T.build(glyphs, mode_eff, seed=11, cells_per_segment=seg)
    y = x if snr is None else C.awgn(x, snr, seed=5)
    res = R.decode(y, P.FS)
    dt = time.perf_counter() - t0
    acc = acc_of(res, glyphs, repeat)
    anchor = "/".join(sorted({s.anchor for s in res.segments})) or "none"
    ok = (res.n_cells == len(glyphs) and anchor == "cc"
          and acc >= (0.999 if snr is None else 0.95))
    info = (f"{name:<22} R={repeat} seg={seg:<2} cells={res.n_cells:>2}/{len(glyphs):<2} "
            f"anchor={anchor:<5} acc={acc:.3f} {dt*1000:5.0f}ms")
    if not ok:
        FAILS.append(f"{name} R={repeat} seg={seg} cells={res.n_cells}/{len(glyphs)} "
                     f"anchor={anchor} acc={acc:.3f}")
    return ok, info


def main() -> int:
    font16 = HzkFont(HZK)
    font24 = SystemFont("SimHei", 24)
    print("档位矩阵（干净信号）")
    cases = [
        ("M1 高速", P.M1, 1, 6, 8),
        ("M2 默认", P.M2, 1, 6, 8),
        ("M4 ≈CP16", P.M4, 1, 6, 8),
        ("M8 弱信号", P.M8, 1, 6, 8),
        ("M2 R=2", P.M2, 2, 6, 8),
        ("M2 R=3", P.M2, 3, 6, 8),
        ("24-M4 清晰档", P.M24_4, 1, 6, 8),
        ("16N-M4 共存", P.M16N4, 1, 6, 8),
        ("M2 单字", P.M2, 1, 1, 8),
        ("M2 三段", P.M2, 1, 10, 4),
        ("M2 逐字分段", P.M2, 1, 5, 1),
    ]
    for name, mode, rep, n, seg in cases:
        ok, info = run_case(font16, font24, name, mode, rep, n, seg, None)
        print(("  [ok]   " if ok else "  [FAIL] ") + info)

    print("\n6 dB 下的同一批（检验抗噪）")
    for name, mode, rep, n, seg in cases:
        ok, info = run_case(font16, font24, name, mode, rep, n, seg, 6.0)
        print(("  [ok]   " if ok else "  [FAIL] ") + info)

    print(f"\n{'矩阵全部通过' if not FAILS else f'失败 {len(FAILS)} 项'}")
    for f in FAILS:
        print("  - " + f)
    return 1 if FAILS else 0


if __name__ == "__main__":
    sys.exit(main())
