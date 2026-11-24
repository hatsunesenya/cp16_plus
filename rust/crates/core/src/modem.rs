//! 控制信道物理层：DQPSK 125 Bd + RRC 成形（与 Python `modem.py` 对齐）。

use crate::complex::C64;
use crate::params::{CC_RRC_ALPHA};
use crate::preambles::{PRE_A, PRE_B};

pub fn rrc_taps(alpha: f64, sps: usize, span: usize) -> Vec<f64> {
    let n = span * sps;
    let mut h = vec![0.0f64; 2 * n + 1];
    for (i, item) in h.iter_mut().enumerate() {
        let t = (i as f64 - n as f64) / sps as f64;
        let v = if t.abs() < 1e-9 {
            1.0 - alpha + 4.0 * alpha / std::f64::consts::PI
        } else if (t.abs() - 1.0 / (4.0 * alpha)).abs() < 1e-9 {
            (alpha / std::f64::consts::SQRT_2)
                * ((1.0 + 2.0 / std::f64::consts::PI)
                    * (std::f64::consts::PI / (4.0 * alpha)).sin()
                    + (1.0 - 2.0 / std::f64::consts::PI)
                        * (std::f64::consts::PI / (4.0 * alpha)).cos())
        } else {
            let pi = std::f64::consts::PI;
            ((pi * t * (1.0 - alpha)).sin() + 4.0 * alpha * t * (pi * t * (1.0 + alpha)).cos())
                / (pi * t * (1.0 - (4.0 * alpha * t).powi(2)))
        };
        *item = v;
    }
    let energy: f64 = h.iter().map(|v| v * v).sum::<f64>().sqrt();
    h.iter().map(|v| v / energy).collect()
}

/// 收发各一次 RRC 造成的群延迟（样点）
pub fn match_delay(_alpha: f64, sps: usize, span: usize) -> i64 {
    (2 * span * sps) as i64
}

fn syms_from_bits(bits: &[u8]) -> Vec<C64> {
    let inv = 1.0 / 2.0f64.sqrt();
    bits.chunks(2)
        .map(|c| {
            let b1 = c.first().copied().unwrap_or(0) as f64;
            let b2 = c.get(1).copied().unwrap_or(0) as f64;
            C64::new((1.0 - 2.0 * b1) * inv, (1.0 - 2.0 * b2) * inv)
        })
        .collect()
}

/// 差分编码 + 脉冲成形 + 搬移到载频。
pub fn modulate(bits: &[u8], fs: usize, center: f64, baud: f64, amp: f64,
                pre: &[C64; 32]) -> Vec<f32> {
    let sps = (fs as f64 / baud).round() as usize;
    let taps = rrc_taps(CC_RRC_ALPHA, sps, 8);
    let data = syms_from_bits(bits);

    let mut syms: Vec<C64> = Vec::with_capacity(pre.len() + data.len());
    syms.extend_from_slice(pre);
    let mut acc = pre[pre.len() - 1];
    for s in &data {
        acc = acc * *s;
        syms.push(acc);
    }

    // 上采样到样点率（冲激串），再做 RRC
    let ups = syms.len() * sps;
    let mut base = vec![C64::ZERO; ups + taps.len() - 1];
    for (k, s) in syms.iter().enumerate() {
        let at = k * sps;
        for (j, &h) in taps.iter().enumerate() {
            base[at + j] = base[at + j] + *s * h;
        }
    }

    base.iter()
        .enumerate()
        .map(|(n, v)| {
            let t = n as f64 / fs as f64;
            let rot = C64::exp_i(2.0 * std::f64::consts::PI * center * t);
            ((*v * rot).re * amp) as f32
        })
        .collect()
}

pub fn preamble_hdr() -> &'static [C64; 32] {
    &PRE_A
}

pub fn preamble_bcon() -> &'static [C64; 32] {
    &PRE_B
}

/// 前导 C：周期同步帧（v1.1）。v1.0 接收端对它的相关≈0，会自然忽略。
pub fn preamble_sync() -> &'static [C64; 32] {
    &crate::preambles::PRE_C
}

