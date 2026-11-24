"""发射：字形 + 导频 + 控制信道 -> 一条完整音频。

时间线以 **CC 符号**为单位排布，视觉行与 CC 符号严格成整数比（T_row = 5k·T_sym），
接收端才能用前导锚定行边界（规范 §6.3）::

    |<-- lead -->|<-- HDR(A) -->|gap|<-- 段 1: 8 字 -->|<-- BCON(B) -->|gap|<-- 段 2 -->| ... |<-- EOT -->|tail|
"""

from __future__ import annotations

from dataclasses import dataclass, field

import numpy as np

from . import params as P
from . import fec
from . import modem
from .frame import Bcon, Hdr, pack_bcon, pack_eot
from .params import FS, CC_SYM, Mode
from .vision import RowPlan, plan_cells, synth


@dataclass
class Burst:
    start: int          # 前导第一个符号的样点
    kind: str           # "HDR" / "BCON"
    symbols: int        # 本突发总符号数（含前导）


@dataclass
class Segment:
    start: int          # 第一行（边距行）的样点
    n_cells: int
    plan: RowPlan


@dataclass
class TxMeta:
    mode: Mode
    text: str
    hdr: Hdr
    fs: int
    total: int
    lead_n: int
    bursts: list[Burst] = field(default_factory=list)
    segments: list[Segment] = field(default_factory=list)

    @property
    def n_cells(self) -> int:
        return sum(s.n_cells for s in self.segments)


def pilots(total: int, fs: int = FS, p1: float = P.PILOT1, p2: float = P.PILOT2,
           amp: float = 0.25) -> np.ndarray:
    t = np.arange(total) / fs
    return (amp * np.cos(2 * np.pi * p1 * t) + amp * np.cos(2 * np.pi * p2 * t)).astype(np.float32)


def cc_burst(bits: np.ndarray, kind: str = "HDR", amp: float = 1.0) -> np.ndarray:
    pre = modem.PREAMBLE_HDR if kind == "HDR" else modem.PREAMBLE_BCON
    coded = fec.conv_encode(bits)
    return modem.modulate(fec.interleave(coded), amp=amp, preamble=pre)


def _match_rms(x: np.ndarray, target: float) -> np.ndarray:
    r = float(np.sqrt(np.mean(x.astype(np.float64) ** 2)))
    return x * (target / max(r, 1e-12))


def build(glyphs, mode: Mode, fs: int = FS, lead: float = P.LEAD_PILOT,
          tail: float = P.TAIL_PILOT, vis_amp: float = 0.8,
          pilot_amp: float = 0.25, cc_ratio: float = 0.6,
          seed: int = 1, text: str = "", call: str = "BG1ABC",
          grid: str = "OM89AB", font_id: int = 1, plan: int | None = None,
          utc: int = 0,
          cells_per_segment: int = P.CELLS_PER_SEGMENT) -> tuple[np.ndarray, TxMeta]:
    t_sym = fs / P.CC_BAUD
    if plan is None:
        plan = P.plan_of(mode)
    hdr = Hdr(call=call, grid=grid, length=len(glyphs), k=mode.k,
              repeat=mode.repeat, raster=mode.n, font_id=font_id, plan=plan, utc=utc)

    cc_target = cc_ratio * vis_amp
    hdr_sig = _match_rms(cc_burst(hdr.pack(), "HDR"), cc_target)
    eot_sig = _match_rms(cc_burst(pack_eot(len(glyphs)), "BCON"), cc_target)
    cc_place: list[tuple[int, np.ndarray]] = []

    meta = TxMeta(mode=mode, text=text, hdr=hdr, fs=fs, total=0,
                  lead_n=int(round(lead * fs)))
    gap = int(round(P.SEGMENT_GAP_SYM * t_sym))
    t = meta.lead_n

    meta.bursts.append(Burst(t, "HDR", P.HDR_SYMBOLS))
    cc_place.append((t, hdr_sig))
    t += int(round(P.HDR_SYMBOLS * t_sym)) + gap

    groups = [glyphs[i:i + cells_per_segment]
              for i in range(0, len(glyphs), cells_per_segment)] or [[]]
    for gi, group in enumerate(groups):
        if not group:
            break
        # 每个段单独合成（局部时间轴从 0 开始），再整体搬到绝对位置
        local = plan_cells(group, mode, 0, fs)
        meta.segments.append(Segment(t, len(group), local))
        t += local.end
        if gi + 1 < len(groups):
            bcon = Bcon(sf_id=gi + 1, cell=(gi + 1) * cells_per_segment,
                        raster=mode.n, k=mode.k, repeat=mode.repeat, font_id=font_id)
            sig = _match_rms(cc_burst(pack_bcon(bcon, call), "BCON"), cc_target)
            meta.bursts.append(Burst(t, "BCON", P.BCON_SYMBOLS))
            cc_place.append((t, sig))
            t += int(round(P.BCON_SYMBOLS * t_sym)) + gap
    meta.bursts.append(Burst(t, "BCON", P.EOT_SYMBOLS))
    cc_place.append((t, eot_sig))
    t += int(round(P.EOT_SYMBOLS * t_sym)) + int(round(tail * fs))
    meta.total = t + len(modem.TAPS)

    x = np.zeros(meta.total, np.float32)
    for start, sig in cc_place:
        x[start: start + len(sig)] += sig
    for seg in meta.segments:
        sig = synth(seg.plan, mode, seg.plan.end + 8, fs, seed=seed + seg.start // 997,
                    amp=vis_amp)
        x[seg.start: seg.start + len(sig)] += sig

    x = x + pilots(meta.total, fs, amp=pilot_amp)
    peak = float(np.max(np.abs(x)))
    if peak > 0.99:
        x *= 0.99 / peak
    return x.astype(np.float32), meta
