"""接收：导频标定 -> CC 前导锚定 -> 分段解调 -> 指标。

规范 §6.3 的落地：行边界不再搜索，而是"由 CC 前导的符号时钟分频得到"。
只有在前导完全缺失时才退回盲搜索（DEGRADED）。
"""

from __future__ import annotations

from dataclasses import dataclass, field

import numpy as np

from . import fec
from . import metrics
from . import modem
from . import params as P
from . import vision as V
from .frame import Hdr
from .params import CC_SYM, FS, Mode
from .pilot import PilotEstimate, estimate


@dataclass
class RxSegment:
    start: int
    n_rows: int
    n_cells: int
    anchor: str          # "cc" / "blind"


@dataclass
class RxResult:
    ok: bool
    hdr: Hdr | None
    mode: Mode | None
    est: PilotEstimate
    blocks: list[np.ndarray] = field(default_factory=list)   # 每字 (n,N) 功率
    segments: list[RxSegment] = field(default_factory=list)
    bursts: list[tuple[int, str, float]] = field(default_factory=list)
    metrics: dict = field(default_factory=dict)
    cc_conf: float = 0.0
    reason: str = ""

    @property
    def n_cells(self) -> int:
        return len(self.blocks)

    def raster(self) -> np.ndarray:
        """把各字符块按时间顺序拼成一幅（中间插 1 行间隔），供预览/渲染。"""
        if not self.blocks:
            return np.zeros((0, 0))
        n = self.blocks[0].shape[1]
        parts = []
        for b in self.blocks:
            parts.append(b)
            parts.append(np.zeros((1, n)))
        return np.vstack(parts[:-1])


def _mode_from_hdr(hdr: Hdr) -> Mode:
    df = 25.0 if hdr.plan == 0 else 18.75
    f_lo = 800.0 if hdr.plan == 0 else 640.0
    return Mode(name=f"hdr-{hdr.raster}-{hdr.k}-{hdr.repeat}", n=hdr.raster, df=df,
                k=hdr.k, repeat=hdr.repeat, f_lo=f_lo)


def _decode_burst(x: np.ndarray, fs: int, est: PilotEstimate, start: int,
                  kind: str) -> Hdr | None:
    center = est.a * P.CC_CENTER + est.b
    baud = P.CC_BAUD * est.a
    if kind == modem.KIND_HDR:
        n_coded = 2 * (P.HDR_BITS + fec.TAIL)
        _, soft, _ = modem.demodulate(x, n_coded, fs=fs, center=center, baud=baud,
                                      start=start, preamble=modem.PREAMBLE_HDR)
        return Hdr.unpack(fec.viterbi(fec.deinterleave(soft), P.HDR_BITS))
    return None


def _phase_offset(Pm: np.ndarray, rows_per_char: int) -> int:
    """锚点已在协议给定位，只允许 ±2 行的整行微调。"""
    best_off, best = 0, -1.0
    for off in range(-2, 3):
        vals = Pm[off: len(Pm): rows_per_char] if off < 0 else Pm[off::rows_per_char]
        vals = vals[vals >= 0]
        if len(vals) == 0:
            continue
        score = float(np.mean(vals))
        if score > best:
            best, best_off = score, off
    return best_off


def _cells_in_range(Pm: np.ndarray, lo: int, hi: int, rows_per_char: int) -> list[int]:
    """在 [lo,hi) 内按 rows_per_char 等距切字符，起点允许 ±2 行微调。

    微调是模 rows_per_char 的：offset 为负时折回一个周期，**不能直接丢弃**，
    否则低信噪比下相位微调取到 −1 会让整段一个字符都切不出来。
    """
    span = Pm[lo:hi]
    if len(span) < rows_per_char:
        return []
    off = _phase_offset(span, rows_per_char) % rows_per_char
    start = lo + off
    return list(range(start, hi - rows_per_char + 1, rows_per_char))


def _sub_ranges(Pw: np.ndarray, n_rows: int, rows_per_char: int,
                gap_rows: int) -> list[tuple[int, int]]:
    """把一段切成若干子段：漏检 BCON 时中间会留下"空洞行"，切开后行相位仍正确。"""
    runs = [r for r in V.silent_runs(Pw, 0, n_rows)
            if abs((r[1] - r[0]) - gap_rows) <= 1]
    out: list[tuple[int, int]] = []
    prev = 0
    for a, b in runs:
        if a - prev >= rows_per_char:
            out.append((prev, a))
        prev = b
    if n_rows - prev >= rows_per_char:
        out.append((prev, n_rows))
    return out or [(0, n_rows)]


