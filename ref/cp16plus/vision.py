"""视觉层：行调度、合成、解调、栅格重建。"""

from __future__ import annotations

from dataclasses import dataclass

import numpy as np

from .params import FS, Mode


@dataclass
class RowPlan:
    """一段连续字符的行调度。"""

    rows: np.ndarray        # (R, N) uint8，每行一个码元的墨点
    margin: np.ndarray      # (R,) bool，是否边距行
    start: int              # 第一行的起始样点
    n_row: int

    @property
    def n_rows(self) -> int:
        return len(self.rows)

    @property
    def end(self) -> int:
        return self.start + self.n_rows * self.n_row


def plan_cells(glyphs, mode: Mode, start: int, fs: int = FS) -> RowPlan:
    """把一组字形排成"边距行 + N 行字形"，每行重复 R 次。"""
    n_row = int(round(mode.t_row * fs))
    rows: list[np.ndarray] = []
    margin: list[bool] = []
    for g in glyphs:
        for _ in range(mode.repeat):
            rows.append(np.zeros(mode.n, np.uint8))
            margin.append(True)
        for r in range(mode.n):
            for _ in range(mode.repeat):
                rows.append(np.asarray(g[r], np.uint8))
                margin.append(False)
    return RowPlan(np.array(rows, np.uint8), np.array(margin, bool), start, n_row)


# --------------------------------------------------------------------------
def _ramp_kernel(n: int) -> np.ndarray:
    k = 0.5 * (1 - np.cos(2 * np.pi * np.arange(n) / max(n - 1, 1)))
    return k / k.sum()


def synth(plan: RowPlan, mode: Mode, total: int, fs: int = FS,
          seed: int = 1, amp: float = 0.8) -> np.ndarray:
    """合成视觉层（不含导频与 CC）。

    每路音独立包络、连续相位、随机初相；过渡用升余弦。
    """
    rng = np.random.default_rng(seed)
    n_row = plan.n_row
    L = max(int(round(0.03 * n_row)), 4) | 1
    kern = _ramp_kernel(L)
    half = L // 2

    t = np.arange(total) / fs
    x = np.zeros(total, np.float64)
    for i in range(mode.n):
        env = np.zeros(total)
        col = plan.rows[:, i]
        if col.any():
            for r in np.flatnonzero(col):
                s = plan.start + int(r) * n_row
                if s >= total:
                    continue
                env[s: min(s + n_row, total)] = 1.0
            full = np.convolve(env, kern, mode="full")
            env = full[half: half + total]
        if env.any():
            x += env * np.cos(2 * np.pi * (mode.f_lo + i * mode.df) * t
                              + rng.uniform(0, 2 * np.pi))

    # 边距点：只在边距行前 40%，幅度略高于单个字形音（它是"标点"，不是笔画）
    marg = np.zeros(total)
    quarter = max(int(n_row * 0.4), 1)
    for r in np.flatnonzero(plan.margin):
        s = plan.start + int(r) * n_row
        if s < total:
            marg[s: min(s + quarter, total)] = 1.0
    if marg.any():
        x += 1.6 * marg * np.cos(2 * np.pi * mode.f_margin * t
                                 + rng.uniform(0, 2 * np.pi))

    rms = float(np.sqrt(np.mean(x ** 2)))
    if rms > 0:
        x *= amp / rms
    return x.astype(np.float32)


# --------------------------------------------------------------------------
def _basis(tones: np.ndarray, n: int, fs: int) -> np.ndarray:
    t = np.arange(n) / fs
    return np.exp(-2j * np.pi * np.outer(tones, t)) / n


