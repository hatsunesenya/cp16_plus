"""控制信道帧：HDR / BCON / EOT 的位布局与打包。

位序统一为"高位在前"，CRC-16/CCITT 覆盖除 CRC 外的全部比特。
"""

from __future__ import annotations

from dataclasses import dataclass, asdict

import numpy as np

from .params import FRAME_MAGIC, PROTO_VER
from .fec import bits_of, crc16, value_of

CALL_ALPHABET = "ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789/ "
GRID_LETTERS = "ABCDEFGHIJKLMNOPQR"


def _call_to_bits(call: str) -> np.ndarray:
    out = []
    for i in range(6):
        ch = call[i] if i < len(call) else " "
        out.append(CALL_ALPHABET.index(ch) if ch in CALL_ALPHABET else 38)
    return np.concatenate([bits_of(v, 6) for v in out])


def _bits_to_call(bits) -> str:
    out = []
    for i in range(6):
        v = value_of(bits[i * 6:(i + 1) * 6])
        out.append(CALL_ALPHABET[v] if v < len(CALL_ALPHABET) else "?")
    return "".join(out).rstrip()


def _grid_to_bits(grid: str) -> np.ndarray:
    g = (grid + "      ")[:6].upper()
    parts = []
    for i in range(2):
        parts.append(bits_of(GRID_LETTERS.index(g[i]) if g[i] in GRID_LETTERS else 0, 5))
    for i in range(2, 4):
        parts.append(bits_of(int(g[i]) if g[i].isdigit() else 0, 4))
    for i in range(4, 6):
        parts.append(bits_of(ord(g[i]) - ord("A") if "A" <= g[i] <= "X" else 0, 5))
    return np.concatenate(parts + [np.zeros(2, np.uint8)])


def _bits_to_grid(bits) -> str:
    a = GRID_LETTERS[value_of(bits[0:5]) % 18]
    b = GRID_LETTERS[value_of(bits[5:10]) % 18]
    c = str(value_of(bits[10:14]) % 10)
    d = str(value_of(bits[14:18]) % 10)
    e = chr(ord("A") + value_of(bits[18:23]) % 24)
    f = chr(ord("A") + value_of(bits[23:28]) % 24)
    return f"{a}{b}{c}{d}{e}{f}"


def mode_bits(raster: int, k: int, repeat: int, font_id: int) -> np.ndarray:
    k_idx = {1: 0, 2: 1, 4: 2, 8: 3}[k]
    return np.concatenate([bits_of(0 if raster == 16 else 1, 2),
                           bits_of(k_idx, 3),
                           bits_of(repeat - 1, 2),
                           bits_of(font_id, 3)])


def bits_mode(bits) -> tuple[int, int, int, int]:
    raster = 16 if value_of(bits[0:2]) == 0 else 24
    k = 1 << value_of(bits[2:5])
    repeat = value_of(bits[5:7]) + 1
    font = value_of(bits[7:10])
    return raster, k, repeat, font


@dataclass
class Hdr:
    call: str = "BG1ABC"
    grid: str = "OM89"
    length: int = 0
    msg_type: int = 0
    sf_id: int = 0
    utc: int = 0
    power: int = 1
    rmap: int = 0
    raster: int = 16
    k: int = 2
    repeat: int = 1
    font_id: int = 1
    plan: int = 0            # 0=默认布局, 1=共存布局

    def pack(self) -> np.ndarray:
        parts = [
            bits_of(FRAME_MAGIC, 16),
            bits_of(PROTO_VER, 4),
            mode_bits(self.raster, self.k, self.repeat, self.font_id),
            _call_to_bits(self.call),
            _grid_to_bits(self.grid),
            bits_of(self.length & 0x3FF, 10),
            bits_of(self.msg_type & 0x7, 3),
            bits_of(self.sf_id & 0xFFF, 12),
            bits_of(self.utc & 0x7FF, 11),
            bits_of(self.power & 0x7, 3),
            bits_of(self.rmap & 0xFF, 8),
            bits_of(self.plan & 0x3, 2),
            np.zeros(4, np.uint8),
        ]
        body = np.concatenate(parts)
        return np.concatenate([body, bits_of(crc16(body), 16)])

    @staticmethod
    def unpack(bits) -> "Hdr | None":
        bits = np.asarray(bits, np.uint8)
        if len(bits) < 165:
            return None
        body, crc = bits[:149], value_of(bits[149:165])
        if value_of(bits[0:16]) != FRAME_MAGIC or value_of(bits[16:20]) != PROTO_VER:
            return None
        if crc16(body) != crc:
            return None
        raster, k, repeat, font = bits_mode(bits[20:30])
        return Hdr(call=_bits_to_call(bits[30:66]),
                   grid=_bits_to_grid(bits[66:96]),
                   length=value_of(bits[96:106]),
                   msg_type=value_of(bits[106:109]),
                   sf_id=value_of(bits[109:121]),
                   utc=value_of(bits[121:132]),
                   power=value_of(bits[132:135]),
                   rmap=value_of(bits[135:143]),
                   plan=value_of(bits[143:145]),
                   raster=raster, k=k, repeat=repeat, font_id=font)