def decode(x: np.ndarray, fs: int = FS, mode: Mode | None = None,
           use_cc: bool = True) -> RxResult:
    est = estimate(x, fs)
    t_sym = fs / (P.CC_BAUD * est.a)
    res = RxResult(ok=False, hdr=None, mode=None, est=est)

    bursts: list[tuple[int, str, float]] = []
    if use_cc:
        bursts = modem.find_bursts(x, fs, est.a * P.CC_CENTER + est.b,
                                   P.CC_BAUD * est.a, rel=0.65)
    res.bursts = bursts

    hdr = None
    for start, kind, _sc in bursts:
        if kind != modem.KIND_HDR:
            continue
        hdr = _decode_burst(x, fs, est, start, kind)
        if hdr is not None:
            break
    res.hdr = hdr
    if mode is None:
        mode = _mode_from_hdr(hdr) if hdr is not None else P.by_name(P.DEFAULT_MODE)
    res.mode = mode

    f_lo = est.a * mode.f_lo + est.b
    df = est.a * mode.df
    tones = f_lo + np.arange(mode.n) * df
    f_margin = f_lo - 2 * df
    t_row = (mode.k / df) * fs          # 实测行周期（含时钟偏差），可为小数

    # --- 按前导切段：每段起点 = 突发起点 + (突发符号数 + GAP) × T_sym ---
    spans: list[tuple[int, int, str]] = []
    if bursts:
        for i, (start, kind, _sc) in enumerate(bursts):
            n_sym = P.HDR_SYMBOLS if kind == modem.KIND_HDR else P.BCON_SYMBOLS
            seg_start = int(round(start + (n_sym + P.SEGMENT_GAP_SYM) * t_sym))
            seg_end = bursts[i + 1][0] if i + 1 < len(bursts) else len(x)
            if seg_end - seg_start >= 4 * t_row:
                spans.append((seg_start, seg_end, "cc"))

    # --- 退回盲搜索（无前导 / DEGRADED）---
    if not spans:
        t0 = V.find_grid_n(x, int(round(t_row)), tones, f_margin, fs, rows_probe=128)
        spans.append((t0, len(x), "blind"))

    blocks: list[np.ndarray] = []
    for seg_start, seg_end, anchor in spans:
        n_rows = int(np.floor((seg_end - seg_start + 0.35 * t_row) / t_row))
        if n_rows < mode.rows_per_char:
            continue
        Pw, Pm = V.demod(x, seg_start, n_rows, t_row, tones, f_margin, fs)
        if anchor == "cc":
            R = mode.rows_per_char
            gap_rows = max(int(round((P.BCON_SYMBOLS + P.SEGMENT_GAP_SYM)
                                     / mode.row_symbols)), 2)
            # 先按整段切；只有"解出的字数比切出来的多"时才尝试按静默空洞重切，
            # 否则文字里偶然的连续空行会被误当成段间隔。
            starts = _cells_in_range(Pm, 0, n_rows, R)
            if hdr is not None and hdr.length > len(starts) and \
                    len(_sub_ranges(Pw, n_rows, R, gap_rows)) > 1:
                starts = []
                for a, b in _sub_ranges(Pw, n_rows, R, gap_rows):
                    starts += _cells_in_range(Pm[a:b], a, b, R)
        else:
            starts = V.find_cells(Pm, Pw, mode.rows_per_char)
        kept = []
        for s in starts:
            blk = Pw[s + mode.repeat: s + mode.repeat + mode.n * mode.repeat]
            if len(blk) < mode.n * mode.repeat:
                continue
            kept.append(s)
            blocks.append(blk)
        res.segments.append(RxSegment(seg_start, n_rows, len(kept), anchor))

    if not blocks:
        res.reason = "no cells"
        res.metrics = {"J": 0.0}
        return res

    res.blocks = blocks
    res.metrics = metrics.blocks_metrics(blocks, mode.n, mode.repeat)
    res.ok = True
    res.cc_conf = float(max((b[2] for b in bursts), default=0.0))
    return res