// --------------------------------------------------------------------------
// 突发检测（接收端）：匹配滤波 + 双前导归一化相关 + 起跳/能量/NMS 三重筛选
// --------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
pub struct Burst {
    /// 音频域符号起点（已扣掉收发两级 RRC 的群延迟）
    pub start: i64,
    pub kind: &'static str,
    pub score: f64,
}

fn baseband(x: &[f32], fs: usize, center: f64) -> Vec<C64> {
    x.iter()
        .enumerate()
        .map(|(n, &v)| {
            let t = n as f64 / fs as f64;
            C64::exp_i(-2.0 * std::f64::consts::PI * center * t) * (2.0 * v as f64)
        })
        .collect()
}

fn convolve(a: &[C64], h: &[f64]) -> Vec<C64> {
    let mut out = vec![C64::ZERO; a.len() + h.len() - 1];
    for (i, v) in a.iter().enumerate() {
        if v.re == 0.0 && v.im == 0.0 {
            continue;
        }
        for (j, &c) in h.iter().enumerate() {
            let o = &mut out[i + j];
            o.re += v.re * c;
            o.im += v.im * c;
        }
    }
    out
}

pub fn score_at(mf: &[C64], pre: &[C64], sps: usize, start: i64) -> f64 {
    let n = pre.len() as i64;
    if start < 0 || start + n * sps as i64 > mf.len() as i64 {
        return 0.0;
    }
    let (mut re, mut im, mut e) = (0.0f64, 0.0f64, 0.0f64);
    for k in 0..pre.len() {
        let y = mf[start as usize + k * sps];
        re += y.re * pre[k].re + y.im * pre[k].im;
        im += y.im * pre[k].re - y.re * pre[k].im;
        e += y.norm2();
    }
    let num = (re * re + im * im).sqrt();
    let den = e.sqrt() * (pre.len() as f64).sqrt() + 1e-12;
    num / den
}

pub fn energy_at(mf: &[C64], a: i64, b: i64) -> f64 {
    let lo = a.max(0) as usize;
    let hi = (b.max(0) as usize).min(mf.len());
    if hi <= lo {
        return 0.0;
    }
    mf[lo..hi].iter().map(|v| v.norm2()).sum::<f64>() / (hi - lo) as f64
}

/// 匹配滤波输出（复基带 → RRC）。一条报文只算一次，避免每个突发重算。
pub fn matched_filter(x: &[f32], fs: usize, center: f64, baud: f64) -> Vec<C64> {
    let sps = (fs as f64 / baud).round() as usize;
    let taps = rrc_taps(CC_RRC_ALPHA, sps, 8);
    convolve(&baseband(x, fs, center), &taps)
}

