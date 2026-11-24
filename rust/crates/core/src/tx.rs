//! 发射：字形 + 导频 + 控制信道 → 一条完整音频（与 Python `tx.py` 同构）。
//!
//! 时间线以 CC 符号为单位排布，视觉行与 CC 符号严格成整数比（T_row = 5k·T_sym）。

use crate::fec;
use crate::frame::{call_crc, mode_bits, pack_bcon, pack_eot, Hdr};
use crate::modem;
use crate::params::{self, Mode};
use crate::vision::{self, RowPlan};

pub struct Segment {
    pub start: usize,
    pub n_cells: usize,
    pub rows: usize,
}

pub struct TxMeta {
    pub hdr: Hdr,
    pub bursts: Vec<(usize, &'static str)>,
    pub segments: Vec<Segment>,
    pub total: usize,
}

pub struct TxOpts {
    pub fs: usize,
    pub lead: f64,
    pub tail: f64,
    pub vis_amp: f64,
    pub pilot_amp: f64,
    pub cc_ratio: f64,
    pub cells_per_segment: usize,
    pub seed: u32,
    pub call: String,
    pub grid: String,
    pub font_id: u8,
    /// 同步帧周期（秒）；0 = 不叠加。整十秒对齐由调用方决定起点。
    pub sync_period: f64,
}

impl Default for TxOpts {
    fn default() -> Self {
        TxOpts {
            fs: params::FS,
            lead: 1.5,
            tail: 0.5,
            vis_amp: 0.8,
            pilot_amp: 0.25,
            cc_ratio: 0.6,
            cells_per_segment: params::CELLS_PER_SEGMENT,
            seed: 1,
            call: "BG1ABC".into(),
            grid: "OM89AB".into(),
            font_id: 1,
            sync_period: 0.0,
        }
    }
}

fn rms(x: &[f32]) -> f64 {
    if x.is_empty() {
        return 0.0;
    }
    (x.iter().map(|v| (*v as f64).powi(2)).sum::<f64>() / x.len() as f64).sqrt()
}

fn scale_to(x: &mut [f32], target: f64) {
    let r = rms(x);
    if r <= 0.0 {
        return;
    }
    let g = target / r;
    for v in x.iter_mut() {
        *v = (*v as f64 * g) as f32;
    }
}

fn cc_burst(bits: &[u8], kind: &str, fs: usize) -> Vec<f32> {
    let coded = fec::interleave(&fec::conv_encode(bits));
    let pre = if kind == "HDR" {
        modem::preamble_hdr()
    } else {
        modem::preamble_bcon()
    };
    modem::modulate(&coded, fs, params::CC_CENTER, params::CC_BAUD, 1.0, pre)
}

fn pilots(total: usize, fs: usize, amp: f64) -> Vec<f32> {
    (0..total)
        .map(|n| {
            let t = n as f64 / fs as f64;
            (amp * (2.0 * std::f64::consts::PI * params::PILOT1 * t).cos()
                + amp * (2.0 * std::f64::consts::PI * params::PILOT2 * t).cos()) as f32
        })
        .collect()
}

pub fn build(glyphs: &[Vec<u8>], mode: Mode, opts: &TxOpts) -> (Vec<f32>, TxMeta) {
    let fs = opts.fs;
    let t_sym = fs as f64 / params::CC_BAUD;
    let n_row = (mode.t_row() * fs as f64).round() as usize;
    let sym = |n: i64| (n as f64 * t_sym).round() as usize;

    let plan_field = if (mode.f_lo - 800.0).abs() < 1e-9 && (mode.df - 25.0).abs() < 1e-9 {
        0
    } else {
        1
    };
    let hdr = Hdr {
        call: opts.call.clone(),
        grid: opts.grid.clone(),
        length: glyphs.len() as u16,
        msg_type: 0,
        sf_id: 0,
        utc: 0,
        power: 1,
        rmap: 0,
        plan: plan_field,
        raster: mode.n as u8,
        k: mode.k,
        repeat: mode.repeat as u8,
        font_id: opts.font_id,
    };

    let mut hdr_sig = cc_burst(&hdr.pack(), "HDR", fs);
    scale_to(&mut hdr_sig, opts.cc_ratio * opts.vis_amp);
    let mut eot_sig = cc_burst(&pack_eot(glyphs.len()), "BCON", fs);
    scale_to(&mut eot_sig, opts.cc_ratio * opts.vis_amp);

    let lead_n = (opts.lead * fs as f64).round() as usize;
    let gap = sym(params::SEGMENT_GAP_SYM);
    let mb = mode_bits(mode.n as u8, mode.k, mode.repeat as u8, opts.font_id);

    // ---- 先排时间线 ----
    let mut cc: Vec<(usize, &'static str, Vec<f32>)> = Vec::new();
    let mut segs: Vec<(usize, Segment, RowPlan)> = Vec::new();

    let mut t = lead_n;
    cc.push((t, "HDR", hdr_sig));
    t += sym(params::HDR_SYMBOLS) + gap;

    let groups: Vec<&[Vec<u8>]> = if glyphs.is_empty() {
        Vec::new()
    } else {
        glyphs.chunks(opts.cells_per_segment.max(1)).collect()
    };
    for (gi, group) in groups.iter().enumerate() {
        let plan = vision::plan_cells(group, mode.n, mode.repeat);
        let rows = plan.rows.len();
        segs.push((t, Segment { start: t, n_cells: group.len(), rows }, plan));
        t += rows * n_row;
        if gi + 1 < groups.len() {
            let bcon = pack_bcon(gi as u16 + 1,
                                 (((gi + 1) * opts.cells_per_segment) & 0xFF) as u8,
                                 0, &mb, call_crc(&opts.call));
            let mut sig = cc_burst(&bcon, "BCON", fs);
            scale_to(&mut sig, opts.cc_ratio * opts.vis_amp);
            cc.push((t, "BCON", sig));
            t += sym(params::BCON_SYMBOLS) + gap;
        }
    }
    let eot_start = t;
    cc.push((eot_start, "BCON", eot_sig));
    let total = t + sym(params::EOT_SYMBOLS) + (opts.tail * fs as f64).round() as usize + 1024;

    // ---- 再混音 ----
    let mut x = vec![0.0f32; total];
    for (start, _k, sig) in &cc {
        for (i, v) in sig.iter().enumerate() {
            if start + i < total {
                x[start + i] += *v;
            }
        }
    }
    for (start, seg, plan) in &segs {
        let sig = vision::synth(plan, &mode, seg.rows * n_row, fs, opts.vis_amp,
                                opts.seed.wrapping_add(*start as u32 / 997));
        for (i, v) in sig.iter().enumerate() {
            if start + i < total {
                x[start + i] += *v;
            }
        }
    }
    for (i, v) in pilots(total, fs, opts.pilot_amp).iter().enumerate() {
        x[i] += *v;
    }
    // ---- 同步帧（v1.1）：按固定周期叠加，与视觉层频分共存 ----
    if opts.sync_period > 0.0 {
        use crate::frame::SyncFrame;
        let period = (opts.sync_period * fs as f64).round() as usize;
        let mut t = 0usize;
        while t < total {
            // 字符计数器：到本帧起点为止已经播出的字符数
            // 字符计数器：按"已播出的整字符数"累加（不是等整段播完）
            let rpc_rows = (mode.n + 1) * mode.repeat;
            let cell_row = rpc_rows * n_row;
            let mut cell = 0u16;
            for s in &segs {
                let seg = &s.1;
                if t <= seg.start {
                    continue;
                }
                let sent = (t - seg.start) / cell_row.max(1);
                cell = cell.saturating_add(sent.min(seg.n_cells) as u16);
            }
            let sf = SyncFrame {
                slot: (((t as f64 / fs as f64) / 10.0) as u32 % 60) as u8,
                cell,
                call_crc: call_crc(&opts.call),
                grid: opts.grid.clone(),
                raster: mode.n as u8,
                k: mode.k as u8,
                font_id: opts.font_id,
            };
            let coded = fec::interleave(&fec::conv_encode(&sf.pack()));
            let mut sig = modem::modulate(&coded, fs, params::CC_CENTER, params::CC_BAUD,
                                          params::CC_RRC_ALPHA, modem::preamble_sync());
            scale_to(&mut sig, opts.cc_ratio * opts.vis_amp);
            for (i, v) in sig.iter().enumerate() {
                if t + i < total {
                    x[t + i] += *v;
                }
            }
            t += period;
        }
    }

    let peak = x.iter().fold(0.0f32, |m, v| m.max(v.abs()));
    if peak > 0.99 {
        let g = 0.99 / peak;
        for v in x.iter_mut() {
            *v *= g;
        }
    }

    let meta = TxMeta {
        hdr,
        bursts: cc.iter().map(|(s, k, _)| (*s, *k)).collect(),
        segments: segs.into_iter().map(|(_, s, _)| s).collect(),
        total,
    };
    (x, meta)
}
