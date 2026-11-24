"""统一测试入口：跑全部套件，写出 report.json。

    python tests/run_all.py

套件：
  test_ref.py        协议基本回路（帧/FEC/CC/导频/端到端/多段/锚点）
  test_matrix.py     档位 × 重复 × 分段 矩阵
  test_robustness.py 异常输入、采样率、信道损伤、确定性、性能
  rust golden        （如果已构建）Python ↔ Rust 数值对拍
"""

from __future__ import annotations

import json
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
PY = sys.executable


def run(name: str, cmd: list[str], tail: int = 6) -> dict:
    t0 = time.perf_counter()
    p = subprocess.run(cmd, cwd=str(ROOT), capture_output=True, text=True,
                       encoding="utf-8", errors="replace")
    dt = time.perf_counter() - t0
    lines = [ln for ln in (p.stdout or "").strip().splitlines() if ln.strip()]
    ok = p.returncode == 0
    print(f"{'✓' if ok else '✗'} {name:<22} {dt:5.1f}s")
    for ln in lines[-tail:]:
        print("    " + ln)
    if not ok and p.stderr.strip():
        print("    stderr: " + p.stderr.strip().splitlines()[-1])
    return {"name": name, "ok": ok, "seconds": round(dt, 2),
            "tail": lines[-tail:], "returncode": p.returncode}


def main() -> int:
    suites = []
    suites.append(run("协议回路", [PY, str(ROOT / "tests" / "test_ref.py")]))
    suites.append(run("档位矩阵", [PY, str(ROOT / "tests" / "test_matrix.py")]))
    suites.append(run("鲁棒性", [PY, str(ROOT / "tests" / "test_robustness.py")]))
    suites.append(run("信噪比扫描", [PY, str(ROOT / "tests" / "test_snr_sweep.py")], tail=12))

    exe = ROOT / "rust" / "target" / "debug" / "cp16plus.exe"
    golden = ROOT / "tests" / "golden"
    if exe.exists() and (golden / "hdr_bits.u8").exists():
        suites.append(run("Rust golden", [str(exe), "golden", str(golden)]))
    else:
        print("· Rust golden      跳过（未构建或缺少 golden）")

    rel = ROOT / "rust" / "target" / "release" / "cp16plus.exe"
    cmp_exe = rel if rel.exists() else exe
    if cmp_exe.exists() and (ROOT / "samples" / "manifest.json").exists():
        suites.append(run("Rust 对拍", [PY, str(ROOT / "tests" / "compare_rust.py"),
                                        "--exe", str(cmp_exe)], tail=3))
    else:
        print("· Rust 对拍        跳过（未构建或没有样张）")

    total = len(suites)
    passed = sum(1 for s in suites if s["ok"])
    report = {
        "when": time.strftime("%Y-%m-%d %H:%M:%S"),
        "python": sys.version.split()[0],
        "suites": suites,
        "passed": passed,
        "total": total,
    }
    out = ROOT / "tests" / "report.json"
    out.write_text(json.dumps(report, ensure_ascii=False, indent=2), encoding="utf-8")

    print(f"\n{passed}/{total} 套件通过  ->  {out.relative_to(ROOT)}")
    return 0 if passed == total else 1


if __name__ == "__main__":
    sys.exit(main())
