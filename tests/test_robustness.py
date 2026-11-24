"""边界与鲁棒性测试：静音、纯噪声、截断、48 kHz、直流/削波、衰落、极限频偏。

    python tests/test_robustness.py

原则：**不能崩、不能挂**；能解就给出正确结果，解不出就明确 ok=False。
"""

from __future__ import annotations

import sys
import time
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
FAILS: list[str] = []


def check(name: str, cond: bool, detail: str = "") -> None:
    print(f"  [{'ok' if cond else 'FAIL'}] {name}" + (f"  {detail}" if detail else ""))
    if not cond:
        FAILS.append(name)


def main() -> int:
    font = HzkFont(HZK)
    glyphs, _ = font.rasters("频谱书写通信")
    x, meta = T.build(glyphs, P.M2, seed=3)
    truth = len(glyphs)

    print("异常输入")

    def safe(fn, label):
        try:
            t0 = time.perf_counter()
            out = fn()
            dt = time.perf_counter() - t0
            check(label, True, f"{dt*1000:.0f} ms")
            return out
        except Exception as e:  # noqa: BLE001
            check(label, False, f"抛异常 {type(e).__name__}: {e}")
            return None

    r = safe(lambda: R.decode(np.zeros(P.FS * 5, np.float32), P.FS), "全静音 5 s")
    check("静音不误判", r is not None and r.n_cells == 0 and not r.ok)

    rng = np.random.default_rng(1)
    r = safe(lambda: R.decode(rng.normal(0, 0.1, P.FS * 5).astype(np.float32), P.FS),
             "纯噪声 5 s")
    check("噪声不误判", r is not None and r.n_cells <= 2, f"cells={r.n_cells if r else '-'}")

    r = safe(lambda: R.decode(x[: len(x) // 2], P.FS), "截断到一半")
    check("截断可解出前几字", r is not None and 2 <= r.n_cells <= truth,
          f"cells={r.n_cells if r else '-'}")

    r = safe(lambda: R.decode(x[: meta.bursts[0].start + 100], P.FS), "只有 HDR 头")
    check("半个突发不崩", r is not None)

    print("\n采样率与电平")

    from cp16plus.wavio import resample_linear
    y48 = resample_linear(np.repeat(x, 6), P.FS * 6, P.FS)   # 先上采到 48k 再降回 8k
    r = safe(lambda: R.decode(y48, P.FS), "48 kHz 往返（线性重采样）")
    check("48k 路径可解", r is not None and r.n_cells == truth,
          f"cells={r.n_cells if r else '-'}")

    dc = x + 0.3
    r = safe(lambda: R.decode(dc.astype(np.float32), P.FS), "直流偏移 +0.3")
    check("直流不影响", r is not None and r.n_cells == truth,
          f"cells={r.n_cells if r else '-'}")

    clip = np.clip(x * 3.0, -1, 1).astype(np.float32)
    r = safe(lambda: R.decode(clip, P.FS), "过载削波 ×3")
    check("削波仍可解", r is not None and r.n_cells == truth,
          f"cells={r.n_cells if r else '-'}")

    print("\n信道损伤")

    fade = C.slow_fade(C.awgn(x, 10, seed=2), depth_db=8.0, rate_hz=0.12)
    r = safe(lambda: R.decode(fade, P.FS), "慢衰落（8 dB, 0.12 Hz）")
    check("衰落仍可解", r is not None and r.n_cells == truth,
          f"cells={r.n_cells if r else '-'}")

    for b in (40.0, -40.0, 60.0):
        y = C.shift_freq(C.awgn(x, 10, seed=4), b)
        r = safe(lambda: R.decode(y, P.FS), f"频偏 {b:+.0f} Hz")
        check(f"频偏 {b:+.0f} Hz 可解", r is not None and r.n_cells == truth,
              f"cells={r.n_cells if r else '-'} b̂={r.est.b:+.1f}" if r else "")

    for a in (1.0002, 0.9998):
        y = C.resample_time(C.awgn(x, 10, seed=5), a)
        r = safe(lambda: R.decode(y, P.FS), f"时钟 {a:.4f}")
        check(f"时钟 {a:.4f} 可解", r is not None and r.n_cells == truth,
              f"â={r.est.a:.7f}" if r else "")

    print("\n确定性与性能")

    y = C.awgn(x, 6, seed=9)
    r1, r2 = R.decode(y, P.FS), R.decode(y, P.FS)
    check("解码确定性", r1.n_cells == r2.n_cells,
          f"cells {r1.n_cells}/{r2.n_cells}")
    same = (len(r1.blocks) == len(r2.blocks)
            and all(np.array_equal(a, b) for a, b in zip(r1.blocks, r2.blocks)))
    check("块内容逐位一致", same)

    long_glyphs, _ = font.rasters("频谱书写通信试验测试频率七三结束应急联络二公里正东")
    xl, _ = T.build(long_glyphs, P.M2, seed=1)
    t0 = time.perf_counter()
    rl = R.decode(C.awgn(xl, 6, seed=1), P.FS)
    dt = time.perf_counter() - t0
    ratio = dt / (len(xl) / P.FS)
    check("长报文解码 < 0.2×实时", ratio < 0.2,
          f"{len(xl)/P.FS:.1f}s 音频用 {dt:.2f}s（{ratio*100:.1f}×实时），cells={rl.n_cells}")

    print(f"\n{'鲁棒性全部通过' if not FAILS else f'失败 {len(FAILS)} 项'}")
    for f in FAILS:
        print("  - " + f)
    return 1 if FAILS else 0


if __name__ == "__main__":
    sys.exit(main())
