"""cp16plus 命令行：发信、收信、生成样张库、跑信噪比扫描。"""

from __future__ import annotations

import argparse
import json
from pathlib import Path

import numpy as np

from . import channel as C
from . import display
from . import params as P
from . import rx as R
from . import tx as T
from .font import HzkFont, SystemFont


def _default_hzk() -> str:
    here = Path(__file__).resolve()
    for cand in (here.parents[3] / "字库(必须复制到C盘hzk目录下)" / "1.hzk",
                 Path("C:/hzk/1.hzk")):
        if cand.exists():
            return str(cand)
    return ""


def _font(kind: str, path: str | None):
    if kind == "hzk":
        return HzkFont(path or _default_hzk())
    if kind == "system":
        return SystemFont(path or "SimHei", 16)
    raise ValueError(kind)


def _render(res: R.RxResult, px_tone: int = 10, clean: bool = False) -> np.ndarray:
    """字形必须是方的：1 个音与 1 个逻辑行等宽（R>1 时行像素宽度按 R 折算）。"""
    raster = res.raster()
    if raster.size == 0:
        return np.zeros((10, 10, 3), np.uint8)
    rep = max(res.mode.repeat if res.mode else 1, 1)
    return display.render(raster, px_per_tone=px_tone,
                          px_per_row=max(px_tone // rep, 1), clean=clean)


# --------------------------------------------------------------------------
def cmd_tx(a) -> None:
    font = _font(a.font, a.font_path)
    glyphs, kept = font.rasters(a.text)
    if not glyphs:
        raise SystemExit("没有可发送的字形")
    mode = P.by_name(a.mode)
    x, meta = T.build(glyphs, mode, seed=a.seed, text=kept, call=a.call,
                      grid=a.grid, font_id=font.font_id)
    from .wavio import write_wav
    write_wav(a.out, x, P.FS)
    info = {
        "text": kept, "mode": mode.name, "call": a.call, "grid": a.grid,
        "seconds": round(len(x) / P.FS, 2), "samples": int(len(x)),
        "cells": meta.n_cells, "segments": len(meta.segments),
        "bursts": [{"kind": b.kind, "t": round(b.start / P.FS, 3),
                    "symbols": b.symbols} for b in meta.bursts],
    }
    Path(a.out).with_suffix(".json").write_text(
        json.dumps(info, ensure_ascii=False, indent=2), encoding="utf-8")
    print(json.dumps(info, ensure_ascii=False))


def cmd_rx(a) -> None:
    from .wavio import read_wav, resample_linear
    x, fs = read_wav(a.inp)
    if fs != P.FS:
        x = resample_linear(x, fs, P.FS)
    res = R.decode(x, P.FS)
    out = {
        "ok": res.ok,
        "hdr": (res.hdr.__dict__ if res.hdr else None),
        "mode": (res.mode.name if res.mode else None),
        "a": round(res.est.a, 9), "b": round(res.est.b, 4),
        "pilot_conf": round(res.est.conf, 3),
        "cc_conf": round(res.cc_conf, 3),
        "bursts": [{"t": round(b[0] / P.FS, 3), "kind": b[1], "score": round(b[2], 2)}
                   for b in res.bursts],
        "segments": [{"t": round(s.start / P.FS, 3), "rows": s.n_rows,
                      "cells": s.n_cells, "anchor": s.anchor} for s in res.segments],
        "cells": res.n_cells,
        "metrics": {k: round(float(v), 3) for k, v in res.metrics.items()},
    }
    if a.preview:
        display.save_png(a.preview, _render(res, clean=a.clean))
        out["preview"] = a.preview
    print(json.dumps(out, ensure_ascii=False, indent=2))


# --------------------------------------------------------------------------
MESSAGES = [
    ("频谱书写通信试验", "中文短句"),
    ("ＢＧ１ＡＢＣ　５９９", "呼号与报告"),
    ("应急联络　二公里正东", "应急报文"),
    ("频率测试　七三　结束", "通联收尾"),
]


def _write_truth(path: Path, glyphs) -> None:
    """把真值字模写成裸二进制：GT16 + n + rows + cols + bits，供 Rust 端算准确率。"""
    import struct

    n = len(glyphs)
    rows, cols = glyphs[0].shape
    head = b"GT16" + struct.pack("<III", n, rows, cols)
    body = b"".join(np.asarray(g, np.uint8).tobytes() for g in glyphs)
    path.write_bytes(head + body)


def cmd_samples(a) -> None:
    """生成样张库：文件名带参数，另附真值字模与索引表。"""
    from .wavio import write_wav

    outdir = Path(a.out)
    outdir.mkdir(parents=True, exist_ok=True)
    (outdir / "png").mkdir(exist_ok=True)
    font = _font(a.font, a.font_path)
    mode = P.by_name(a.mode)

    snrs = [float(s) for s in a.snrs.split(",")]
    rows = []
    for mi, (text, cn) in enumerate(MESSAGES, start=1):
        glyphs, kept = font.rasters(text)
        x, meta = T.build(glyphs, mode, seed=11 + mi, text=text, call="BG1ABC",
                          grid="OM89AB", font_id=font.font_id)
        for snr in snrs:
            tag = f"m{mi}_snr{snr:+.0f}_{mode.name}".replace("+", "p").replace("-", "m")
            wav = outdir / f"{tag}.wav"
            y = C.awgn(x, snr, seed=100 + mi * 10 + int(abs(snr)))
            write_wav(wav, y, P.FS)
            _write_truth(outdir / f"{tag}.truth", glyphs)

            res = R.decode(y, P.FS)
            png = outdir / "png" / f"{tag}.png"
            display.save_png(png, _render(res))

            acc, cells = _acc(res, glyphs, mode.repeat)
            rows.append({
                "file": wav.name, "msg": mi, "text": text, "cn": cn,
                "chars": len(glyphs), "snr_db": snr, "mode": mode.name,
                "cells": res.n_cells, "acc": round(acc, 3),
                "hdr": bool(res.hdr is not None),
                "J": round(float(res.metrics["J"]), 3),
                "anchor": res.segments[0].anchor if res.segments else "none",
                "png": f"png/{tag}.png", "truth": f"{tag}.truth",
            })
            print(f"  {tag:<18} {text[:8]:<10} {snr:>5.0f} dB  {cells}/{len(glyphs)} 字  "
                  f"准 {acc:.3f}  J {res.metrics['J']:.2f}")

    # 索引表：Excel 直接打开
    import csv

    with open(outdir / "index.csv", "w", newline="", encoding="utf-8-sig") as fh:
        w = csv.DictWriter(fh, fieldnames=list(rows[0]))
        w.writeheader()
        w.writerows(rows)

    (outdir / "manifest.json").write_text(
        json.dumps({"mode": mode.name, "fs": P.FS, "snrs": snrs,
                    "items": rows}, ensure_ascii=False, indent=2), encoding="utf-8")

    (outdir / "README.txt").write_text(
        "CP16+ 测试音频\n"
        "=============\n"
        "文件名规则： m<报文号>_snr<信噪比>_<档位>.wav\n"
        "  例：m1_snrm09_M2.wav  表示第 1 条报文、-9 dB、M2 档\n"
        "（文件名里 m 代表负号，p 代表正号）\n\n"
        "每个 wav 配一个同名 .truth（真值点阵，供自动打分）。\n"
        "index.csv 里有：文件名 / 报文号 / 真值文本 / 字数 / 信噪比 / 档位。\n"
        "png/ 下是解码后的瀑布预览。\n\n"
        "一键检查：\n"
        "  cp16plus check .            （Rust CLI，读 index.csv 逐条解码并打分）\n",
        encoding="utf-8")

    print(f"\n共 {len(rows)} 条样张 -> {outdir}")
    print("一键检查：cp16plus check " + str(outdir))


def _acc(res: R.RxResult, glyphs, repeat: int) -> tuple[float, int]:
    accs = []
    for i, blk in enumerate(res.blocks):
        if i >= len(glyphs):
            break
        g = np.repeat(glyphs[i], repeat, axis=0)
        th = (blk.max() + np.median(blk)) / 2
        accs.append(float(((blk > th).astype(np.uint8) == g).mean()))
    return (float(np.mean(accs)) if accs else 0.0), len(accs)


def cmd_mkfont(a) -> None:
    """把 HZK16 导成 CPF（按 Unicode 码点索引），供 Rust 端直接读取。"""
    from .font import save_cpf, to_fullwidth

    font = HzkFont(a.hzk or _default_hzk())
    pairs = []
    seen = set()
    # ASCII 走全角，其余按 GB2312 区位反查
    for o in range(0x20, 0x7F):
        ch = to_fullwidth(chr(o))
        g = font.raster(ch)
        if g is not None and ord(ch) not in seen:
            seen.add(ord(ch))
            pairs.append((ch, g))
    for zone in range(1, 88):
        for pos in range(1, 95):
            try:
                ch = bytes([zone + 0xA0, pos + 0xA0]).decode("gb2312")
            except UnicodeDecodeError:
                continue
            g = font.raster(ch)
            if g is None or ord(ch) in seen:
                continue
            seen.add(ord(ch))
            pairs.append((ch, g))

    out = Path(a.out)
    out.parent.mkdir(parents=True, exist_ok=True)
    save_cpf(out, pairs, 16)
    print(f"{len(pairs)} 字形 -> {out}  ({out.stat().st_size/1024:.0f} KB)")


def cmd_bench(a) -> None:
    """信噪比扫描：视觉位准确率 / HDR 成功率 / 锚定方式 / J。"""
    font = _font(a.font, a.font_path)
    mode = P.by_name(a.mode)
    glyphs, _ = font.rasters(a.text)
    x, meta = T.build(glyphs, mode, seed=3, text=a.text)
    print(f"{'SNR':>5} {'位准':>6} {'字符准':>7} {'HDR':>4} {'字数':>4} {'锚定':>6} {'J':>5}")
    for snr in [float(s) for s in a.snrs.split(",")]:
        accs, chars, anchors = [], 0.0, set()
        for trial in range(a.trials):
            y = C.awgn(x, snr, seed=200 + trial)
            y = C.shift_freq(C.resample_time(y, 1 + (trial - 1) * 4e-5),
                             (trial - 1) * 6.0)
            res = R.decode(y, P.FS)
            anchors.update(s.anchor for s in res.segments)
            hit = 0
            for i, blk in enumerate(res.blocks):
                if i >= len(glyphs):
                    break
                g = np.repeat(glyphs[i], mode.repeat, axis=0)
                th = (blk.max() + np.median(blk)) / 2
                acc = float(((blk > th).astype(np.uint8) == g).mean())
                accs.append(acc)
                hit += acc > 0.95
            chars += hit / max(len(glyphs), 1)
        print(f"{snr:>5.0f} {np.mean(accs) if accs else 0:>6.3f} "
              f"{chars / max(a.trials,1):>7.2f} {'OK' if res.hdr else '--':>4} "
              f"{res.n_cells:>4} {'/'.join(sorted(anchors)):>6} {res.metrics['J']:>5.2f}")


def main(argv=None) -> None:
    ap = argparse.ArgumentParser(prog="cp16plus")
    sub = ap.add_subparsers(dest="cmd", required=True)

    t = sub.add_parser("tx", help="文本 -> WAV")
    t.add_argument("text")
    t.add_argument("--out", default="out.wav")
    t.add_argument("--mode", default=P.DEFAULT_MODE)
    t.add_argument("--font", default="hzk")
    t.add_argument("--font-path", default=None)
    t.add_argument("--call", default="BG1ABC")
    t.add_argument("--grid", default="OM89AB")
    t.add_argument("--seed", type=int, default=1)
    t.set_defaults(func=cmd_tx)

    r = sub.add_parser("rx", help="WAV -> 参数/指标/预览")
    r.add_argument("inp")
    r.add_argument("--preview", default=None)
    r.add_argument("--clean", action="store_true")
    r.set_defaults(func=cmd_rx)

    s = sub.add_parser("samples", help="生成样张库")
    s.add_argument("--out", default="samples")
    s.add_argument("--mode", default=P.DEFAULT_MODE)
    s.add_argument("--font", default="hzk")
    s.add_argument("--font-path", default=None)
    s.add_argument("--snrs", default="-9,-6,-3,0,3,6")
    s.set_defaults(func=cmd_samples)

    m = sub.add_parser("mkfont", help="HZK16 -> CPF 字库（给 Rust 端用）")
    m.add_argument("--hzk", default=None)
    m.add_argument("--out", default="fonts/hzk16.cpf")
    m.set_defaults(func=cmd_mkfont)

    b = sub.add_parser("bench", help="信噪比扫描")
    b.add_argument("--text", default="频谱书写通信")
    b.add_argument("--mode", default=P.DEFAULT_MODE)
    b.add_argument("--font", default="hzk")
    b.add_argument("--font-path", default=None)
    b.add_argument("--snrs", default="-12,-9,-6,-3,0,3,6,10")
    b.add_argument("--trials", type=int, default=3)
    b.set_defaults(func=cmd_bench)

    a = ap.parse_args(argv)
    a.func(a)


if __name__ == "__main__":
    main()
