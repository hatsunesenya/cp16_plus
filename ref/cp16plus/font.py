"""字模：HZK16 读取、CPF 容器读写、系统字体临时栅格化。

CPF（CP16+ Font）容器格式（草案 v1）::

    magic  b"CPF1"         4
    ver    u8              1
    n      u8              网格边长（16 / 24）
    count  u32 LE          字形数
    flags  u16 LE          保留
    index  count × 6       字符码 u16（GB2312 或 0xFFFF=按序号） + 偏移 u32
    data   count × ceil(n*n/8) 字节，行优先，MSB 在前
"""

from __future__ import annotations

import struct
from pathlib import Path

import numpy as np

HZK_GLYPHS = 8178
HZK_BYTES = 32
HZK_W = HZK_H = 16


# --------------------------------------------------------------------------
# ASCII <-> 全角（HZK16 只有全角拉丁与数字）
# --------------------------------------------------------------------------
def to_fullwidth(text: str) -> str:
    out = []
    for ch in text:
        if ch == " ":
            out.append("\u3000")
        elif 0x21 <= ord(ch) <= 0x7E:
            out.append(chr(ord(ch) + 0xFEE0))
        else:
            out.append(ch)
    return "".join(out)


def to_ascii(text: str) -> str:
    out = []
    for ch in text:
        o = ord(ch)
        if ch == "\u3000":
            out.append(" ")
        elif 0xFF01 <= o <= 0xFF5E:
            out.append(chr(o - 0xFEE0))
        else:
            out.append(ch)
    return "".join(out)


def gb2312_index(ch: str) -> int | None:
    try:
        b = ch.encode("gb2312")
    except UnicodeEncodeError:
        return None
    if len(b) != 2:
        return None
    return (b[0] - 0xA1) * 94 + (b[1] - 0xA1)


# --------------------------------------------------------------------------
class Font:
    """字模源接口。"""

    n: int = 16
    font_id: int = 0
    name: str = "font"

    def raster(self, ch: str) -> np.ndarray | None:      # (n, n) uint8
        raise NotImplementedError

    def rasters(self, text: str) -> tuple[list[np.ndarray], str]:
        """返回能渲染的字形列表与"实际送出的文本"（不可编码字符被跳过）。"""
        out, kept = [], []
        for ch in text:
            r = self.raster(ch)
            if r is not None:
                out.append(r)
                kept.append(ch)
        return out, "".join(kept)


class HzkFont(Font):
    """CRAC CP16 原程序使用的 16×16 GB2312 点阵字库。"""

    def __init__(self, path: str | Path, font_id: int = 1):
        data = Path(path).read_bytes()
        n = len(data) // HZK_BYTES
        bits = np.unpackbits(np.frombuffer(data[: n * HZK_BYTES], dtype=np.uint8))
        self.glyphs = bits.reshape(n, HZK_H, HZK_W)
        self.n = 16
        self.font_id = font_id
        self.name = Path(path).stem

    def raster(self, ch: str) -> np.ndarray | None:
        idx = gb2312_index(to_fullwidth(ch)[0] if ch else ch)
        if idx is None or idx >= len(self.glyphs):
            return None
        return self.glyphs[idx]


class CpfFont(Font):
    def __init__(self, path: str | Path):
        data = Path(path).read_bytes()
        if data[:4] != b"CPF1":
            raise ValueError("not a CPF font")
        _m, _v, n, count, _flags = struct.unpack("<4sBB I H".replace(" ", ""), data[:12])
        self.n = int(n)
        self.name = Path(path).stem
        self.font_id = 2
        idx = 12
        self.map: dict[int, int] = {}
        offsets = []
        for i in range(count):
            code, off = struct.unpack_from("<HI", data, idx)
            idx += 6
            self.map[i if code == 0xFFFF else code] = i
            offsets.append(off)
        self.offsets = offsets
        self.blob = data
        self.stride = (self.n * self.n + 7) // 8

    def raster(self, ch: str) -> np.ndarray | None:
        code = ord(ch)
        i = self.map.get(code)
        if i is None:
            return None
        off = self.offsets[i]
        bits = np.unpackbits(np.frombuffer(self.blob[off:off + self.stride], dtype=np.uint8))
        return bits[: self.n * self.n].reshape(self.n, self.n)


def _resolve_font_file(family: str) -> str:
    """把字体族名解析成文件路径（PIL 在 Windows 上只按文件名找不保险）。"""
    if Path(family).exists():
        return family
    roots = [Path("C:/Windows/Fonts"), Path("/usr/share/fonts"),
             Path("/System/Library/Fonts")]
    cands = {
        "SimHei": ["simhei.ttf"],
        "Microsoft YaHei": ["msyh.ttc"],
        "SimSun": ["simsun.ttc"],
    }.get(family, [])
    for root in roots:
        for name in cands:
            p = root / name
            if p.exists():
                return str(p)
    return family


class SystemFont(Font):
    """用系统字体临时栅格化（仅用于生成样张/字库，不参与协议）。"""
    def __init__(self, family: str = "SimHei", n: int = 16, font_id: int = 3):
        from PIL import Image, ImageDraw, ImageFont

        self.n = n
        self.font_id = font_id
        self.name = f"sys:{family}"
        path = _resolve_font_file(family)
        self._pil = ImageFont.truetype(path, n)
        self._Image = Image
        self._Draw = ImageDraw

    def raster(self, ch: str) -> np.ndarray | None:
        if ch == " ":
            return np.zeros((self.n, self.n), np.uint8)
        im = self._Image.new("L", (self.n, self.n), 0)
        d = self._Draw.Draw(im)
        d.text((0, -1), ch, fill=255, font=self._pil)
        a = np.asarray(im)
        return (a > 110).astype(np.uint8)


# --------------------------------------------------------------------------
class BitmapFont(Font):
    """内存字形表（测试与样张库用）。"""

    def __init__(self, glyphs: dict[str, np.ndarray], name: str = "mem"):
        self.glyphs = glyphs
        self.n = next(iter(glyphs.values())).shape[0]
        self.name = name

    def raster(self, ch: str) -> np.ndarray | None:
        return self.glyphs.get(ch)


def save_cpf(path: str | Path, glyphs: list[tuple[str, np.ndarray]], n: int = 16) -> None:
    stride = (n * n + 7) // 8
    blob = bytearray()
    index = bytearray()
    off = 12 + 6 * len(glyphs)
    for code, g in glyphs:
        index += struct.pack("<HI", ord(code), off + len(blob))
        blob += np.packbits(g.reshape(-1)).tobytes()[:stride]
    head = struct.pack("<4sBB I H", b"CPF1", 1, n, len(glyphs), 0)
    Path(path).write_bytes(head + bytes(index) + bytes(blob))


def load_font(kind: str, path: str | None = None) -> Font:
    if kind == "hzk":
        return HzkFont(path or "")
    if kind == "cpf":
        return CpfFont(path or "")
    if kind == "system":
        return SystemFont(path or "SimHei")
    raise ValueError(f"unknown font kind {kind!r}")
