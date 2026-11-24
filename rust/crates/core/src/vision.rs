//! 视觉层解调：按 CC 锚定的栅格做 box 积分，产出每个音的相关功率。

use std::f64::consts::PI;

pub struct Demod {
    /// (行, 音) 的相关功率
    pub p: Vec<Vec<f64>>,
    /// 每行的边距音功率（只积分行首 1/4）
    pub pm: Vec<f64>,
}

/// 行起点按小数行周期递推（含时钟偏差时 T_row 不是整数样点）。
pub fn row_starts(t0: f64, n_rows: usize, t_row: f64) -> Vec<i64> {
    (0..n_rows).map(|r| (t0 + r as f64 * t_row).round() as i64).collect()
}

pub fn demod(x: &[f32], t0: f64, n_rows: usize, t_row: f64,
             tones: &[f64], f_margin: f64, fs: usize) -> Demod {
    let n = t_row.round().max(1.0) as usize;
    let starts = row_starts(t0, n_rows, t_row);
    let mut p = Vec::with_capacity(n_rows);
    let mut pm = Vec::with_capacity(n_rows);
    let q = (n / 4).max(1);

    for s in &starts {
        let base = (*s).max(0) as usize;
        let mut row = Vec::with_capacity(tones.len());
        for &f in tones {
            let w = 2.0 * PI * f / fs as f64;
            let (mut wr, mut wi) = (1.0f64, 0.0f64);
            let (c, sn) = (w.cos(), w.sin());
            let (mut re, mut im) = (0.0f64, 0.0f64);
            for k in 0..n {
                let idx = base + k;
                let v = if idx < x.len() { x[idx] as f64 } else { 0.0 };
                re += v * wr;
                im -= v * wi;
                let nr = wr * c - wi * sn;
                let ni = wr * sn + wi * c;
                wr = nr;
                wi = ni;
            }
            let re = re / n as f64;
            let im = im / n as f64;
            row.push(re * re + im * im);
        }
        p.push(row);

        // 边距音：只取行首 1/4
        let w = 2.0 * PI * f_margin / fs as f64;
        let (mut wr, mut wi) = (1.0f64, 0.0f64);
        let (c, sn) = (w.cos(), w.sin());
        let (mut re, mut im) = (0.0f64, 0.0f64);
        for k in 0..q {
            let idx = base + k;
            let v = if idx < x.len() { x[idx] as f64 } else { 0.0 };
            re += v * wr;
            im -= v * wi;
            let nr = wr * c - wi * sn;
            let ni = wr * sn + wi * c;
            wr = nr;
            wi = ni;
        }
        let re = re / q as f64;
        let im = im / q as f64;
        pm.push(re * re + im * im);
    }
    Demod { p, pm }
}

/// 锚点已在协议给定位，只允许 ±2 行的整行微调。
pub fn phase_offset(pm: &[f64], rows_per_char: usize) -> i64 {
    if pm.is_empty() || rows_per_char == 0 {
        return 0;
    }
    let mut best_off = 0i64;
    let mut best = -1.0f64;
    for off in -2i64..=2 {
        let mut sum = 0.0;
        let mut cnt = 0usize;
        let mut i = off;
        while i >= 0 && (i as usize) < pm.len() {
            sum += pm[i as usize];
            cnt += 1;
            i += rows_per_char as i64;
        }
        if cnt == 0 {
            continue;
        }
        let score = sum / cnt as f64;
        if score > best {
            best = score;
            best_off = off;
        }
    }
    best_off
}

