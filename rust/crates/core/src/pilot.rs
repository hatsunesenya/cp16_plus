//! 双导频频率标定：由 P1/P2 解出音频域的仿射参数 `f_meas = a·f_nom + b`。
//!
//! 与 Python `pilot.py` 同一思路：长窗扫描粗测峰值（避开低通零点），再用相位斜率精修。
//! 实现上全部用递推相量与前缀和，**不做三角函数内层循环**，8 kHz × 4 s 单音约 40 ms。

use std::f64::consts::PI;

#[derive(Clone, Copy, Debug)]
pub struct PilotEst {
    pub a: f64,
    pub b: f64,
    pub f1: f64,
    pub f2: f64,
    pub conf: f64,
}

impl PilotEst {
    pub fn apply(&self, f_nom: f64) -> f64 {
        self.a * f_nom + self.b
    }
}

/// 在 f_nom ± search 内扫描找峰（Hann 窗 + 抛物线插值）。
fn coarse_peak(x: &[f32], f_nom: f64, fs: usize, search: f64) -> f64 {
    let n = x.len().min(4 * fs);
    if n < 256 {
        return f_nom;
    }
    // 预加窗
    let seg: Vec<f64> = (0..n)
        .map(|i| {
            let w = 0.5 - 0.5 * (2.0 * PI * i as f64 / (n - 1) as f64).cos();
            x[i] as f64 * w
        })
        .collect();

    let step = 0.5;
    let n_steps = (2.0 * search / step) as usize + 1;
    let mut mags = vec![0.0f64; n_steps];
    let mut best_i = 0usize;
    let mut best = -1.0;
    for k in 0..n_steps {
        let f = f_nom - search + k as f64 * step;
        let th = 2.0 * PI * f / fs as f64;
        let (c, s) = (th.cos(), th.sin());
        let (mut wr, mut wi) = (1.0f64, 0.0f64);
        let (mut re, mut im) = (0.0f64, 0.0f64);
        for (i, &v) in seg.iter().enumerate() {
            re += v * wr;
            im -= v * wi;
            let nr = wr * c - wi * s;
            let ni = wr * s + wi * c;
            wr = nr;
            wi = ni;
            if i & 1023 == 1023 {
                // 相量递推会缓慢丢模，定期归一
                let g = (wr * wr + wi * wi).sqrt();
                wr /= g;
                wi /= g;
            }
        }
        let m = re * re + im * im;
        mags[k] = m;
        if m > best {
            best = m;
            best_i = k;
        }
    }
    if best_i == 0 || best_i + 1 >= n_steps {
        return f_nom - search + best_i as f64 * step;
    }
    let (y0, y1, y2) = (
        mags[best_i - 1].max(1e-30).ln(),
        mags[best_i].max(1e-30).ln(),
        mags[best_i + 1].max(1e-30).ln(),
    );
    let den = y0 - 2.0 * y1 + y2;
    let d = if den.abs() < 1e-12 { 0.0 } else { 0.5 * (y0 - y2) / den };
    f_nom - search + (best_i as f64 + d) * step
}

