"""双导频频率标定：由 P1/P2 解出音频域的仿射参数 (a, b)。

    f_meas = a · f_nom + b

a 含采样时钟比例（量级 1e-6…1e-4），b 是调谐/BFO 造成的固定偏移（几十 Hz）。
拿到 (a, b) 之后，视觉梳、CC 载波、边距点全部由标称表直接算出，不必搜索。
"""

from __future__ import annotations

from dataclasses import dataclass

import numpy as np

from .params import FS, PILOT1, PILOT2


@dataclass
class PilotEstimate:
    a: float
    b: float
    f1: float          # 实测 P1
    f2: float          # 实测 P2
    conf: float        # 0..1，相位拟合的线性度与幅度

    def apply(self, f_nom: np.ndarray | float):
        return self.a * np.asarray(f_nom, float) + self.b

    @property
    def df_scale(self) -> float:
        return self.a


def _demod_tone(x: np.ndarray, f: float, fs: int) -> np.ndarray:
    t = np.arange(len(x)) / fs
    return x.astype(np.complex128) * np.exp(-2j * np.pi * f * t)


def _smooth(z: np.ndarray, n: int) -> np.ndarray:
    k = np.ones(n) / n
    return np.convolve(z, k, mode="valid")


def _coarse_peak(x: np.ndarray, f_nom: float, fs: int, search: float = 60.0) -> float:
    """在 f_nom ± search 内找谱峰（Hann + 抛物线插值）。

    搜索窗不能太宽：+1250 Hz 导频上方 ±120 Hz 会碰到视觉带顶端（1175 Hz），
    弱信号下会把视觉层的能量当成导频。±60 Hz 覆盖常规调谐误差，且留足隔离。
    """
    seg = x[: min(len(x), 4 * fs)]
    if len(seg) < 64:
        return f_nom
    n = 1 << int(np.ceil(np.log2(len(seg))))
    w = np.hanning(len(seg))
    mag = np.abs(np.fft.rfft(seg * w, n))
    freqs = np.fft.rfftfreq(n, 1 / fs)
    band = np.flatnonzero((freqs > f_nom - search) & (freqs < f_nom + search))
    if len(band) < 3:
        return f_nom
    j = int(band[np.argmax(mag[band])])
    y0, y1, y2 = (np.log(mag[j - 1] + 1e-12), np.log(mag[j] + 1e-12),
                  np.log(mag[j + 1] + 1e-12))
    denom = y0 - 2 * y1 + y2
    d = 0.0 if abs(denom) < 1e-12 else 0.5 * (y0 - y2) / denom
    return float(freqs[j] + d * (freqs[1] - freqs[0]))


def _tone_freq(x: np.ndarray, f_nom: float, fs: int, win: float = 0.5) -> tuple[float, float]:
    """测一根导频的实际频率，返回 (f_meas, conf)。

    先用长 FFT 找到粗频（不受低通零点影响），再用相位斜率精修到 0.01 Hz 量级。
    """
    f0 = _coarse_peak(x, f_nom, fs)
    L = max(int(win * fs), 16)
    z = _smooth(_demod_tone(x, f0, fs), L)
    if len(z) < 8:
        return f0, 0.0
    mag = np.abs(z)
    good = mag > 0.25 * np.percentile(mag, 95)
    if good.sum() < 8:
        return f0, 0.0
    ph = np.unwrap(np.angle(z))
    t = (np.arange(len(z)) + L / 2) / fs
    tt, pp = t[good], ph[good]
    if len(tt) < 8:
        return f0, 0.0
    A = np.vstack([tt, np.ones_like(tt)]).T
    slope, icept = np.linalg.lstsq(A, pp, rcond=None)[0]
    resid = pp - (slope * tt + icept)
    conf = float(np.clip(1.0 - np.std(resid) / (np.pi / 2), 0.0, 1.0))
    return f0 + slope / (2 * np.pi), conf


def estimate(x: np.ndarray, fs: int = FS, p1: float = PILOT1, p2: float = PILOT2,
             win: float = 0.5) -> PilotEstimate:
    f1, c1 = _tone_freq(x, p1, fs, win)
    f2, c2 = _tone_freq(x, p2, fs, win)
    a = (f2 - f1) / (p2 - p1)
    b = f1 - a * p1
    return PilotEstimate(a=float(a), b=float(b), f1=float(f1), f2=float(f2),
                         conf=float(min(c1, c2)))
