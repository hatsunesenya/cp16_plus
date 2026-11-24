"""控制信道物理层：DQPSK 125 Bd + RRC 成形 + 前导相关定时。

一个突发 = 32 符号前导 + 若干数据符号；差分编码从前导的最后一个符号开始，
所以数据区的第一个符号相对前导末符号做差分，接收端不需要绝对相位。

前导有两种（HDR 用 A、BCON/EOT 用 B）：接收端只靠前导就能判断突发类型与长度，
不必先解出载荷——这是"CC 锚定定时"能工作的前提。
"""

from __future__ import annotations

import numpy as np

from .params import CC_BAUD, CC_CENTER, CC_RRC_ALPHA, FS

PREAMBLE_BITS = 64           # 32 符号
SPS = 64                     # 8 kHz / 125 Bd


def _make_preamble(seed: int) -> np.ndarray:
    rng = np.random.default_rng(seed)
    q = rng.integers(0, 4, PREAMBLE_BITS // 2)
    b1 = (q >> 1) & 1
    b2 = q & 1
    return ((1 - 2 * b1) + 1j * (1 - 2 * b2)) / np.sqrt(2)


PREAMBLE_HDR = _make_preamble(0xC16)      # type A
PREAMBLE_BCON = _make_preamble(0xB0C)     # type B（BCON 与 EOT 共用）
PREAMBLE_SYNC = _make_preamble(0xC3)     # type C：周期同步帧（v1.1）
PREAMBLE = PREAMBLE_HDR                   # 兼容旧名

KIND_HDR = "HDR"
KIND_BCON = "BCON"


def rrc_taps(alpha: float = CC_RRC_ALPHA, sps: int = SPS, span: int = 8) -> np.ndarray:
    n = span * sps
    t = np.arange(-n, n + 1) / sps
    h = np.empty_like(t, dtype=float)
    for i, ti in enumerate(t):
        if abs(ti) < 1e-9:
            h[i] = 1 - alpha + 4 * alpha / np.pi
        elif abs(abs(ti) - 1 / (4 * alpha)) < 1e-9:
            h[i] = (alpha / np.sqrt(2)) * ((1 + 2 / np.pi) * np.sin(np.pi / (4 * alpha))
                                           + (1 - 2 / np.pi) * np.cos(np.pi / (4 * alpha)))
        else:
            h[i] = (np.sin(np.pi * ti * (1 - alpha)) + 4 * alpha * ti
                    * np.cos(np.pi * ti * (1 + alpha))) / (np.pi * ti * (1 - (4 * alpha * ti) ** 2))
    return h / np.sqrt(np.sum(h ** 2))


TAPS = rrc_taps()
#: 收发各一次 RRC，相关峰相对音频域符号起点的滞后（样点）
MATCH_DELAY = len(TAPS) - 1


def _syms_from_bits(bits: np.ndarray) -> np.ndarray:
    """比特对 -> 单位符号；星座在 (±1±j)/√2，I 与 Q 各承载 1 bit。

    b1 = 1 → I<0；b2 = 1 → Q<0。软值直接取 (I, Q)，与 Viterbi 的"正=0"一致。
    """
    bits = np.asarray(bits, np.uint8)
    i = 1 - 2 * bits[0::2].astype(float)
    q = 1 - 2 * bits[1::2].astype(float)
    return (i + 1j * q) / np.sqrt(2)


def _bits_from_syms(d: np.ndarray) -> tuple[np.ndarray, np.ndarray]:
    bits = np.empty(len(d) * 2, np.uint8)
    bits[0::2] = (d.real < 0).astype(np.uint8)
    bits[1::2] = (d.imag < 0).astype(np.uint8)
    soft = np.empty(len(d) * 2)
    soft[0::2] = d.real          # 正 = 0
    soft[1::2] = d.imag
    return bits, soft


def modulate(bits: np.ndarray, fs: int = FS, center: float = CC_CENTER,
             baud: float = CC_BAUD, amp: float = 0.35,
             preamble: np.ndarray | None = None) -> np.ndarray:
    """返回一段带前导的 DQPSK 音频。"""
    sps = int(round(fs / baud))
    pre = PREAMBLE_HDR if preamble is None else preamble
    data = _syms_from_bits(bits)
    n_pre = len(pre)
    d = np.empty(n_pre + len(data), complex)
    d[:n_pre] = pre
    acc = pre[-1]
    for i, s in enumerate(data):
        acc = acc * s
        d[n_pre + i] = acc
    imp = np.zeros(len(d) * sps, complex)
    imp[::sps] = d
    base = np.convolve(imp, TAPS)
    t = np.arange(len(base)) / fs
    return (np.real(base * np.exp(2j * np.pi * center * t)) * amp).astype(np.float32)


def _to_baseband(x: np.ndarray, fs: int, center: float) -> np.ndarray:
    t = np.arange(len(x)) / fs
    return 2.0 * x * np.exp(-2j * np.pi * center * t)


def _scores(mf: np.ndarray, pre: np.ndarray, sps: int, step: int) -> tuple[np.ndarray, np.ndarray]:
    n_pre = len(pre)
    span = len(mf) - n_pre * sps
    starts = np.arange(0, max(span, 1), step)
    idx = starts[:, None] + np.arange(n_pre)[None, :] * sps
    idx = np.clip(idx, 0, len(mf) - 1)
    Y = mf[idx]
    num = np.abs(Y @ np.conj(pre))
    den = np.linalg.norm(Y, axis=1) * np.linalg.norm(pre) + 1e-12
    return starts, num / den


def find_bursts(x: np.ndarray, fs: int = FS, center: float = CC_CENTER,
                baud: float = CC_BAUD, rel: float = 0.45,
                min_sep_sym: int = 40, step: int = 2) -> list[tuple[int, str, float]]:
    """找出全部 CC 突发：[(符号起点样点, 类型, 置信度)]。

    两类前导分别做归一化相关，取较强者；再做非极大抑制，保证一个突发只报一次。
    定时锚点只需要前导，因此**即使载荷 CRC 失败也能锚定**——这正是弱信号下
    解调器不再失锁的关键。
    """
    sps = int(round(fs / baud))
    mf = np.convolve(_to_baseband(x, fs, center), TAPS)
    st_a, sc_a = _scores(mf, PREAMBLE_HDR, sps, step)
    _, sc_b = _scores(mf, PREAMBLE_BCON, sps, step)
    best = np.maximum(sc_a, sc_b)
    kind = np.where(sc_a >= sc_b, KIND_HDR, KIND_BCON)
    peak = float(best.max()) if best.size else 0.0
    if peak <= 0:
        return []

    thr = rel * peak
    sep = max(int(min_sep_sym * sps), 1)

    def _energy(a: int, b: int) -> float:
        a, b = max(0, a), min(len(mf), b)
        return float(np.mean(np.abs(mf[a:b]) ** 2)) if b > a else 0.0

    def _onset_ok(s0: int) -> bool:
        """真突发前面是 CC 静默；视觉层里的假峰"一直有能量"。

        门限只要 0.6 dB：−9 dB 参考信噪比下 CC 带内只有约 +2.5 dB，
        取太严会把真突发也筛掉（实测 1.6 倍门限在 −9 dB 会误杀 HDR）。
        **必须先判再抑制**，否则落在真突发附近的高分假峰会把它挤掉。
        """
        e_dur = _energy(s0, s0 + 32 * sps)
        e_pre = _energy(s0 - 16 * sps, s0)
        return e_dur >= 1.15 * e_pre

    # ---- 在线（因果）判决：与 Rust `rt::Stream` 同一条规则 ----
    # 按时间顺序扫描"局部极大"，门限取 max(0.4, 0.65×迄今最高分)。
    # 离线只是把同一进程顺序跑一遍，所以离线与实时结果天然一致——
    # 这是规范，不是巧合；上线后不允许再出现"离线能用、实时不行"。
    running_max = 0.0
    cands: list[tuple[int, str, float, float]] = []
    for j in range(1, len(best) - 1):
        sc = float(best[j])
        if sc < best[j - 1] or sc < best[j + 1] or sc < 0.4:
            continue
        ceiling = max(running_max, sc)
        # 绝对下限 0.6 + 相对 0.65×迄今最高分。
        # 下限的作用：早期一个 0.5 的弱峰会把"最高分"拉低，门限随之塌掉，
        # 后面全是假突发（−9 dB 实测：段被切碎只剩 1 字）。
        if sc < 0.6 or sc < 0.65 * ceiling:
            continue
        s0 = int(st_a[j])
        if not _onset_ok(s0):
            continue
        running_max = max(running_max, sc)
        cands.append((s0, str(kind[j]), sc, _energy(s0, s0 + 32 * sps)))

    # 相对能量门限：整条报文里各突发的电平应当接近，差 6 dB 以上的多半是假峰
    if cands:
        e_max = max(c[3] for c in cands)
        cands = [c for c in cands if c[3] >= 0.25 * e_max]

    out: list[tuple[int, str, float]] = []
    for s0, k, sc, _e in cands:
        if any(abs(s0 - t) < sep for t, _k, _s in out):
            continue
        out.append((s0, k, sc))
    out.sort(key=lambda r: r[0])

    # 精修到采样点级：取符号点能量最大的位置
    refined: list[tuple[int, str, float]] = []
    for s0, k, sc in out:
        pre = PREAMBLE_HDR if k == KIND_HDR else PREAMBLE_BCON
        pick, e_best = s0, -1.0
        for st in range(max(0, s0 - step), min(len(mf) - len(pre) * sps, s0 + step + 1)):
            yk = mf[st + np.arange(len(pre)) * sps]
            e = float(np.sum(np.abs(yk) ** 2))
            if e > e_best:
                e_best, pick = e, st
        refined.append((pick, k, sc))

    # 相关峰在匹配滤波器输出上，减去收发两级 RRC 的群延迟，换算成音频域符号起点
    return [(int(s0 - MATCH_DELAY), k, sc) for s0, k, sc in refined]


def _score_at(mf: np.ndarray, pre: np.ndarray, sps: int, start: int) -> float:
    n_pre = len(pre)
    if start < 0 or start + n_pre * sps > len(mf):
        return 0.0
    yk = mf[start: start + n_pre * sps: sps]
    num = abs(np.dot(yk, np.conj(pre)))
    den = np.linalg.norm(yk) * np.linalg.norm(pre) + 1e-12
    return float(num / den)


def demodulate(x: np.ndarray, n_bits: int, fs: int = FS, center: float = CC_CENTER,
               baud: float = CC_BAUD, start: int | None = None,
               preamble: np.ndarray | None = None) -> tuple[np.ndarray, np.ndarray, float]:
    """返回 (硬判比特, 软值, 相关置信度)。

    `start` 给定时直接按该样点取符号（CC 锚定路径）；否则自行扫描前导。
    """
    sps = int(round(fs / baud))
    pre = PREAMBLE_HDR if preamble is None else preamble
    mf = np.convolve(_to_baseband(x, fs, center), TAPS)

    if start is None:
        want = KIND_HDR if pre is PREAMBLE_HDR else KIND_BCON
        bursts = find_bursts(x, fs, center, baud, rel=0.0)
        pick = [b for b in bursts if b[1] == want] or bursts
        start = pick[0][0] if pick else 0
    # start 是音频域符号起点；匹配滤波输出上要加回群延迟
    pos = int(start) + MATCH_DELAY
    conf = _score_at(mf, pre, sps, pos)

    n_pre = len(pre)
    total = n_pre + n_bits // 2
    yk = mf[pos: pos + total * sps: sps]
    if len(yk) < total:
        yk = np.concatenate([yk, np.zeros(total - len(yk), complex)])
    d = yk[1:] * np.conj(yk[:-1])
    bits, soft = _bits_from_syms(d)
    data_bits = bits[n_pre * 2 - 2:][:n_bits]
    data_soft = soft[n_pre * 2 - 2:][:n_bits]
    return data_bits[:n_bits], data_soft[:n_bits], conf
