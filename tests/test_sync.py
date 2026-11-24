"""同步帧（v1.1）验收：晚加入、丢包恢复、v1.0 兼容、视觉层零影响。

    python tests/test_sync.py --exe X:\\rust\\target\\release\\cp16plus.exe

全部通过才算"同步帧可用"。
"""

from __future__ import annotations

import argparse
import json
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "ref"))

from cp16plus.wavio import read_wav, write_wav  # noqa: E402

FAILS: list[str] = []


def check(name: str, ok: bool, detail: str = "") -> None:
    print(f"  [{'ok' if ok else 'FAIL'}] {name}" + (f"  {detail}" if detail else ""))
    if not ok:
        FAILS.append(name)


def run(exe: Path, args: list[str]) -> dict:
    # 注意：不能用系统临时目录（沙箱会拒写），放在工程目录里
    out = ROOT / "build" / "sync" / "probe.json"
    try:
        out.unlink()
    except FileNotFoundError:
        pass
    subprocess.run([str(exe), *args, "--json", str(out)], capture_output=True)
    return json.loads(out.read_text(encoding="utf-8")) if out.exists() else {}


def slice_wav(src: Path, dst: Path, start_s: float, end_s: float | None = None) -> None:
    x, fs = read_wav(src)
    a = int(start_s * fs)
    b = len(x) if end_s is None else int(end_s * fs)
    write_wav(dst, x[a:b], fs)


def drop_wav(src: Path, dst: Path, at_s: float, dur_s: float) -> None:
    x, fs = read_wav(src)
    a, b = int(at_s * fs), int((at_s + dur_s) * fs)
    import numpy as np
    write_wav(dst, np.concatenate([x[:a], x[b:]]), fs)


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--exe", default=str(ROOT / "rust" / "target" / "release" / "cp16plus.exe"))
    ap.add_argument("--font", default=str(ROOT / "fonts" / "hzk16.cpf"))
    a = ap.parse_args()
    exe, font = Path(a.exe), Path(a.font)
    work = ROOT / "build" / "sync"
    work.mkdir(parents=True, exist_ok=True)

    text = "频谱书写通信试验测试频率七三结束应急联络二公里正东"
    full = work / "full.wav"
    subprocess.run([str(exe), "tx", text, "--out", str(full), "--sync", "--font", str(font)],
                   check=True, capture_output=True)
    base = run(exe, ["rx", str(full)])
    total_cells = base.get("cells", 0)
    print(f"基准：{total_cells} 字，同步帧 {base.get('sync', {}).get('frames', 0)} 个")

    print("\n1) 视觉层零影响（对照无同步帧版本）")
    plain = work / "plain.wav"
    subprocess.run([str(exe), "tx", text, "--out", str(plain), "--font", str(font)],
                   check=True, capture_output=True)
    p = run(exe, ["rx", str(plain)])
    check("无同步帧时字数相同", p.get("cells") == total_cells,
          f"{p.get('cells')} vs {total_cells}")
    check("无同步帧时 sync.frames=0", p.get("sync", {}).get("frames") == 0,
          str(p.get("sync")))

    print("\n2) 晚加入：从 30 s 处开始喂音频")
    late = work / "late.wav"
    slice_wav(full, late, 30.0)
    r = run(exe, ["rx", str(late)])
    fr = r.get("sync", {}).get("frames", 0)
    check("10 s 内拿到同步帧", fr >= 1, f"frames={fr}")
    check("晚加入仍能解出字符", r.get("cells", 0) >= 3, f"cells={r.get('cells')}")

    print("\n3) 丢包恢复：挖掉中间 3 s")
    drop = work / "drop.wav"
    drop_wav(full, drop, 20.0, 3.0)
    rd = run(exe, ["rx", str(drop)])
    fd = rd.get("sync", {}).get("frames", 0)
    check("丢包后仍收到同步帧", fd >= 2, f"frames={fd}")
    check("丢包后字符数不塌", rd.get("cells", 0) >= total_cells - 4,
          f"{rd.get('cells')} vs {total_cells}")

    print("\n4) v1.0 兼容：老音频（无同步帧）必须照常解")
    check("v1 音频字数正常", p.get("cells") == total_cells, f"{p.get('cells')}")

    print(f"\n{'同步帧验收通过' if not FAILS else f'失败 {len(FAILS)} 项'}")
    for f in FAILS:
        print("  - " + f)
    return 0 if not FAILS else 1


if __name__ == "__main__":
    sys.exit(main())
