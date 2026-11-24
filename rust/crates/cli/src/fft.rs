//! 极小的 radix-2 FFT + 频谱行编码，只为"原始瀑布图"服务。
//!
//! 为什么自己写：整个工程除 cpal 外不引第三方依赖，而这里只需要
//! 定长复数 FFT 和一次取模，40 行代码就够。

use std::f64::consts::PI;

/// 原地基-2 FFT（长度必须是 2 的幂）
pub fn fft_inplace(re: &mut [f64], im: &mut [f64]) {
    let n = re.len();
    debug_assert!(n.is_power_of_two() && im.len() == n);

    // 位反转置换
    let mut j = 0usize;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j |= bit;
        if i < j {
            re.swap(i, j);
            im.swap(i, j);
        }
    }

    let mut len = 2usize;
    while len <= n {
        let ang = -2.0 * PI / len as f64;
        let (wr, wi) = (ang.cos(), ang.sin());
        let mut i = 0usize;
        while i < n {
            let (mut cr, mut ci) = (1.0f64, 0.0f64);
            for k in 0..len / 2 {
                let (ur, ui) = (re[i + k], im[i + k]);
                let (vr, vi) = (re[i + k + len / 2], im[i + k + len / 2]);
                let tr = vr * cr - vi * ci;
                let ti = vr * ci + vi * cr;
                re[i + k] = ur + tr;
                im[i + k] = ui + ti;
                re[i + k + len / 2] = ur - tr;
                im[i + k + len / 2] = ui - ti;
                let ncr = cr * wr - ci * wi;
                ci = cr * wi + ci * wr;
                cr = ncr;
            }
            i += len;
        }
        len <<= 1;
    }
}

/// 汉宁窗（减少频谱泄漏，看"原始电平"时更干净）
pub fn hann(n: usize) -> Vec<f64> {
    (0..n)
        .map(|i| 0.5 - 0.5 * (2.0 * PI * i as f64 / (n as f64 - 1.0)).cos())
        .collect()
}

/// 一帧频谱：返回 (每 bin 的 dBFS, 峰值 dBFS)
///
/// `spec` 长度 = n/2（只取前半，直流和 Nyquist 之间的正频率），
/// 频点 `i` 对应 `i * fs / n` Hz。
pub fn frame_db(samples: &[f32], window: &[f64]) -> (Vec<f64>, f64) {
    let n = samples.len();
    let mut re: Vec<f64> = Vec::with_capacity(n);
    let mut im = vec![0.0f64; n];
    for (i, s) in samples.iter().enumerate() {
        re.push(*s as f64 * window[i]);
    }
    fft_inplace(&mut re, &mut im);

    // 归一化：窗增益补偿后取幅度谱，再转 dBFS
    let gain: f64 = window.iter().sum::<f64>() / n as f64;
    let scale = 2.0 / (n as f64 * gain.max(1e-9));
    let mut out = Vec::with_capacity(n / 2);
    let mut peak = -120.0f64;
    for i in 0..n / 2 {
        let mag = (re[i] * re[i] + im[i] * im[i]).sqrt() * scale;
        let db = if mag <= 1e-9 { -120.0 } else { 20.0 * mag.log10() };
        let db = db.max(-120.0);
        if db > peak {
            peak = db;
        }
        out.push(db);
    }
    (out, peak)
}

/// dBFS → 0..255（贴近屏幕观感的对数映射；-100 dB 以下算黑）
pub fn db_to_u8(db: f64) -> u8 {
    let lo = -100.0;
    let hi = 0.0;
    let t = ((db - lo) / (hi - lo)).clamp(0.0, 1.0);
    (t * 255.0) as u8
}