def demod(x: np.ndarray, t0: float, n_rows: int, t_row: float,
          tones, f_margin: float, fs: int = FS) -> tuple[np.ndarray, np.ndarray]:
    """按栅格做 box 积分。`t_row` 允许是小数（含时钟偏差时行周期不是整数样点）。

    返回 (P, Pm)：P 为 (n_rows, N) 各音功率，Pm 为 (n_rows,) 边距音功率。
    """
    tones = np.asarray(tones, float)
    n = int(round(t_row))
    B = _basis(tones, n, fs)
    starts = np.round(t0 + np.arange(n_rows) * t_row).astype(np.int64)
    idx = np.clip(starts[:, None] + np.arange(n)[None, :], 0, len(x) - 1)
    segs = x[idx].astype(np.float64)
    P = np.abs(segs @ B.T) ** 2

    q = max(n // 4, 1)
    Bm = _basis(np.array([f_margin]), q, fs)[0]
    segq = x[idx[:, :q]].astype(np.float64)
    Pm = np.abs(segq @ Bm) ** 2
    return P, Pm


def score_grid(P: np.ndarray) -> float:
    """栅格对齐的粗评分：行内"亮/暗"分离度。

    取各行的 75 分位而不是中位数——探针窗口里常混着大段静音（HDR/间隔），
    中位数会被"没墨的行"拉平，导致相位搜索失去分辨力。
    """
    if len(P) == 0:
        return 0.0
    k = max(int(P.shape[1] * 0.25), 2)
    lo = max(int(P.shape[1] * 0.5), 2)
    part = np.sort(P, axis=1)
    bright = part[:, -k:].mean(axis=1)
    dark = part[:, :lo].mean(axis=1) + 1e-12
    ratio = bright / dark
    return float(np.percentile(ratio, 75))


def find_grid(x: np.ndarray, mode: Mode, fs: int = FS,
              coarse: int = 32, rows_probe: int = 96) -> int:
    """在 0..T_row 内搜索码元相位（按档位的标称参数）。"""
    n_row = int(round(mode.t_row * fs))
    return find_grid_n(x, n_row, mode.tones(), mode.f_margin, fs, coarse, rows_probe)


def find_grid_n(x: np.ndarray, n_row: int, tones, f_margin: float, fs: int = FS,
                coarse: int = 32, rows_probe: int = 96) -> int:
    """按"实测"行周期与频率搜索码元相位。"""
    best_t0, best = 0, -1.0
    for step in range(coarse):
        t0 = int(round(step * n_row / coarse))
        n_probe = min(rows_probe, max(1, (len(x) - t0) // n_row))
        if n_probe < 8:
            break
        P, _ = demod(x, t0, n_probe, n_row, tones, f_margin, fs)
        s = score_grid(P)
        if s > best:
            best, best_t0 = s, t0
    # 细化
    span = n_row / coarse
    for d in np.linspace(-span, span, 9):
        t0 = int(round(best_t0 + d)) % n_row
        n_probe = min(rows_probe, max(1, (len(x) - t0) // n_row))
        if n_probe < 8:
            continue
        P, _ = demod(x, t0, n_probe, n_row, tones, f_margin, fs)
        s = score_grid(P)
        if s > best:
            best, best_t0 = s, t0
    return best_t0


def ink_segment(Pw: np.ndarray, rel: float = 0.12) -> tuple[int, int]:
    """用行能量切出"有字"的连续段 [r0, r1)。"""
    if len(Pw) == 0:
        return 0, 0
    E = Pw.sum(axis=1)
    thr = rel * float(np.percentile(E, 90)) if E.size else 0.0
    idx = np.flatnonzero(E > thr)
    if len(idx) == 0:
        return 0, 0
    return int(idx[0]), int(idx[-1]) + 1


def silent_runs(Pw: np.ndarray, lo: int, hi: int, rel: float = 0.05) -> list[tuple[int, int]]:
    """[lo,hi) 内的静默行区间（行能量低于中位数的 rel）。

    用于"漏检一个 BCON、两段被合并"时把段重新切开——120 个符号的空洞
    在行栅格上是整数行，所以切开后行相位仍然正确。
    """
    if hi <= lo:
        return []
    E = Pw[lo:hi].sum(axis=1)
    med = float(np.median(E)) + 1e-12
    quiet = E < rel * med
    runs: list[tuple[int, int]] = []
    i = 0
    while i < len(quiet):
        if not quiet[i]:
            i += 1
            continue
        j = i
        while j < len(quiet) and quiet[j]:
            j += 1
        runs.append((lo + i, lo + j))
        i = j
    return runs


def find_cells(Pm: np.ndarray, Pw: np.ndarray | None = None, rows_per_char: int = 17,
               thr_ratio: float = 4.0) -> list[int]:
    """定位字符起始行。

    以最强的边距点为锚，按 rows_per_char 前后延伸；比"逐行找边距点"稳，
    也不会像"按段起点枚举相位"那样漏掉第一个字符。
    """
    if len(Pm) == 0:
        return []
    i0 = int(np.argmax(Pm))
    peak = float(Pm[i0])
    if peak <= 0:
        return []
    step = max(rows_per_char, 1)
    thr = 0.45 * peak
    while i0 - step >= 0 and Pm[i0 - step] > thr:
        i0 -= step
    cells: list[int] = []
    i = i0
    while i < len(Pm) and Pm[i] > thr:
        cells.append(i)
        i += step
    return cells
