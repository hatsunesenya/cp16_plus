"""参考实现的回归测试（无 pytest 依赖，直接 python tests/test_ref.py）。

覆盖：帧 CRC、FEC 环路、DQPSK 环路、双导频精度、端到端收发、指标合理性。
"""

from __future__ import annotations

import sys
from pathlib import Path

import numpy as np

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "ref"))

from cp16plus import channel as C
from cp16plus import fec, modem
from cp16plus import params as P
from cp16plus import rx as R
from cp16plus import tx as T
from cp16plus.font import HzkFont
from cp16plus.frame import Hdr, pack_eot, is_eot
from cp16plus.pilot import estimate, _tone_freq

FAILS: list[str] = []


def check(name: str, cond: bool, detail: str = "") -> None:
    print(f"  [{'ok' if cond else 'FAIL'}] {name}" + (f"  {detail}" if detail else ""))
    if not cond:
        FAILS.append(name)


def test_frame() -> None:
    h = Hdr(call="BG1ABC", grid="OM89AB", length=42, k=4, repeat=2, font_id=1, plan=0)
    bits = h.pack()
    check("HDR 165 bit", len(bits) == 165, f"len={len(bits)}")
    h2 = Hdr.unpack(bits)
    check("HDR 往返", h2 is not None and h2.call == h.call and h2.k == 4 and h2.repeat == 2)
    bad = bits.copy()
    bad[40] ^= 1
    check("HDR CRC 拒错", Hdr.unpack(bad) is None)
    check("EOT 标识", is_eot(pack_eot(42)))


def test_fec() -> None:
    rng = np.random.default_rng(7)
    bits = rng.integers(0, 2, 300).astype(np.uint8)
    coded = fec.conv_encode(bits)
    check("编码长度", len(coded) == 2 * (len(bits) + fec.TAIL))
    dec = fec.viterbi(1 - 2 * coded.astype(float), len(bits))
    check("无噪译码", np.array_equal(dec, bits))
    il = fec.interleave(coded)
    check("交织可逆", np.array_equal(fec.deinterleave(il.astype(float)), coded.astype(float)))
    soft = (1 - 2 * coded.astype(float)) + 0.7 * rng.normal(size=len(coded))
    check("带噪译码纠正多数错误",
          int((fec.viterbi(soft, len(bits)) != bits).sum()) < 3,
          str(int((fec.viterbi(soft, len(bits)) != bits).sum())))


def test_cc() -> None:
    h = Hdr(call="BG1ABC", grid="OM89AB", length=6, k=2)
    coded = fec.interleave(fec.conv_encode(h.pack()))
    sig = modem.modulate(coded)
    for snr in (30, 10, 0):
        y = C.awgn(sig, snr, seed=3)
        _, soft, conf = modem.demodulate(y, len(coded))
        dec = fec.viterbi(fec.deinterleave(soft), P.HDR_BITS)
        ok = Hdr.unpack(dec) is not None
        check(f"CC 环路 {snr} dB", ok, f"conf={conf:.2f}")
    check("CC 前导相关", conf > 0.8, f"conf={conf:.2f}")


def test_pilot() -> None:
    fs = P.FS
    x = T.pilots(int(6 * fs), fs, amp=0.25)
    for a, b in ((1.0, 0.0), (1.00008, 15.0), (0.99992, -22.0)):
        y = C.shift_freq(C.resample_time(x, a), b, fs)
        e = estimate(y, fs)
        check(f"导频 a={a} b={b}",
              abs(e.a - a) < 1e-6 and abs(e.b - b) < 0.05,
              f"â={e.a:.8f} b̂={e.b:+.3f}")


