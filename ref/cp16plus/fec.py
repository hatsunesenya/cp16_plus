"""FEC：K=7 卷积码（r=1/2）、块交织、CRC-16。

码字用 NASA 标准生成多项式 0o171 / 0o133；软判决 Viterbi。
"""

from __future__ import annotations

import numpy as np

K = 7
NSTATE = 1 << (K - 1)          # 64
POLYS = (0o171, 0o133)
TAIL = K - 1                   # 编码尾部清零比特


def _parity(v: int) -> int:
    v ^= v >> 4
    v ^= v >> 2
    v ^= v >> 1
    return v & 1


def _out_table() -> np.ndarray:
    """(state, bit) -> (out0, out1)。"""
    t = np.zeros((NSTATE, 2, 2), np.uint8)
    for s in range(NSTATE):
        for b in range(2):
            reg = (s << 1) | b
            t[s, b, 0] = _parity(reg & POLYS[0])
            t[s, b, 1] = _parity(reg & POLYS[1])
    return t


OUT = _out_table()
NEXT = np.array([[(s << 1) & (NSTATE - 1) | b for b in (0, 1)] for s in range(NSTATE)])


# --------------------------------------------------------------------------
def conv_encode(bits) -> np.ndarray:
    bits = np.asarray(bits, np.uint8)
    padded = np.concatenate([bits, np.zeros(TAIL, np.uint8)])
    out = np.empty(len(padded) * 2, np.uint8)
    s = 0
    for i, b in enumerate(padded):
        out[2 * i] = OUT[s, b, 0]
        out[2 * i + 1] = OUT[s, b, 1]
        s = NEXT[s, b]
    return out


def viterbi(soft: np.ndarray, n_info: int) -> np.ndarray:
    """soft: 长度 2·(n_info+TAIL) 的软值（正=0，负=1）。"""
    soft = np.asarray(soft, np.float64)
    n_steps = len(soft) // 2
    NEG = -1e18
    metric = np.full(NSTATE, NEG)
    metric[0] = 0.0
    pred = np.zeros((n_steps, NSTATE), np.uint8)     # 前驱编码 s*2+b

    r = soft.reshape(n_steps, 2)
    for t in range(n_steps):
        new = np.full(NSTATE, NEG)
        for s in range(NSTATE):
            ms = metric[s]
            if ms <= NEG:
                continue
            for b in (0, 1):
                ns = NEXT[s, b]
                m = ms + r[t, 0] * (1 - 2 * int(OUT[s, b, 0])) \
                       + r[t, 1] * (1 - 2 * int(OUT[s, b, 1]))
                if m > new[ns]:
                    new[ns] = m
                    pred[t, ns] = s * 2 + b
        metric = new

    state = 0
    out = np.zeros(n_steps, np.uint8)
    for t in range(n_steps - 1, -1, -1):
        code = int(pred[t, state])
        out[t] = code & 1
        state = code >> 1
    return out[: n_info or n_steps]


# --------------------------------------------------------------------------
def _perm(n: int, seed: int = 0) -> np.ndarray:
    """确定性块交织置换（矩阵转置读法，跨语言可逐位复现）。

    规则：cols = ceil(n / rows)，rows = floor(sqrt(n))；
    按"写行、读列"生成 `p[i] = (i % rows) * cols + (i // rows)`，跳过 ≥ n 的位置。
    不用伪随机置换——那会把 numpy 的 PRNG 变成协议的一部分。
    """
    rows = max(int(np.floor(np.sqrt(n))), 1)
    cols = (n + rows - 1) // rows
    order = [(i % rows) * cols + (i // rows) for i in range(n * 2)]
    p = [v for v in order if v < n][:n]
    seen = np.zeros(n, bool)
    out = []
    for v in p:
        if not seen[v]:
            seen[v] = True
            out.append(v)
    for v in range(n):
        if not seen[v]:
            out.append(v)
    return np.asarray(out, dtype=np.int64)


def interleave(bits: np.ndarray, seed: int = 0x5EED) -> np.ndarray:
    p = _perm(len(bits), seed)
    out = np.empty_like(bits)
    out[p] = bits
    return out


def deinterleave(soft: np.ndarray, seed: int = 0x5EED) -> np.ndarray:
    p = _perm(len(soft), seed)
    return soft[p].copy()


# --------------------------------------------------------------------------
def crc16(bits) -> int:
    crc = 0xFFFF
    for b in np.asarray(bits, np.uint8):
        crc ^= (int(b) & 1) << 15
        for _ in range(1):
            if crc & 0x8000:
                crc = ((crc << 1) ^ 0x1021) & 0xFFFF
            else:
                crc = (crc << 1) & 0xFFFF
    return crc


def bits_of(value: int, width: int) -> np.ndarray:
    return np.array([(value >> (width - 1 - i)) & 1 for i in range(width)], np.uint8)


def value_of(bits) -> int:
    v = 0
    for b in np.asarray(bits, np.uint8):
        v = (v << 1) | int(b)
    return v
