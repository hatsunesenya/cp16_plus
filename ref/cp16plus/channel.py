"""信道模拟：时钟比例 a、频率偏移 b、白噪声、慢衰落。

仿真是 M0 的"测量仪器"：没有它就没法量化"自动调参"和"可读性"。
"""

from __future__ import annotations

import numpy as np

from .params import FS


def resample_time(x: np.ndarray, a: float) -> np.ndarray:
    """把信号按 a 压缩时间轴：y[n] = x(a·n)，所有频率乘 a。"""
    if abs(a - 1.0) < 1e-12:
        return x.astype(np.float32)
    n_out = int(len(x) / a)
    idx = a * np.arange(n_out)
    return np.interp(idx, np.arange(len(x)), x).astype(np.float32)


def shift_freq(x: np.ndarray, b: float, fs: int = FS) -> np.ndarray:
    """整体搬移 b Hz（等价于 BFO 偏差）。

    必须走解析信号：直接乘 exp 再取实部会得到 AM 双边带，而不是频移。
    """
    if abs(b) < 1e-12:
        return x.astype(np.float32)
    n = len(x)
    X = np.fft.fft(x)
    h = np.zeros(n)
    h[0] = 1.0
    h[1:(n + 1) // 2] = 2.0
    if n % 2 == 0:
        h[n // 2] = 1.0
    xa = np.fft.ifft(X * h)
    t = np.arange(n) / fs
    return (xa * np.exp(2j * np.pi * b * t)).real.astype(np.float32)


def slow_fade(x: np.ndarray, fs: int = FS, depth_db: float = 6.0,
              rate_hz: float = 0.15, seed: int = 0) -> np.ndarray:
    """周期性慢衰落（Watterson 的单径简化）。"""
    rng = np.random.default_rng(seed)
    t = np.arange(len(x)) / fs
    ph = rng.uniform(0, 2 * np.pi, 2)
    g = np.cos(2 * np.pi * rate_hz * t + ph[0]) * 0.5 + 0.5
    g = 1.0 - (10 ** (-depth_db / 20) - 1) * g
    return (x * g).astype(np.float32)


def awgn(x: np.ndarray, snr_ref_db: float | None, fs: int = FS,
         b_ref: float = 2400.0, seed: int = 0) -> np.ndarray:
    if snr_ref_db is None:
        return x
    ps = float(np.mean(x.astype(np.float64) ** 2))
    pn = ps / (10 ** (snr_ref_db / 10))
    sigma2 = pn * fs / (2 * b_ref)
    rng = np.random.default_rng(seed)
    return (x + rng.normal(0, np.sqrt(sigma2), len(x))).astype(np.float32)
