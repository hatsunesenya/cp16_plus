//! 接收流水线：导频标定 → CC 前导锚定 → 分段解调 → 字符块 + 指标。
//!
//! 与 Python `rx.py` 同构；差异只在指标（Rust 端先做 η/κ/ν 三项，ρ 待 R>1 的用例补齐）。

use crate::fec;
use crate::frame::Hdr;
use crate::modem::{self, Burst};
use crate::params::{self, Mode};
use crate::pilot::{self, PilotEst};
use crate::vision;

#[derive(Clone, Copy, Debug, Default)]
pub struct Metrics {
    pub eta: f64,
    pub kappa: f64,
    pub nu: f64,
    pub rho: f64,
    pub j: f64,
}

pub struct RxResult {
    pub hdr: Option<Hdr>,
    pub est: PilotEst,
    pub mode: Mode,
    pub bursts: Vec<Burst>,
    pub cells: usize,
    pub blocks: Vec<Vec<Vec<f64>>>,
    pub anchors: Vec<&'static str>,
    pub metrics: Metrics,
    pub t_row: f64,
    /// 同步帧（v1.1）：收到几帧、最后一帧的时隙与字符计数
    pub sync_frames: usize,
    pub sync_slot: u8,
    pub sync_cell: u16,
}

fn mode_from_hdr(h: &Hdr) -> Mode {
    let (df, f_lo) = if h.plan == 0 { (25.0, 800.0) } else { (18.75, 640.0) };
    Mode::new("hdr", h.raster as usize, df, h.k, h.repeat as usize, f_lo)
}

/// Otsu 阈值与分离度（输入已归一化到 0..1）。
fn otsu(vals: &[f64]) -> (f64, f64) {
    const BINS: usize = 64;
    let mut hist = [0.0f64; BINS];
    for &v in vals {
        let b = ((v.clamp(0.0, 1.0)) * (BINS as f64 - 1.0)).round() as usize;
        hist[b.min(BINS - 1)] += 1.0;
    }
    let total: f64 = hist.iter().sum();
    if total <= 0.0 {
        return (0.5, 0.0);
    }
    let mean: f64 = (0..BINS)
        .map(|i| hist[i] / total * (i as f64 + 0.5) / BINS as f64)
        .sum();
    let var: f64 = (0..BINS)
        .map(|i| {
            let x = (i as f64 + 0.5) / BINS as f64;
            hist[i] / total * (x - mean) * (x - mean)
        })
        .sum();
    let (mut w0, mut m0, mut best, mut best_k) = (0.0f64, 0.0f64, 0.0f64, 0usize);
    for k in 0..BINS {
        w0 += hist[k] / total;
        m0 += hist[k] / total * (k as f64 + 0.5) / BINS as f64;
        let w1 = 1.0 - w0;
        if w0 <= 0.0 || w1 <= 0.0 {
            continue;
        }
        let sb = (mean * w0 - m0) * (mean * w0 - m0) / (w0 * w1);
        if sb > best {
            best = sb;
            best_k = k;
        }
    }
    let eta = if var > 1e-12 { (best / var).clamp(0.0, 1.0) } else { 0.0 };
    ((best_k as f64 + 1.0) / BINS as f64, eta)
}

/// 供流式解码器复用同一套指标计算。
pub fn block_metrics_public(blocks: &[Vec<Vec<f64>>]) -> Metrics {
    block_metrics(blocks)
}

fn block_metrics(blocks: &[Vec<Vec<f64>>]) -> Metrics {
    if blocks.is_empty() {
        return Metrics { nu: 1.0, ..Default::default() };
    }
    // 全帧统一噪声底（20 分位）
    let mut all: Vec<f64> = blocks.iter().flat_map(|b| b.iter().flatten().copied()).collect();
    all.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let floor = all[(all.len() as f64 * 0.2) as usize % all.len().max(1)].max(1e-30);

    let mut etas = Vec::new();
    let mut kappas = Vec::new();
    let mut nus = Vec::new();
    for blk in blocks {
        let img: Vec<Vec<f64>> = blk
            .iter()
            .map(|row| {
                row.iter()
                    .map(|&p| {
                        let db = 10.0 * (p / floor).max(1e-12).log10();
                        ((db - 6.0) / 12.0).clamp(0.0, 1.0)
                    })
                    .collect()
            })
            .collect();
        let flat: Vec<f64> = img.iter().flatten().copied().collect();
        let (thr, eta) = otsu(&flat);
        etas.push(eta);

        let h = img.len();
        let w = if h > 0 { img[0].len() } else { 0 };
        let ink = |y: isize, x: isize| -> bool {
            y >= 0 && x >= 0 && (y as usize) < h && (x as usize) < w
                && img[y as usize][x as usize] > thr
        };
        let (mut ink_n, mut conn, mut lonely) = (0usize, 0usize, 0usize);
        for y in 0..h {
            for x in 0..w {
                if !ink(y as isize, x as isize) {
                    continue;
                }
                ink_n += 1;
                let n4 = [(0, -1), (0, 1), (-1, 0), (1, 0)]
                    .iter()
                    .filter(|(dy, dx)| ink(y as isize + dy, x as isize + dx))
                    .count();
                if n4 > 0 {
                    conn += 1;
                }
                let n8 = (-1..=1)
                    .flat_map(|dy| (-1..=1).map(move |dx| (dy, dx)))
                    .filter(|(dy, dx)| !(*dy == 0 && *dx == 0))
                    .filter(|(dy, dx)| ink(y as isize + dy, x as isize + dx))
                    .count();
                if n8 == 0 {
                    lonely += 1;
                }
            }
        }
        if ink_n > 0 {
            kappas.push(conn as f64 / ink_n as f64);
            nus.push(lonely as f64 / ink_n as f64);
        }
    }
    let med = |v: &mut Vec<f64>| -> f64 {
        if v.is_empty() {
            return 0.0;
        }
        v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        v[v.len() / 2]
    };
    let eta = med(&mut etas);
    let kappa = med(&mut kappas);
    let nu = if nus.is_empty() { 1.0 } else { med(&mut nus) };
    let rho = 1.0;
    Metrics {
        eta,
        kappa,
        nu,
        rho,
        j: 0.35 * eta + 0.25 * kappa - 0.25 * nu + 0.15 * rho,
    }
}

pub fn decode(x: &[f32], fs: usize, force_mode: Option<Mode>) -> RxResult {
    // 单一管线：离线解码就是把整段音频按 20 ms 喂给同一个流式解码器。
    // 这样"实时"和"WAV"物理上不存在第二条实现，行为差异无从产生。
    let mut st = crate::rt::Stream::new(fs, force_mode);
    let block = (fs / 50).max(64);
    for chunk in x.chunks(block) {
        let _ = st.feed(chunk);
    }
    let _ = st.finish();
    st.snapshot()
}