/// 在匹配滤波输出上找突发（不需要原始样点）。
pub fn find_bursts_mf(mf: &[C64], fs: usize, baud: f64,
                      rel: f64, min_sep_sym: usize) -> Vec<Burst> {
    let sps = (fs as f64 / baud).round() as usize;
    let step = 2usize;
    let n_pre = PRE_A.len();
    let span = mf.len() as i64 - (n_pre * sps) as i64;
    if span <= 1 {
        return Vec::new();
    }

    let mut cand: Vec<(i64, &'static str, f64)> = Vec::new();
    let mut i = 0i64;
    while i < span {
        let sa = score_at(mf, &PRE_A, sps, i);
        let sb = score_at(mf, &PRE_B, sps, i);
        let (sc, kind) = if sa >= sb { (sa, "HDR") } else { (sb, "BCON") };
        cand.push((i, kind, sc));
        i += step as i64;
    }
    let peak = cand.iter().fold(0.0f64, |m, c| m.max(c.2));
    if peak <= 0.0 {
        return Vec::new();
    }
    let thr = rel * peak;

    let mut kept: Vec<(i64, &'static str, f64, f64)> = Vec::new();
    for &(s0, kind, sc) in cand.iter().filter(|c| c.2 >= thr) {
        let e_dur = energy_at(mf, s0, s0 + 32 * sps as i64);
        let e_pre = energy_at(mf, s0 - 16 * sps as i64, s0);
        if e_dur < 1.15 * e_pre {
            continue;
        }
        kept.push((s0, kind, sc, e_dur));
    }
    if kept.is_empty() {
        return Vec::new();
    }
    let e_max = kept.iter().fold(0.0f64, |m, k| m.max(k.3));
    kept.retain(|k| k.3 >= 0.25 * e_max);

    kept.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));
    let sep = (min_sep_sym * sps) as i64;
    let mut out: Vec<(i64, &'static str, f64)> = Vec::new();
    for (s0, kind, sc, _e) in kept {
        if out.iter().any(|o| (o.0 - s0).abs() < sep) {
            continue;
        }
        out.push((s0, kind, sc));
    }
    out.sort_by_key(|o| o.0);

    let delay = match_delay(CC_RRC_ALPHA, sps, 8);
    out.iter()
        .map(|&(s0, kind, sc)| {
            let pre = if kind == "HDR" { PRE_A } else { PRE_B };
            let mut best = s0;
            let mut best_e = -1.0f64;
            for st in (s0 - step as i64)..=(s0 + step as i64) {
                if st < 0 || st + (pre.len() * sps) as i64 > mf.len() as i64 {
                    continue;
                }
                let e: f64 = (0..pre.len())
                    .map(|k| mf[(st as usize) + k * sps].norm2())
                    .sum();
                if e > best_e {
                    best_e = e;
                    best = st;
                }
            }
            Burst { start: best - delay, kind, score: sc }
        })
        .collect()
}

/// 找出全部 CC 突发（内部做一次匹配滤波；批量处理时用 `find_bursts_mf`）。
pub fn find_bursts(x: &[f32], fs: usize, center: f64, baud: f64,
                   rel: f64, min_sep_sym: usize) -> Vec<Burst> {
    let mf = matched_filter(x, fs, center, baud);
    find_bursts_mf(&mf, fs, baud, rel, min_sep_sym)
}

/// 在给定样点处解调一段突发（内部做匹配滤波；批量时用 `demodulate_mf`）。
pub fn demodulate_at(x: &[f32], fs: usize, center: f64, baud: f64,
                     start: i64, n_bits: usize, pre: &[C64; 32]) -> Vec<f32> {
    let mf = matched_filter(x, fs, center, baud);
    demodulate_mf(&mf, fs, baud, start, n_bits, pre)
}

/// 在已算好的匹配滤波输出上解调，返回软值（正 = 比特 0）。
pub fn demodulate_mf(mf: &[C64], fs: usize, baud: f64,
                     start: i64, n_bits: usize, pre: &[C64; 32]) -> Vec<f32> {
    let sps = (fs as f64 / baud).round() as usize;
    let delay = match_delay(CC_RRC_ALPHA, sps, 8);
    let pos = start + delay;
    let total = pre.len() + n_bits / 2;

    let mut y = Vec::with_capacity(total);
    for k in 0..total {
        let idx = pos + (k * sps) as i64;
        y.push(if idx >= 0 && (idx as usize) < mf.len() {
            mf[idx as usize]
        } else {
            C64::ZERO
        });
    }
    let mut soft = Vec::with_capacity((total - 1) * 2);
    for k in 1..total {
        let d = y[k] * y[k - 1].conj();
        soft.push(d.re as f32);
        soft.push(d.im as f32);
    }
    let skip = (pre.len() - 1) * 2;
    soft.drain(0..skip.min(soft.len()));
    soft.truncate(n_bits);
    soft
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::params::{CC_BAUD, CC_CENTER, FS};

    #[test]
    fn rrc_is_unit_energy() {
        let h = rrc_taps(CC_RRC_ALPHA, 64, 8);
        let e: f64 = h.iter().map(|v| v * v).sum();
        assert!((e - 1.0).abs() < 1e-12);
    }

    #[test]
    fn burst_len() {
        let bits: Vec<u8> = (0..342).map(|i| (i % 2) as u8).collect();
        let sig = modulate(&bits, FS, CC_CENTER, CC_BAUD, 0.35, preamble_hdr());
        let sps = (FS as f64 / CC_BAUD) as usize;
        let taps = rrc_taps(CC_RRC_ALPHA, sps, 8).len();
        assert_eq!(sig.len(), (32 + 171) * sps + taps - 1);
    }
}
