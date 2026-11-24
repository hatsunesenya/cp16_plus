"""WAV 读写（16/24/32 bit PCM 与 float32/64），只依赖 numpy。

参考实现只需要"单声道、给定采样率"这一件事，所以接口刻意做窄：
``read_wav(path) -> (x float32 单声道, fs)``、``write_wav(path, x, fs)``。
"""

from __future__ import annotations

import struct
from pathlib import Path

import numpy as np


def _parse(data: bytes):
    if data[:4] != b"RIFF" or data[8:12] != b"WAVE":
        raise ValueError("not a RIFF/WAVE file")
    pos = 12
    fmt = None
    payload = None
    while pos + 8 <= len(data):
        cid = data[pos:pos + 4]
        size = struct.unpack_from("<I", data, pos + 4)[0]
        body = data[pos + 8: pos + 8 + size]
        if cid == b"fmt ":
            fmt = body
        elif cid == b"data":
            payload = body
        pos += 8 + size + (size & 1)
    if fmt is None or payload is None:
        raise ValueError("missing fmt/data chunk")
    tag, ch, fs, _brate, _align, bits = struct.unpack_from("<HHIIHH", fmt, 0)
    if tag == 0xFFFE and len(fmt) >= 40:          # WAVE_FORMAT_EXTENSIBLE
        tag = struct.unpack_from("<H", fmt, 24)[0]
    return tag, ch, fs, bits, payload


def read_wav(path: str | Path) -> tuple[np.ndarray, int]:
    tag, ch, fs, bits, payload = _parse(Path(path).read_bytes())
    if tag == 3:
        dt = np.float32 if bits == 32 else np.float64
        x = np.frombuffer(payload, dtype=dt).astype(np.float32)
    elif tag == 1:
        if bits == 16:
            x = np.frombuffer(payload, dtype="<i2").astype(np.float32) / 32768.0
        elif bits == 24:
            b = np.frombuffer(payload, dtype=np.uint8).reshape(-1, 3).astype(np.int32)
            v = (b[:, 0] | (b[:, 1] << 8) | (b[:, 2] << 16))
            v = np.where(v & 0x800000, v - 0x1000000, v)
            x = (v / 8388608.0).astype(np.float32)
        elif bits == 32:
            x = np.frombuffer(payload, dtype="<i4").astype(np.float32) / 2147483648.0
        else:
            raise ValueError(f"unsupported PCM depth {bits}")
    else:
        raise ValueError(f"unsupported format tag {tag}")
    if ch > 1:
        x = x.reshape(-1, ch).mean(axis=1)
    return x, fs


def write_wav(path: str | Path, x: np.ndarray, fs: int) -> None:
    x = np.asarray(x, dtype=np.float32)
    x = np.clip(x, -1.0, 1.0)
    pcm = (x * 32767.0).astype("<i2").tobytes()
    head = struct.pack("<4sI4s4sIHHIIHH4sI",
                       b"RIFF", 36 + len(pcm), b"WAVE",
                       b"fmt ", 16, 1, 1, fs, fs * 2, 2, 16,
                       b"data", len(pcm))
    Path(path).write_bytes(head + pcm)


def resample_linear(x: np.ndarray, fs_in: int, fs_out: int) -> np.ndarray:
    """线性插值重采样（参考实现够用；产品用多相 FIR）。"""
    if fs_in == fs_out:
        return x.astype(np.float32)
    n_out = int(round(len(x) * fs_out / fs_in))
    t = np.arange(n_out) * (fs_in / fs_out)
    return np.interp(t, np.arange(len(x)), x).astype(np.float32)


def add_awgn(x: np.ndarray, snr_ref_db: float, fs: int, b_ref: float = 2400.0,
             seed: int = 0) -> np.ndarray:
    """按 B_REF 带宽内的信噪比加白噪声。"""
    ps = float(np.mean(x.astype(np.float64) ** 2))
    pn = ps / (10 ** (snr_ref_db / 10))
    sigma2 = pn * fs / (2 * b_ref)
    rng = np.random.default_rng(seed)
    return (x + rng.normal(0, np.sqrt(sigma2), len(x))).astype(np.float32)
