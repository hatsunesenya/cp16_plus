"""可读性代理指标 J：不依赖 OCR，只看图像质量。

η 双峰分离度、κ 笔画连通率、ν 孤立噪点率、ρ 重复一致性、σ_t 时间稳定性。
"""

from __future__ import annotations

import numpy as np


def to_db(P: np.ndarray, floor: float) -> np.ndarray:
    return 10 * np.log10(np.asarray(P, float) / max(floor, 1e-30) + 1e-12)


def otsu(img: np.ndarray) -> tuple[float, float]:
    """返回 (阈值, 分离度 η)。img 已归一化到 0..1。"""
    flat = np.clip(img.reshape(-1), 0, 1)
    hist, edges = np.histogram(flat, bins=64, range=(0, 1))
    p = hist / max(hist.sum(), 1)
    centers = 0.5 * (edges[:-1] + edges[1:])
    w0 = np.cumsum(p)
    m0 = np.cumsum(p * centers)
    mt = m0[-1]
    denom = w0 * (1 - w0)
    with np.errstate(divide="ignore", invalid="ignore"):
        sb = (mt * w0 - m0) ** 2 / denom
    sb = np.nan_to_num(sb)
    j = int(np.argmax(sb))
    var = float(np.var(flat)) + 1e-12
    eta = float(sb[j] / var)
    return float(centers[j]), float(np.clip(eta, 0, 1))


def binarize(img: np.ndarray, thr: float) -> np.ndarray:
    return (img > thr).astype(np.uint8)


def _neighbours(b: np.ndarray, diag: bool = False) -> np.ndarray:
    k = np.array([[1, 1, 1], [1, 0, 1], [1, 1, 1]]) if diag else np.array([[0, 1, 0], [1, 0, 1], [0, 1, 0]])
    pad = np.pad(b, 1)
    out = np.zeros_like(b, dtype=int)
    h, w = b.shape
    for dy in (-1, 0, 1):
        for dx in (-1, 0, 1):
            if k[dy + 1, dx + 1]:
                out += pad[1 + dy:1 + dy + h, 1 + dx:1 + dx + w]
    return out


def cell_metrics(img: np.ndarray, thr: float | None = None) -> dict:
    eta_thr, eta = otsu(img)
    t = eta_thr if thr is None else thr
    b = binarize(img, t)
    ink = int(b.sum())
    if ink == 0:
        return {"eta": eta, "kappa": 0.0, "nu": 1.0, "ink": 0, "thr": float(t)}
    nb4 = _neighbours(b, diag=False)
    nb8 = _neighbours(b, diag=True)
    kappa = float(np.mean(nb4[b > 0] > 0))
    nu = float(np.mean(nb8[b > 0] == 0))
    return {"eta": eta, "kappa": kappa, "nu": nu, "ink": ink, "thr": float(t)}


def repeat_consistency(P_cell_rows: np.ndarray) -> float:
    """R 份重复行之间的相关系数（无重复时返回 1）。"""
    n = len(P_cell_rows)
    if n < 2:
        return 1.0
    a = 10 * np.log10(P_cell_rows[0::2][: min(len(P_cell_rows[0::2]), len(P_cell_rows[1::2]))] + 1e-12)
    b = 10 * np.log10(P_cell_rows[1::2][: len(a)] + 1e-12)
    if a.size < 2:
        return 1.0
    a = a.reshape(-1)
    b = b.reshape(-1)
    if np.std(a) < 1e-9 or np.std(b) < 1e-9:
        return 0.0
    return float(np.clip(np.corrcoef(a, b)[0, 1], -1, 1))


def j_score(eta: float, kappa: float, nu: float, rho: float = 1.0, sigma_t: float = 0.0) -> float:
    return 0.35 * eta + 0.25 * kappa - 0.25 * nu + 0.15 * rho - 0.10 * sigma_t


def blocks_metrics(blocks: list[np.ndarray], n: int, repeat: int,
                   lo_db: float = 6.0, hi_db: float = 18.0) -> dict:
    """对全部字符块取中位数。

    `blocks` 里每个元素是 (n*repeat, N) 的功率块；噪声底用全帧统一估计，
    避免逐块归一造成的"越密越淡"。
    """
    if not blocks:
        return {"eta": 0.0, "kappa": 0.0, "nu": 1.0, "rho": 0.0, "J": 0.0, "cells": 0}
    floor = float(np.percentile(np.concatenate([b.reshape(-1) for b in blocks]), 20))
    rows = []
    for blk in blocks:
        img = np.clip((to_db(blk, floor) - lo_db) / (hi_db - lo_db), 0, 1)
        m = cell_metrics(img)
        m["rho"] = repeat_consistency(blk) if repeat > 1 else 1.0
        rows.append(m)
    out = {k: float(np.median([r[k] for r in rows])) for k in ("eta", "kappa", "nu", "rho")}
    out["J"] = j_score(out["eta"], out["kappa"], out["nu"], out["rho"])
    out["cells"] = len(rows)
    return out