/// 短窗 + 零填充：窗长决定时间分辨率（64 ms），FFT 长度决定频率栅格（7.8 Hz）。
/// 这样既能跟上 80 ms 的字形行，又不会把 25 Hz 间隔的音糊在一起。
pub fn frame_db_zp(samples: &[f32], window: &[f64], fft_len: usize) -> (Vec<f64>, f64) {
    let n = fft_len;
    let mut re = vec![0.0f64; n];
    let mut im = vec![0.0f64; n];
    for (i, s) in samples.iter().enumerate().take(window.len()) {
        re[i] = *s as f64 * window[i];
    }
    fft_inplace(&mut re, &mut im);
    // 幅度归一：单个幅度 A 的正弦，加窗后峰值 ≈ A·Σw/2
    let wsum: f64 = window.iter().sum::<f64>();
    let scale = 2.0 / wsum.max(1e-9);
    let mut out = Vec::with_capacity(n / 2);
    let mut peak = -120.0f64;
    for i in 0..n / 2 {
        let mag = (re[i] * re[i] + im[i] * im[i]).sqrt() * scale;
        let db = if mag <= 1e-9 { -120.0 } else { 20.0 * mag.log10() };
        let db = db.max(-120.0);
        if db > peak { peak = db; }
        out.push(db);
    }
    (out, peak)
}

/// 从完整频谱里按 [f_lo, f_hi] 抽 n 个 bin（取区间内最大值，避免漏掉窄峰）
pub fn slice_band(spec: &[f64], fs: f64, n: usize, f_lo: f64, f_hi: f64) -> Vec<u8> {
    let bin_hz = fs / (2.0 * spec.len() as f64);
    let mut out = vec![0u8; n];
    for (j, slot) in out.iter_mut().enumerate() {
        let f0 = f_lo + (f_hi - f_lo) * j as f64 / n as f64;
        let f1 = f_lo + (f_hi - f_lo) * (j + 1) as f64 / n as f64;
        let i0 = ((f0 / bin_hz).floor() as isize).max(0) as usize;
        let i1 = ((f1 / bin_hz).ceil() as usize).min(spec.len());
        let mut best = -120.0f64;
        for i in i0..i1.max(i0 + 1).min(spec.len()) {
            if spec[i] > best {
                best = spec[i];
            }
        }
        *slot = db_to_u8(best);
    }
    out
}


/// 与 `slice_band` 相同，但不量化成 u8（诊断/统计用，保留 dB 精度）。
pub fn slice_band_f32(spec: &[f64], fs: f64, n: usize, f_lo: f64, f_hi: f64) -> Vec<f32> {
    let bin_hz = fs / (2.0 * spec.len() as f64);
    let mut out = vec![0f32; n];
    for (j, slot) in out.iter_mut().enumerate() {
        let f0 = f_lo + (f_hi - f_lo) * j as f64 / n as f64;
        let f1 = f_lo + (f_hi - f_lo) * (j + 1) as f64 / n as f64;
        let i0 = ((f0 / bin_hz).floor() as isize).max(0) as usize;
        let i1 = ((f1 / bin_hz).ceil() as usize).min(spec.len());
        let mut best = -120.0f64;
        for i in i0..i1.max(i0 + 1).min(spec.len()) {
            if spec[i] > best {
                best = spec[i];
            }
        }
        *slot = best as f32;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peak_hz(fs: f64, win: usize, fft_len: usize, tone: f64) -> f64 {
        let mut x = vec![0f32; win];
        for (i, v) in x.iter_mut().enumerate() {
            let t = i as f64 / fs;
            *v = (2.0 * PI * tone * t).sin() as f32;
        }
        let w = hann(win);
        let (spec, _) = frame_db_zp(&x, &w, fft_len);
        let bins = slice_band(&spec, fs, 512, 0.0, 4000.0);
        let (i, _) = bins
            .iter()
            .enumerate()
            .max_by_key(|(_, v)| **v)
            .unwrap();
        (i as f64 + 0.5) * 4000.0 / 512.0
    }

    /// 8 kHz 采集（WAV 回放）与 48 kHz 采集（实时声卡）都必须把 1250 Hz 画在 1250 Hz 上
    #[test]
    fn tone_lands_on_its_frequency() {
        for &(fs, win, fft_len) in &[(8000.0, 512usize, 1024usize), (48000.0, 3072, 8192)] {
            let f = peak_hz(fs, win, fft_len, 1250.0);
            assert!((f - 1250.0).abs() < 25.0, "fs={fs}: peak at {f} Hz");
        }
    }
}
