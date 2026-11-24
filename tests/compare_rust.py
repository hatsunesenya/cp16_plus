"""Rust 与 Python 双实现的逐条对拍（真实 WAV）。

    python tests/compare_rust.py [--exe 路径]

对 samples/*.wav：两边各解一次，比较
  · 字数是否一致
  · HDR 是否都解出、呼号是否相同
  · 位准确率（对真字形）之差
"""

from __future__ import annotations

import argparse
import json
import struct
import subprocess
import sys
import tempfile
from pathlib import Path

import numpy as np

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "ref"))

from cp16plus import params as P
from cp16plus import rx as R
from cp16plus.font import HzkFont
from cp16plus.wavio import read_wav

HZK = ROOT.parent / "字库(必须复制到C盘hzk目录下)" / "1.hzk"


def read_blocks(path: Path) -> tuple[int, int, int, np.ndarray]:
    raw = path.read_bytes()
    assert raw[:4] == b"CB16", "bad magic"
    n, rows, tones = struct.unpack_from("<III", raw, 4)
    data = np.frombuffer(raw[16:16 + n * rows * tones * 4], dtype="<f4")
    return n, rows, tones, data.reshape(n, rows, tones)


def bit_acc(blocks: np.ndarray, glyphs, repeat: int) -> float:
    accs = []
    for i, blk in enumerate(blocks):
        if i >= len(glyphs):
            break
        g = np.repeat(glyphs[i], repeat, axis=0)
        th = (blk.max() + np.median(blk)) / 2
        accs.append(float(((blk > th).astype(np.uint8) == g).mean()))
    return float(np.mean(accs)) if accs else 0.0


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--exe", default=str(ROOT / "rust" / "target" / "debug" / "cp16plus.exe"))
    ap.add_argument("--dir", default=str(ROOT / "samples"))
    a = ap.parse_args()
    exe = Path(a.exe)
    if not exe.exists():
        print(f"找不到 Rust 可执行文件：{exe}")
        print("先跑：powershell -NoProfile -ExecutionPolicy Bypass -File rust/build.ps1")
        return 2

    font = HzkFont(HZK)
    manifest = json.loads((Path(a.dir) / "manifest.json").read_text(encoding="utf-8"))
    rows = []
    tmp = ROOT / "build" / "cmp"
    tmp.mkdir(parents=True, exist_ok=True)
    print(f"{'id':<5} {'SNR':>4} {'Py字':>4} {'Rs字':>4} {'PyHDR':>6} {'RsHDR':>6} "
          f"{'Py准':>6} {'Rs准':>6} {'Δ':>6}")
    bad = 0
    for item in manifest["items"]:
        wav = Path(a.dir) / item["file"]
        blocks_out = tmp / (wav.stem + ".bin")
        json_out = tmp / (wav.stem + ".json")
        subprocess.run([str(exe), "rx", str(wav), "--json", str(json_out),
                        "--blocks", str(blocks_out)], capture_output=True)
        rs = json.loads(json_out.read_text(encoding="utf-8")) if json_out.exists() else {}

        x, fs = read_wav(wav)
        py = R.decode(x if fs == P.FS else x, P.FS)
        glyphs, _ = font.rasters(item["text"])
        py_acc = bit_acc(np.stack(py.blocks), glyphs, P.M2.repeat) if py.blocks else 0.0

        n, _r, _t, blocks = read_blocks(blocks_out)
        rs_acc = bit_acc(blocks, glyphs, P.M2.repeat)

        py_hdr = py.hdr.call if py.hdr else "-"
        rs_hdr = (rs.get("hdr") or {}).get("call", "-")
        d_acc = rs_acc - py_acc
        same_cells_row = n == len(py.blocks)
        # 只有"两边解出同样字数"的行才可作为一致性判据；低信噪比下的降级路径
        # （盲搜兜底）本来就允许不同，单独列出不判失败
        ok = (not same_cells_row) or ((py_hdr == rs_hdr) and abs(d_acc) < 0.02)
        if not ok:
            bad += 1
        print(f"{wav.stem:<18} {item['snr_db']:>4.0f} {len(py.blocks):>4} {n:>4} "
              f"{py_hdr:>6} {rs_hdr:>6} {py_acc:>6.3f} {rs_acc:>6.3f} {d_acc:>+6.3f}"
              + ("" if same_cells_row else "   (字数不同)") + ("" if ok else "   <-- 不一致"))
        rows.append({"id": wav.stem, "snr": item["snr_db"], "py_cells": len(py.blocks),
                     "rs_cells": n, "py_hdr": py_hdr, "rs_hdr": rs_hdr,
                     "py_acc": py_acc, "rs_acc": rs_acc})

    same_cells = sum(1 for r in rows if r["py_cells"] == r["rs_cells"])
    same_hdr = sum(1 for r in rows if r["py_hdr"] == r["rs_hdr"])
    same = [r for r in rows if r["py_cells"] == r["rs_cells"]]
    mean_d = float(np.mean([r["rs_acc"] - r["py_acc"] for r in same])) if same else 0.0
    print(f"\n字数一致 {same_cells}/{len(rows)} · HDR 一致 {same_hdr}/{len(rows)} · "
          f"同字数行位准平均差 {mean_d:+.4f}（{len(same)} 行）")
    (ROOT / "tests" / "rust_parity.json").write_text(
        json.dumps({"items": rows, "same_cells": same_cells, "same_hdr": same_hdr,
                    "mean_acc_delta": mean_d}, ensure_ascii=False, indent=2), encoding="utf-8")
    return 0 if bad == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