@dataclass
class Bcon:
    sf_id: int = 0
    cell: int = 0
    flags: int = 0
    raster: int = 16
    k: int = 2
    repeat: int = 1
    font_id: int = 1


def pack_bcon(b: Bcon, call: str) -> np.ndarray:
    body = np.concatenate([
        bits_of(b.sf_id & 0xFFF, 12),
        mode_bits(b.raster, b.k, b.repeat, b.font_id),
        bits_of(crc16(_call_to_bits(call)) & 0xFF, 8),
        bits_of(b.cell & 0xFF, 8),
        bits_of(b.flags & 0xFF, 8),
    ])
    return np.concatenate([body, bits_of(crc16(body), 16)])


def unpack_bcon(bits) -> Bcon | None:
    bits = np.asarray(bits, np.uint8)
    if len(bits) < 62:
        return None
    body, crc = bits[:46], value_of(bits[46:62])
    if crc16(body) != crc:
        return None
    raster, k, repeat, font = bits_mode(bits[12:22])
    return Bcon(sf_id=value_of(bits[0:12]), raster=raster, k=k, repeat=repeat,
                font_id=font, cell=value_of(bits[30:38]), flags=value_of(bits[38:46]))


def pack_eot(length: int) -> np.ndarray:
    return np.concatenate([bits_of(0x5A, 8), bits_of(length & 0xFF, 8)])


def is_eot(bits) -> bool:
    bits = np.asarray(bits, np.uint8)
    return len(bits) >= 8 and value_of(bits[:8]) == 0x5A

# --------------------------------------------------------------------------
# 同步帧（v1.1）：104 比特
#   MAGIC8 | SLOT6 | CELL16 | CALL_CRC8 | GRID30 | MODE10 | FONT4 | RSV6 | CRC16
# --------------------------------------------------------------------------
SYNC_MAGIC = 0xC5
SYNC_BITS = 104


@dataclass
class SyncFrame:
    slot: int = 0          # UTC 秒 / 10 mod 60
    cell: int = 0          # 字符计数器：本时隙起点后第一个字符的序号
    call: str = "BG1ABC"
    grid: str = "OM89AB"
    raster: int = 16
    k: int = 2
    font_id: int = 1

    def pack(self) -> np.ndarray:
        b = bits_of(SYNC_MAGIC, 8)
        b = np.concatenate([b, bits_of(self.slot % 60, 6)])
        b = np.concatenate([b, bits_of(self.cell & 0xFFFF, 16)])
        b = np.concatenate([b, bits_of(int(crc16(_call_to_bits(self.call))) & 0xFF, 8)])
        b = np.concatenate([b, _grid_to_bits(self.grid)])
        b = np.concatenate([b, mode_bits(self.raster, self.k, 1, self.font_id)])
        b = np.concatenate([b, bits_of(self.font_id & 0xF, 4)])
        b = np.concatenate([b, np.zeros(6, np.uint8)])
        return np.concatenate([b, bits_of(int(crc16(b)), 16)]).astype(np.uint8)


def unpack_sync(bits) -> SyncFrame | None:
    b = np.asarray(bits, np.uint8)[:SYNC_BITS]
    if b.size < SYNC_BITS:
        return None
    if int(value_of(b[:8])) != SYNC_MAGIC:
        return None
    if int(value_of(b[88:104])) != int(crc16(b[:88])):
        return None
    raster, k, _rep, _font = bits_mode(b[68:78])
    return SyncFrame(
        slot=int(value_of(b[8:14])),
        cell=int(value_of(b[14:30])),
        call="",
        grid=_bits_to_grid(b[38:68]),
        raster=raster,
        k=k,
        font_id=int(value_of(b[78:82])),
    )