/// 粗频上的复解调 + 滑动平均，用相位斜率精修，返回 (f_meas, conf)。
fn refine(x: &[f32], f0: f64, fs: usize, win: f64) -> (f64, f64) {
    let l = ((win * fs as f64) as usize).max(16);
    if x.len() < l + 8 {
        return (f0, 0.0);
    }
    let w = 2.0 * PI * f0 / fs as f64;
    // 前缀和做 boxcar，避免 O(n·l)
    let mut pre_re = Vec::with_capacity(x.len() + 1);
    let mut pre_im = Vec::with_capacity(x.len() + 1);
    pre_re.push(0.0);
    pre_im.push(0.0);
    let (mut ar, mut ai) = (0.0f64, 0.0f64);
    for (n, &v) in x.iter().enumerate() {
        let ph = w * n as f64;
        ar += v as f64 * ph.cos();
        ai -= v as f64 * ph.sin();
        pre_re.push(ar);
        pre_im.push(ai);
    }
    let m = x.len() - l + 1;
    if m < 8 {
        return (f0, 0.0);
    }
    let inv = 1.0 / l as f64;
    let z: Vec<(f64, f64)> = (0..m)
        .map(|k| ((pre_re[k + l] - pre_re[k]) * inv, (pre_im[k + l] - pre_im[k]) * inv))
        .collect();

    let mag: Vec<f64> = z.iter().map(|(r, i)| (r * r + i * i).sqrt()).collect();
    let mut sorted = mag.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let p95 = sorted[((sorted.len() as f64 * 0.95) as usize).min(sorted.len() - 1)];
    let thr = 0.25 * p95;

    // 相位解缠（累加差分，避免 atan2 跳变）
    let mut unwrapped = Vec::with_capacity(z.len());
    let mut prev = z[0].1.atan2(z[0].0);
    let mut acc = prev;
    unwrapped.push(acc);
    for (re, im) in z.iter().skip(1) {
        let raw = im.atan2(*re);
        let mut d = raw - prev;
        while d > PI {
            d -= 2.0 * PI;
        }
        while d < -PI {
            d += 2.0 * PI;
        }
        acc += d;
        unwrapped.push(acc);
        prev = raw;
    }

    let (mut sx, mut sy, mut sxx, mut sxy, mut cnt) = (0.0f64, 0.0, 0.0, 0.0, 0.0);
    for k in 0..unwrapped.len() {
        if mag[k] < thr {
            continue;
        }
        let t = (k as f64 + l as f64 / 2.0) / fs as f64;
        sx += t;
        sy += unwrapped[k];
        sxx += t * t;
        sxy += t * unwrapped[k];
        cnt += 1.0;
    }
    if cnt < 8.0 {
        return (f0, 0.0);
    }
    let den = cnt * sxx - sx * sx;
    let slope = if den.abs() < 1e-18 { 0.0 } else { (cnt * sxy - sx * sy) / den };
    let icept = (sy - slope * sx) / cnt;

    let (mut resid, mut n) = (0.0f64, 0.0f64);
    for k in 0..unwrapped.len() {
        if mag[k] < thr {
            continue;
        }
        let t = (k as f64 + l as f64 / 2.0) / fs as f64;
        let r = unwrapped[k] - (slope * t + icept);
        resid += r * r;
        n += 1.0;
    }
    let sd = (resid / n.max(1.0)).sqrt();
    let conf = (1.0 - sd / (PI / 2.0)).clamp(0.0, 1.0);
    (f0 + slope / (2.0 * PI), conf)
}

pub fn estimate(x: &[f32], fs: usize, p1: f64, p2: f64, search: f64) -> PilotEst {
    // 固定只看前 4 s：流式（缓冲还短）与离线（整条文件）必须得到同一个结果
    let n = x.len().min(4 * fs);
    let x = &x[..n];
    let f1 = coarse_peak(x, p1, fs, search);
    let f2 = coarse_peak(x, p2, fs, search);
    let (f1, c1) = refine(x, f1, fs, 0.5);
    let (f2, c2) = refine(x, f2, fs, 0.5);
    let a = (f2 - f1) / (p2 - p1);
    let b = f1 - a * p1;
    PilotEst { a, b, f1, f2, conf: c1.min(c2) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovers_affine() {
        let fs = 8000usize;
        let n = 3 * fs;
        let (a_true, b_true) = (1.00007f64, 12.0f64);
        let x: Vec<f32> = (0..n)
            .map(|i| {
                let t = i as f64 / fs as f64;
                (0.25 * (2.0 * PI * (a_true * 450.0 + b_true) * t).cos()
                    + 0.25 * (2.0 * PI * (a_true * 1250.0 + b_true) * t).cos()) as f32
            })
            .collect();
        let e = estimate(&x, fs, 450.0, 1250.0, 60.0);
        assert!((e.a - a_true).abs() < 1e-6, "a={}", e.a);
        assert!((e.b - b_true).abs() < 0.1, "b={}", e.b);
    }
}
