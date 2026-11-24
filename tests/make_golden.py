"""导出 golden vectors：给 Rust 实现做数值对拍。

    python tests/make_golden.py

产物在 tests/golden/，全部是**裸二进制**（.f32 / .u8）+ manifest.json，
这样 Rust 端不需要任何 npy 解析库，也不引入第三方依赖。
"""

from __future__ import annotations

import json
import sys
from pathlib import Path

import numpy as np

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "ref"))

from cp16plus import fec, modem
from cp16plus import params as P
from cp16plus import tx as T
from cp16plus.font import HzkFont
from cp16plus.frame import Hdr

OUT = ROOT / "tests" / "golden"
OUT.mkdir(parents=True, exist_ok=True)

MANIFEST: dict[str, dict] = {}


def save(name: str, arr: np.ndarray, dtype: str) -> None:
    if dtype == "u8":
        data = np.asarray(arr, np.uint8).tobytes()
    else:
        data = np.asarray(arr, "<f4").tobytes()
    (OUT / name).write_bytes(data)
    MANIFEST[name] = {"dtype": dtype, "shape": list(np.shape(arr))}
    print(f"  {name:<22} {np.shape(arr)} {dtype}")


def main() -> None:
    print("导出 golden vectors")

    modes = {k: {"n": m.n, "df": m.df, "k": m.k, "repeat": m.repeat,
                 "f_lo": m.f_lo, "t_row": m.t_row, "rows_per_char": m.rows_per_char,
                 "row_symbols": m.row_symbols}
             for k, m in P.MODES.items()}
    (OUT / "modes.json").write_text(json.dumps(modes, ensure_ascii=False, indent=2),
                                    encoding="utf-8")
    print("  modes.json")

    hdr = Hdr(call="BG1ABC", grid="OM89AB", length=42, k=2, repeat=1,
              font_id=1, plan=0, utc=733)
    bits = hdr.pack()
    save("hdr_bits.u8", bits, "u8")
    (OUT / "hdr.json").write_text(json.dumps(hdr.__dict__, ensure_ascii=False, indent=2),
                                  encoding="utf-8")

    coded = fec.interleave(fec.conv_encode(bits))
    save("hdr_coded.u8", coded, "u8")

    sig = modem.modulate(coded, amp=0.35)
    save("cc_audio.f32", sig, "f32")
    _, soft, _ = modem.demodulate(sig, len(coded))
    save("cc_soft.f32", soft, "f32")

    font = HzkFont(ROOT.parent / "字库(必须复制到C盘hzk目录下)" / "1.hzk")
    glyphs, _ = font.rasters("频谱书写")
    from cp16plus.vision import plan_cells, synth
    plan = plan_cells(glyphs, P.M2, 0, P.FS)
    save("rows.u8", plan.rows, "u8")
    save("row_margin.u8", plan.margin.astype(np.uint8), "u8")
    vis = synth(plan, P.M2, plan.end + 8, P.FS, seed=42, amp=0.8)
    save("vision_audio.f32", vis[: 64 * 1024], "f32")

    x = T.pilots(int(4 * P.FS), P.FS, amp=0.25)
    from cp16plus import channel as C
    y = C.shift_freq(C.resample_time(x, 1.00007), 12.0)
    save("pilot_audio.f32", y, "f32")
    (OUT / "pilot_case.json").write_text(json.dumps(
        {"a": 1.00007, "b": 12.0, "fs": P.FS, "p1": P.PILOT1, "p2": P.PILOT2},
        indent=2), encoding="utf-8")

    (OUT / "manifest.json").write_text(
        json.dumps({"fs": P.FS, "proto": P.PROTO_VER, "files": MANIFEST},
                   ensure_ascii=False, indent=2), encoding="utf-8")

    # 顺手生成 Rust 的前导常量（保证两边逐比特一致）
    rs = ROOT / "rust" / "crates" / "core" / "src" / "preambles.rs"
    if rs.parent.exists():
        def fmt(arr, name):
            body = "\n".join(
                f"    C64 {{ re: {v.real:.17e}, im: {v.imag:.17e} }},"
                for v in arr)
            return f"pub const {name}: [C64; 32] = [\n{body}\n];"

        src = ("//! 前导序列常量（由 tests/make_golden.py 生成，勿手改）。\n"
               "//!\n//! 两套序列只用于让接收端在不解析载荷的情况下分辨 HDR / BCON 突发。\n\n"
               "use crate::complex::C64;\n\n"
               + fmt(modem.PREAMBLE_HDR, "PRE_A") + "\n\n"
               + fmt(modem.PREAMBLE_BCON, "PRE_B") + "\n")
        rs.write_text(src, encoding="utf-8", newline="\n")
        print(f"  -> {rs.relative_to(ROOT)}")

    total = sum(f.stat().st_size for f in OUT.iterdir())
    print(f"  合计 {total/1024:.0f} KB -> {OUT}")


if __name__ == "__main__":
    main()