/// 在 [lo,hi) 内按 rows_per_char 等距切字符。
pub fn cells_in_range(pm: &[f64], lo: usize, hi: usize, rows_per_char: usize) -> Vec<usize> {
    if hi <= lo || hi - lo < rows_per_char {
        return Vec::new();
    }
    let span = &pm[lo..hi];
    let off = phase_offset(span, rows_per_char);
    if off < 0 {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut s = lo + off as usize;
    while s + rows_per_char <= hi {
        out.push(s);
        s += rows_per_char;
    }
    out
}

/// 取某个字符块（跳过边距行）。repeat>1 时每逻辑行占 repeat 行。
pub fn block(p: &[Vec<f64>], start: usize, n: usize, repeat: usize) -> Vec<Vec<f64>> {
    let from = start + repeat;
    let to = from + n * repeat;
    p[from.min(p.len())..to.min(p.len())].to_vec()
}

// --------------------------------------------------------------------------
// 发射：行调度 + 合成
// --------------------------------------------------------------------------

pub struct RowPlan {
    pub rows: Vec<Vec<u8>>,
    pub margin: Vec<bool>,
}

/// 把一组字形排成"1 行边距 + n 行字形"，每行重复 repeat 次。
pub fn plan_cells(glyphs: &[Vec<u8>], n: usize, repeat: usize) -> RowPlan {
    let mut rows = Vec::new();
    let mut margin = Vec::new();
    for g in glyphs {
        for _ in 0..repeat {
            rows.push(vec![0u8; n]);
            margin.push(true);
        }
        for r in 0..n {
            let row: Vec<u8> = (0..n).map(|c| *g.get(r * n + c).unwrap_or(&0)).collect();
            for _ in 0..repeat {
                rows.push(row.clone());
                margin.push(false);
            }
        }
    }
    RowPlan { rows, margin }
}

struct XorShift(u32);

impl XorShift {
    fn next(&mut self) -> f64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 17;
        self.0 ^= self.0 << 5;
        (self.0 as f64) / (u32::MAX as f64)
    }
}

/// 合成一段视觉层：每路独立包络、连续相位、随机初相、升余弦过渡。
pub fn synth(plan: &RowPlan, mode: &crate::params::Mode, total: usize,
             fs: usize, amp: f64, seed: u32) -> Vec<f32> {
    let n_row = (mode.t_row() * fs as f64).round() as usize;
    if n_row == 0 || plan.rows.is_empty() {
        return vec![0.0; total];
    }
    let l = ((0.03 * n_row as f64).round() as usize).max(4) | 1;
    let mut kern = vec![0.0f64; l];
    for (i, k) in kern.iter_mut().enumerate() {
        *k = 0.5 * (1.0 - (2.0 * PI * i as f64 / (l - 1) as f64).cos());
    }
    let ksum: f64 = kern.iter().sum();
    for k in kern.iter_mut() {
        *k /= ksum;
    }
    let half = l / 2;

    let mut rng = XorShift(seed | 1);
    let tones = mode.tones();
    let mut x = vec![0.0f64; total];

    for (ti, &f) in tones.iter().enumerate() {
        // 该路音的 0/1 包络
        let mut env = vec![0.0f64; total];
        let mut any = false;
        for (r, row) in plan.rows.iter().enumerate() {
            if row.get(ti).copied().unwrap_or(0) == 0 {
                continue;
            }
            any = true;
            let s = r * n_row;
            let e = (s + n_row).min(total);
            if s < total {
                for v in env[s..e].iter_mut() {
                    *v = 1.0;
                }
            }
        }
        if !any {
            continue;
        }
        // 升余弦平滑（与 Python 的 convolve(full) 后取 [half, half+total] 等价）
        let mut sm = vec![0.0f64; total];
        for (i, &v) in env.iter().enumerate() {
            if v == 0.0 {
                continue;
            }
            let base = i as i64 - half as i64;
            for (j, &k) in kern.iter().enumerate() {
                let idx = base + j as i64;
                if idx >= 0 && (idx as usize) < total {
                    sm[idx as usize] += v * k;
                }
            }
        }
        let ph0 = 2.0 * PI * rng.next();
        let w = 2.0 * PI * f / fs as f64;
        for (n, &e) in sm.iter().enumerate() {
            if e != 0.0 {
                x[n] += e * (w * n as f64 + ph0).cos();
            }
        }
    }

    // 边距点：每字符第一行前 40%，幅度 1.6 倍
    let quarter = ((n_row as f64 * 0.4) as usize).max(1);
    let wm = 2.0 * PI * mode.f_margin() / fs as f64;
    let phm = 2.0 * PI * rng.next();
    for (r, is_margin) in plan.margin.iter().enumerate() {
        if !is_margin {
            continue;
        }
        let s = r * n_row;
        for k in 0..quarter {
            let n = s + k;
            if n < total {
                x[n] += 1.6 * (wm * n as f64 + phm).cos();
            }
        }
    }

    let rms = (x.iter().map(|v| v * v).sum::<f64>() / total.max(1) as f64).sqrt();
    let g = if rms > 0.0 { amp / rms } else { 0.0 };
    x.iter().map(|v| (v * g) as f32).collect()
}