def test_e2e() -> None:
    font = HzkFont(ROOT.parent / "字库(必须复制到C盘hzk目录下)" / "1.hzk")
    glyphs, _ = font.rasters("频谱书写")
    x, meta = T.build(glyphs, P.M2, seed=5)
    cases = ((1.0, 0.0, 20, True, 0.95), (1.00008, 15.0, 6, True, 0.95),
             (1.0, 0.0, -9, False, 0.75))
    for a, b, snr, need_hdr, min_acc in cases:
        y = C.shift_freq(C.resample_time(x, a), b)
        y = C.awgn(y, snr, seed=9)
        res = R.decode(y, P.FS)
        if need_hdr:
            check(f"e2e HDR {a:.5f}/{snr}dB",
                  res.hdr is not None and res.hdr.call == "BG1ABC")
        check(f"e2e 字数 {a:.5f}/{snr}dB", res.n_cells == len(glyphs),
              f"cells={res.n_cells}")
        check(f"e2e 锚定 {a:.5f}/{snr}dB",
              all(s.anchor == "cc" for s in res.segments),
              str([s.anchor for s in res.segments]))
        acc = []
        for i, blk in enumerate(res.blocks):
            if i >= len(glyphs):
                break
            g = np.repeat(glyphs[i], P.M2.repeat, axis=0)
            th = (blk.max() + np.median(blk)) / 2
            acc.append(float(((blk > th).astype(np.uint8) == g).mean()))
        check(f"e2e 位准 {a:.5f}/{snr}dB", np.mean(acc) > min_acc,
              f"acc={np.mean(acc):.3f} J={res.metrics['J']:.2f}")


def test_segments() -> None:
    """多段：10 个字按每段 4 个切分，锚点必须逐段成立。"""
    font = HzkFont(ROOT.parent / "字库(必须复制到C盘hzk目录下)" / "1.hzk")
    glyphs, _ = font.rasters("频谱书写通信试验测试")
    x, meta = T.build(glyphs, P.M2, seed=7, cells_per_segment=4)
    check("段数", len(meta.segments) == 3, f"segments={len(meta.segments)}")
    check("突发数", len(meta.bursts) == 4, f"bursts={len(meta.bursts)}")   # HDR + 2 BCON + EOT
    y = C.awgn(x, 10, seed=2)
    res = R.decode(y, P.FS)
    check("多段字数", res.n_cells == len(glyphs), f"cells={res.n_cells}")
    check("多段全锚定", all(s.anchor == "cc" for s in res.segments),
          str([s.anchor for s in res.segments]))


def test_anchor_accuracy() -> None:
    """锚定定时与真值之差应 < 1% T_row。"""
    font = HzkFont(ROOT.parent / "字库(必须复制到C盘hzk目录下)" / "1.hzk")
    glyphs, _ = font.rasters("频率测试")
    x, meta = T.build(glyphs, P.M2, seed=3)
    y = C.awgn(C.shift_freq(x, 12.0), 10, seed=4)
    res = R.decode(y, P.FS)
    true_start = meta.segments[0].start
    got = res.segments[0].start if res.segments else -10**9
    err = abs(got - true_start) / (P.M2.t_row * P.FS)
    check("锚点精度 < 1% T_row", err < 0.01, f"err={err*100:.2f}%")


def test_metrics() -> None:
    from cp16plus import metrics
    clean = np.zeros((16, 16))
    clean[3:5, 3:13] = 1
    clean[5:13, 3:5] = 1
    m = metrics.cell_metrics(clean)
    check("指标：η 高", m["eta"] > 0.8, f"eta={m['eta']:.2f}")
    check("指标：连通", m["kappa"] > 0.9, f"kappa={m['kappa']:.2f}")
    check("指标：无噪点", m["nu"] < 0.2, f"nu={m['nu']:.2f}")


if __name__ == "__main__":
    print("CP16+ 参考实现自测")
    test_frame()
    test_fec()
    test_cc()
    test_pilot()
    test_e2e()
    test_segments()
    test_anchor_accuracy()
    test_metrics()
    print(f"\n{'全部通过' if not FAILS else '失败: ' + ', '.join(FAILS)}")
    sys.exit(1 if FAILS else 0)
