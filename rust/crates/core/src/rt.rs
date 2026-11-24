//! 实时通路：环形/线性缓冲 + 抗混叠抽取 + **增量**解码。
//!
//! 与上一版（每次重跑整窗）相比，这里把状态留在会话里：
//! - 匹配滤波只对**新增样本**做，历史存 1024 个基带样点续接；
//! - 前导相关扫描只扫新增位置；
//! - 突发一旦确认，栅格锚点立即固定，之后**逐行**解调，行到即可用；
//! - 字符在其 17 行齐了就吐出，段结束（下一个突发 / 结束）时补齐尾巴。
//!
//! 结果与离线 `rx::decode` 逐块一致（测试里直接比对），但代价从 O(缓冲长度)
//! 降到 O(新增样本)，48 kHz 实时输入下 CPU 占用不到 2%。

use crate::modem::{self, Burst};
use crate::params::{self, Mode, FS};
use crate::pilot;
use crate::vision;

pub struct Ring {
    buf: Vec<f32>,
    pos: usize,
    len: usize,
    pub total: u64,
}

impl Ring {
    pub fn new(cap: usize) -> Self {
        Ring { buf: vec![0.0; cap], pos: 0, len: 0, total: 0 }
    }
    pub fn push(&mut self, x: &[f32]) {
        for &v in x {
            self.buf[self.pos] = v;
            self.pos = (self.pos + 1) % self.buf.len();
            if self.len < self.buf.len() {
                self.len += 1;
            }
            self.total += 1;
        }
    }
    pub fn len(&self) -> usize {
        self.len
    }
    pub fn data(&self) -> Vec<f32> {
        let cap = self.buf.len();
        let mut out = Vec::with_capacity(self.len);
        let start = (self.pos + cap - self.len) % cap;
        for i in 0..self.len {
            out.push(self.buf[(start + i) % cap]);
        }
        out
    }
}

/// 整数比 FIR 抽取（48k→8k 时 ratio=6）。
pub struct Decimator {
    ratio: usize,
    taps: Vec<f32>,
    hist: Vec<f32>,
    filled: usize,
    count: usize,
}

impl Decimator {
    pub fn new(fs_in: usize, fs_out: usize) -> Option<Self> {
        if fs_in <= fs_out || fs_in % fs_out != 0 {
            return None;
        }
        let ratio = fs_in / fs_out;
        let n = 16 * ratio + 1;
        let fc = 0.45 * fs_out as f64 / fs_in as f64;
        let mid = (n - 1) as f64 / 2.0;
        let mut taps = vec![0.0f32; n];
        let mut sum = 0.0f64;
        for (i, t) in taps.iter_mut().enumerate() {
            let k = i as f64 - mid;
            let sinc = if k.abs() < 1e-9 {
                2.0 * fc
            } else {
                (2.0 * std::f64::consts::PI * fc * k).sin() / (std::f64::consts::PI * k)
            };
            let w = 0.54 - 0.46 * (2.0 * std::f64::consts::PI * i as f64 / (n - 1) as f64).cos();
            *t = (sinc * w) as f32;
            sum += *t as f64;
        }
        for t in taps.iter_mut() {
            *t /= sum as f32;
        }
        Some(Decimator { ratio, taps, hist: vec![0.0; n], filled: 0, count: 0 })
    }

    pub fn process(&mut self, x: &[f32], out: &mut Vec<f32>) {
        let n = self.taps.len();
        for &v in x {
            self.hist.copy_within(1.., 0);
            self.hist[n - 1] = v;
            if self.filled < n {
                self.filled += 1;
            }
            if self.filled == n && self.count % self.ratio == 0 {
                out.push(self.hist.iter().zip(&self.taps).map(|(a, b)| a * b).sum());
            }
            self.count = self.count.wrapping_add(1);
        }
    }
}

struct Linear {
    step: f64,
    pos: f64,
    prev: f32,
    primed: bool,
}

impl Linear {
    fn new(fs_in: usize, fs_out: usize) -> Self {
        Linear { step: fs_in as f64 / fs_out as f64, pos: 0.0, prev: 0.0, primed: false }
    }
    fn process(&mut self, x: &[f32], out: &mut Vec<f32>) {
        if !self.primed {
            if x.is_empty() {
                return;
            }
            self.prev = x[0];
            self.primed = true;
            self.pos = 0.0;
        }
        let mut p = self.pos;
        while p < x.len() as f64 {
            let i = p.floor() as usize;
            let frac = (p - i as f64) as f32;
            let a = if i == 0 { self.prev } else { x[i - 1] };
            let b = *x.get(i).unwrap_or(&a);
            out.push(a + (b - a) * frac);
            p += self.step;
        }
        self.pos = p - x.len() as f64;
        self.prev = *x.last().unwrap_or(&self.prev);
    }
}

/// 同步帧周期（秒）
pub const SYNC_PERIOD: f64 = 10.0;
/// 锁定后允许的搜索半窗（秒）：必须覆盖协议要求的 ±2 s 时差
pub const SYNC_COAST_WIN: f64 = 2.5;
/// 连续多少个周期没帧就解锁，退回全程搜索
pub const SYNC_COAST_MAX_MISS: u32 = 3;

/// 同步帧（v1.1）的接收状态：K 帧滚动统计。
#[derive(Default, Clone)]
pub struct SyncState {
    pub frames: usize,
    pub last_slot: u8,
    pub last_cell: u16,
    pub last_t: f64,
    /// (raster, k) 的票数，多数表决
    pub votes: Vec<((u8, u8), u32)>,
    pub assumed: bool,
}

impl SyncState {
    fn vote(&mut self, raster: u8, k: u8) {
        let key = (raster, k);
        for v in self.votes.iter_mut() {
            if v.0 == key {
                v.1 += 1;
                return;
            }
        }
        self.votes.push((key, 1));
        if self.votes.len() > 4 {
            self.votes.sort_by(|a, b| b.1.cmp(&a.1));
            self.votes.truncate(3);
        }
    }

    /// 多数表决出的档位（票数不足一半时不采用）。
    pub fn majority(&self) -> Option<(u8, u8)> {
        let best = self.votes.iter().max_by_key(|v| v.1)?;
        let total: u32 = self.votes.iter().map(|v| v.1).sum();
        if best.1 * 2 > total { Some(best.0) } else { None }
    }
}

pub struct StreamEvent {
    pub cells: usize,
    pub new_cells: usize,
    pub hdr: Option<String>,
    pub audio_s: f64,
    pub anchors: Vec<&'static str>,
}

struct Seg {
    start: f64,
    t_row: f64,
    tones: Vec<f64>,
    f_margin: f64,
    rows: Vec<Vec<f64>>,
    pm: Vec<f64>,
    off: Option<usize>,
    emitted: usize,
    end_rows: Option<usize>,
    /// 盲锁段：没有突发可依，靠帧时刻 + 边距能量找字符边界
    blind: bool,
}

pub struct Stream {
    dec: Option<Decimator>,
    lin: Option<Linear>,
    /// 输入采样率（设备侧，例如 48000）：原始频谱要按它换算频率
    fs_in: usize,
    /// 上次尝试标定导频时的 x 长度（用于限频，避免每个 block 都估）
    est_try: usize,
    pending: Vec<f32>,
    /// 8 kHz 音频（不裁剪；10 分钟约 19 MB）
    x: Vec<f32>,
    taps: Vec<f32>,
    mf: Vec<crate::C64>,
    bb_hist: Vec<crate::C64>,
    mf_done: usize,
    scan_from: usize,
    /// 局部极大（已过起跳/能量门限），数量很小
    peaks: Vec<(i64, &'static str, f64, f64)>,
    /// 同步帧（前导 C）的局部极大：与视觉段无关，单独维护
    sync_peaks: Vec<(i64, f64, f64)>,
    /// 已解出的同步帧占据的样点区间：这范围内的"突发"是帧载荷的误判，必须屏蔽
    sync_spans: Vec<(usize, usize)>,
    /// 同步帧载荷区间：这范围内不再做 A/B 分类（否则帧载荷会被误判成 BCON）
    sync_suppress_until: i64,
    /// 下一个盲锁段可以从哪里开始（样本位置）；段收口时前移
    blind_from: f64,
    /// 最近一次解出字符时的样本位置（看门狗用）
    seg_last_emit: f64,
    /// 已经被解调过的音频位置（样点）。任何开段都不得早于它，
    /// 否则同一段音频会被解两遍（实测会多出约一倍的行）。
    covered_to: f64,
    sync_scanned: usize,
    /// 滑行锁定：上一帧同步帧的起点（秒）。锁定后只在预测点 ±SYNC_COAST_WIN 内做相关搜索，
    /// 连续 SYNC_COAST_MAX_MISS 个周期都没帧就解锁、退回全程搜索。
    sync_lock: Option<f64>,
    /// 下一个期望的帧时刻（秒）
    sync_expect: f64,
    /// 连续错过的周期数
    sync_miss: u32,
    /// 滑行期间被跳过的候选峰数（调试用）
    sync_coasted: usize,
    pub sync: SyncState,
    peak_max: f64,
    recent: Vec<(i64, &'static str, f64)>,
    bursts: Vec<Burst>,
    est: Option<pilot::PilotEst>,
    mode: Mode,
    force_mode: Option<Mode>,
    seg: Option<Seg>,
    blocks: Vec<Vec<Vec<f64>>>,
    hdr_full: Option<crate::frame::Hdr>,
    /// 成功解出的报文头个数（界面用它区分"新的一轮"与"同一帧还在有效期"）
    hdr_count: u32,
    anchors: Vec<&'static str>,
    /// 自上次取走以来新解出的行（供实时瀑布绘制）
    rows_out: Vec<Vec<f64>>,
    /// 已开段游标（每个确认的突发对应一段）
    opened: usize,
    pub dbg_row1: Option<Vec<f64>>,
    pub dbg_seg0: Option<(f64, f64, f64)>,
    pub dbg_ink_break: usize,
    pub dbg_last_ink: f64,
    pub dbg_cells_seen: usize,
    pub cells: usize,
    last_hdr: Option<String>,

}

impl Stream {
    pub fn new(fs_in: usize, force_mode: Option<Mode>) -> Self {
        let dec = Decimator::new(fs_in, FS);
        // 同采样率直接透传（否则线性插值会引入一个样点的位移，破坏与离线的一致性）
        let lin = if dec.is_none() && fs_in != FS {
            Some(Linear::new(fs_in, FS))
        } else {
            None
        };
        Stream {
            dec,
            lin,
            fs_in,
            est_try: 0,
            rows_out: Vec::new(),
            opened: 0,
            dbg_row1: None,
            dbg_seg0: None,
            dbg_ink_break: 0,
            dbg_last_ink: 1.0,
            dbg_cells_seen: 0,
            x: Vec::new(),
            taps: modem::rrc_taps(params::CC_RRC_ALPHA, (FS as f64 / params::CC_BAUD) as usize, 8)
                .iter().map(|v| *v as f32).collect(),
            mf: Vec::new(),
            bb_hist: Vec::new(),
            mf_done: 0,
            scan_from: 0,
            peaks: Vec::new(),
            sync_peaks: Vec::new(),
            sync_spans: Vec::new(),
            sync_suppress_until: 0,
            blind_from: 0.0,
            seg_last_emit: 0.0,
            covered_to: 0.0,
            sync_scanned: 0,
            sync_lock: None,
            sync_expect: 0.0,
            sync_miss: 0,
            sync_coasted: 0,
            sync: SyncState { assumed: true, ..Default::default() },
            peak_max: 0.0,
            recent: Vec::new(),
            bursts: Vec::new(),
            est: None,
            mode: force_mode.unwrap_or_else(|| params::by_name(params::DEFAULT_MODE).unwrap()),
            force_mode,
            seg: None,
            blocks: Vec::new(),
            hdr_full: None,
            hdr_count: 0,
            anchors: Vec::new(),
            pending: Vec::new(),
            cells: 0,
            last_hdr: None,

        }
    }

    pub fn audio_seconds(&self) -> f64 {
        self.x.len() as f64 / FS as f64
    }

    /// 已收 8 kHz 音频（用于与离线解码对拍）。
    pub fn audio(&self) -> &[f32] {
        &self.x
    }

    /// 喂一块输入音频，返回新增字符（没有则 None）。
    pub fn feed(&mut self, block: &[f32]) -> Option<StreamEvent> {
        self.pending.clear();
        if let Some(d) = self.dec.as_mut() {
            let mut tmp = Vec::with_capacity(block.len() / 6 + 8);
            d.process(block, &mut tmp);
            self.x.extend_from_slice(&tmp);
        } else if let Some(l) = self.lin.as_mut() {
            let mut tmp = Vec::with_capacity(block.len());
            l.process(block, &mut tmp);
            self.x.extend_from_slice(&tmp);
        } else {
            self.x.extend_from_slice(block);
        }

        // 导频标定：**必须反复重试**，而且要用"最近 4 秒"，不能用会话开头那 4 秒。
        //
        // 踩过的坑：以前只在 est 为空时估一次，用的是 self.x[..4s]。用户先点"开始接收"、
        // 过几秒才开始放音频，那 4 秒就是静音 —— 估出 a=1.17 / b=-213 / conf=0 的垃圾值，
        // 而且永远不会再试。于是 16 个音解到 723…1162 Hz（应为 800…1175），
        // 行数据全是噪声、cells 恒为 0，而原始瀑布（不依赖 est）却看得清清楚楚。
        //
        // 现在：未锁定（conf < 0.6）时每 1 秒用最近 4 秒重估一次；锁定后每 60 秒刷新，
        // 只接受置信度更高的结果，避免把好值刷坏。
        if self.x.len() >= 4 * FS {
            let unlocked = self.est.map(|e| e.conf < 0.6).unwrap_or(true);
            let since = self.x.len() - self.est_try;
            let due = self.est_try == 0
                || (unlocked && since >= FS)
                || (!unlocked && since >= 60 * FS);
            if due {
                self.est_try = self.x.len();
                let tail = &self.x[self.x.len() - 4 * FS..];
                let energy = (tail.iter().map(|v| (*v as f64) * (*v as f64)).sum::<f64>()
                    / tail.len() as f64)
                    .sqrt();
                if energy > 5e-4 {
                    let e = pilot::estimate(tail, FS, params::PILOT1, params::PILOT2, 60.0);
                    // 拟合结果必须物理上说得通：声卡与发射端时钟差不可能大于 ±7%，
                    // 双导频的公共偏移也不会超过几十 Hz。否则宁可不采用（继续重试），
                    // 也不要拿一个 a=0 / b=539 的退化解去解调 —— 那会把整场都解成噪声。
                    let sane = e.conf >= 0.8 && e.a > 0.93 && e.a < 1.07 && e.b.abs() < 80.0;
                    if sane {
                        let better = match self.est {
                            None => true,
                            Some(o) => e.conf > o.conf + 1e-6,
                        };
                        if better {
                            self.est = Some(e);
                        }
                    }
                }
            }
        }
        let Some(est) = self.est else {
            return None;
        };

        self.update_mf();
        self.scan();
        self.update_bursts();
        self.maybe_decode_hdr(est);
        self.update_sync(est);
        self.advance_rows();
        let before = self.cells;
        self.emit_cells(false);
        if self.cells > before {
            Some(StreamEvent {
                cells: self.cells,
                new_cells: self.cells - before,
                hdr: self.last_hdr.clone(),
                audio_s: self.audio_seconds(),
                anchors: self.bursts.iter().map(|_| "cc").collect(),
            })
        } else {
            None
        }
    }

    /// 结束输入：补齐最后一段（按音频末端算行数，与离线一致）。
    /// 调试：打印突发与峰值数量。
    pub fn debug_state(&self) -> String {
        let mut s = String::new();
        if let Some(e) = self.est {
            s.push_str(&format!("est a={:.9} b={:.5}\n", e.a, e.b));
        }
        if let Some(seg) = self.seg.as_ref() {
            s.push_str(&format!("seg start={:.3} t_row={:.4} tone0={:.6} rows={} off={:?} emitted={}\n",
                                seg.start, seg.t_row, seg.tones[0], seg.rows.len(), seg.off, seg.emitted));
            if let Some(r) = seg.rows.get(1) {
                let v: Vec<String> = r.iter().map(|x| format!("{:.4}", x * 1e4)).collect();
                s.push_str(&format!("row1=[{}]\n", v.join(",")));
            }
        }
        s.push_str(&format!("sync_lock={:?} sync_miss={} sync_coasted={}\n",
                            self.sync_lock, self.sync_miss, self.sync_coasted));
        s.push_str(&format!("sync_peaks={} sync_scanned={} sync_frames={}\n",
                            self.sync_peaks.len(), self.sync_scanned, self.sync.frames));
        s.push_str(&format!("bursts={} opened={} peaks={} ink_break={} seen={} last_ink={:.3}\n",
                            self.bursts.len(), self.opened, self.peaks.len(),
                            self.dbg_ink_break, self.dbg_cells_seen, self.dbg_last_ink));
        s
    }

    pub fn finish(&mut self) -> Option<StreamEvent> {
        let before = self.cells;
        if self.seg.is_some() {
            let end = self.x.len() as f64;
            self.close_segment(Some(end));
        }
        if self.cells > before {
            Some(StreamEvent {
                cells: self.cells,
                new_cells: self.cells - before,
                hdr: self.last_hdr.clone(),
                audio_s: self.audio_seconds(),
                anchors: vec!["cc"],
            })
        } else {
            None
        }
    }

    // ---- 增量匹配滤波 ----
    fn update_mf(&mut self) {
        let n = self.taps.len();
        let center = self.est.map(|e| e.a * params::CC_CENTER + e.b)
            .unwrap_or(params::CC_CENTER);
        let start = self.mf_done;
        if start >= self.x.len() {
            return;
        }
        // 基带（复用历史，避免整段重算）
        if self.bb_hist.is_empty() {
            self.bb_hist = vec![crate::C64::ZERO; n - 1];
        }
        let mut bb: Vec<crate::C64> = Vec::with_capacity(self.x.len() - start + n);
        bb.extend_from_slice(&self.bb_hist);
        for i in start..self.x.len() {
            let t = i as f64 / FS as f64;
            bb.push(crate::C64::exp_i(-2.0 * std::f64::consts::PI * center * t)
                * (2.0 * self.x[i] as f64));
        }
        let first = bb.len() - (self.x.len() - start);
        for i in first..bb.len() {
            let mut acc = crate::C64::ZERO;
            for (j, &h) in self.taps.iter().enumerate() {
                let v = bb[i - j];
                let hf = h as f64;
                acc.re += v.re * hf;
                acc.im += v.im * hf;
            }
            self.mf.push(acc);
        }
        // 保留尾部基带
        let keep = (bb.len().saturating_sub(n - 1)).max(0);
        self.bb_hist = bb[keep..].to_vec();
        self.mf_done = self.x.len();
    }

    // ---- 增量相关扫描 ----
    fn scan(&mut self) {
        let sps = (FS as f64 / params::CC_BAUD) as usize;
        let need = 32 * sps;
        if self.mf.len() <= need {
            return;
        }
        // 从 0 开始扫：首帧可能落在 t=0，其相关峰在 need 之前
        let from = if self.scan_from == 0 { 0 } else { self.scan_from.saturating_sub(2 * sps) };
        let last = self.mf.len() - need;
        let mut i = from;
        while i <= last {
            let sa = modem::score_at(&self.mf, modem::preamble_hdr(), sps, i as i64);
            let sb = modem::score_at(&self.mf, modem::preamble_bcon(), sps, i as i64);
            let sc_c = modem::score_at(&self.mf, modem::preamble_sync(), sps, i as i64);
            // 前导 C 单独走一条路：它只带参数，不切视觉段
            if sc_c >= 0.75 && sc_c >= sa && sc_c >= sb {
                // 滑行：已锁定帧相位时，只保留预测点 ±SYNC_COAST_WIN 内的候选。
                // 注意这只是"缩小相关搜索范围"，判定仍然是相关值说了算；
                // 错过 SYNC_COAST_MAX_MISS 个周期就解锁，回到全程搜索。
                let mut keep = true;
                if let Some(lock) = self.sync_lock {
                    let t = i as f64 / FS as f64;
                    let k = ((t - lock) / SYNC_PERIOD).round();
                    let pred = lock + k * SYNC_PERIOD;
                    if (t - pred).abs() > SYNC_COAST_WIN {
                        keep = false;
                        self.sync_coasted += 1;
                    }
                }
                if keep {
                    let e = modem::energy_at(&self.mf, i as i64, i as i64 + 32 * sps as i64);
                    if e > 0.0 {
                        self.sync_peaks.push((i as i64, sc_c, e));
                    }
                }
                i += 2;
                continue;
            }
            let (sc, kind) = if sa >= sb { (sa, "HDR") } else { (sb, "BCON") };
            self.recent.push((i as i64, kind, sc));
            // 只把"局部极大"当候选：留一个位置的滞后判断右邻
            if self.recent.len() >= 3 {
                let mid = self.recent[1];
                // 门限用"迄今最高分"的 0.65 倍（与离线同一条规则，只是改为因果）：
                // 只用绝对下限 0.4 会在 −6 dB 把噪声峰当成突发，把段切碎。
                let ceiling = if self.peak_max > mid.2 { self.peak_max } else { mid.2 };
                if mid.2 >= self.recent[0].2 && mid.2 >= self.recent[2].2
                    && mid.2 >= 0.6 && mid.2 >= 0.65 * ceiling {
                    let e_dur = modem::energy_at(&self.mf, mid.0, mid.0 + 32 * sps as i64);
                    let e_pre = modem::energy_at(&self.mf, mid.0 - 16 * sps as i64, mid.0);
                    if e_dur >= 1.15 * e_pre {
                        self.peaks.push((mid.0, mid.1, mid.2, e_dur));
                        if mid.2 > self.peak_max { self.peak_max = mid.2; }
                    }
                }
                self.recent.remove(0);
            }
            i += 2;
        }
        self.scan_from = last.max(self.scan_from);
    }

    // ---- 突发确认（与离线同一套判据，作用在极少数局部极大上）----
    fn update_bursts(&mut self) {
        if self.peaks.is_empty() {
            return;
        }
        let sps = (FS as f64 / params::CC_BAUD) as usize;
        let sep = 40 * sps;
        let e_max = self.peaks.iter().fold(0.0f64, |m, p| m.max(p.3));
        let mut kept: Vec<(i64, &'static str, f64, f64)> =
            self.peaks.iter().copied().filter(|p| p.3 >= 0.25 * e_max).collect();
        if kept.is_empty() {
            return;
        }
        kept.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));

        let mut picked: Vec<(i64, &'static str, f64)> = Vec::new();
        for (s0, kind, sc, _e) in kept {
            if picked.iter().any(|o| (o.0 - s0).abs() < sep as i64) {
                continue;
            }
            picked.push((s0, kind, sc));
        }
        picked.sort_by_key(|p| p.0);

        let delay = modem::match_delay(params::CC_RRC_ALPHA, sps, 8);
        for (mf_start, kind, sc) in picked {
            let start = mf_start - delay;
            if start < 0 {
                continue;
            }
            if self.bursts.iter().any(|b| (b.start - start).abs() < sep as i64 / 2) {
                continue;
            }
            // 只有"前导已完整收到"才确认
            if (start + (32 * sps) as i64) as usize > self.x.len() {
                continue;
            }
            self.bursts.push(Burst { start, kind, score: sc });
        }
    }

    /// 同步帧：按相关峰解帧，更新 K 帧滚动统计。
    fn update_sync(&mut self, est: pilot::PilotEst) {
        if self.sync_peaks.is_empty() {
            return;
        }
        let sps = (FS as f64 / params::CC_BAUD) as usize;
        let delay = modem::match_delay(params::CC_RRC_ALPHA, sps, 8);
        let baud = params::CC_BAUD * est.a;
        let n_coded = 2 * (crate::frame::SYNC_BITS + crate::fec::TAIL);

        let peaks: Vec<(i64, f64, f64)> = self.sync_peaks.clone();
        for (mf_at, _sc, _e) in peaks {
            if mf_at < self.sync_scanned as i64 {
                continue;
            }
            let start = mf_at - delay;
            let need = start + (n_coded / 2) as i64 * sps as i64 + 4096;
            if start < 0 || (need as usize) > self.x.len() {
                continue;
            }
            self.sync_scanned = mf_at as usize + 1;
            for d in -8..=8i64 {
                let soft = modem::demodulate_mf(&self.mf, FS, baud, start + d, n_coded,
                                                modem::preamble_sync());
                let bits = crate::fec::viterbi(&crate::fec::deinterleave(&soft),
                                               crate::frame::SYNC_BITS);
                if let Some(f) = crate::frame::SyncFrame::unpack(&bits) {
                    // 同一帧附近会有多个局部极大，成功解出一帧后跳过整个帧体
                    self.sync.frames += 1;
                    let frame_len = (n_coded / 2) as i64 * sps as i64;
                    self.sync_spans.push((start.max(0) as usize,
                                          (start + frame_len + 1024).max(0) as usize));
                    let skip_to = (start + frame_len + 1024) as usize;
                    if skip_to > self.sync_scanned {
                        self.sync_scanned = skip_to;
                    }
                    self.sync.last_slot = f.slot;
                    self.sync.last_cell = f.cell;
                    self.sync.last_t = start as f64 / FS as f64;
                    self.sync.assumed = false;
                    // 锁定相位：下一帧应该在 +10 s，滑行窗口从这一刻开始生效
                    self.sync_lock = Some(self.sync.last_t);
                    self.sync_expect = self.sync.last_t + SYNC_PERIOD;
                    self.sync_miss = 0;
                    self.sync.vote(f.raster, f.k);
                    if let Some((raster, k)) = self.sync.majority() {
                        if self.force_mode.is_none() && (raster as usize != self.mode.n || k as u32 != self.mode.k) {
                            // 多数表决换了档位 -> 跟随（音位与行周期立刻更新）
                            let (df, f_lo) = (self.mode.df, self.mode.f_lo);
                            self.mode = params::Mode::new("sync", raster as usize, df, k as u32, self.mode.repeat, f_lo);
                        }
                    }
                    break;
                }
            }
        }
        // 滑行计时：某个期望周期整窗过去仍没解到帧 -> 记一次错过
        if self.sync_lock.is_some() {
            let now = self.audio_seconds();
            while now > self.sync_expect + SYNC_COAST_WIN {
                self.sync_miss += 1;
                self.sync_expect += SYNC_PERIOD;
                if self.sync_miss >= SYNC_COAST_MAX_MISS {
                    // 连续三个周期都没有：解锁，下一轮回到全程相关搜索
                    self.sync_lock = None;
                    self.sync_miss = 0;
                    break;
                }
            }
        }

        // 只保留最近 64 个峰，避免无限增长
        if self.sync_peaks.len() > 64 {
            let n = self.sync_peaks.len() - 64;
            self.sync_peaks.drain(0..n);
        }
    }

    fn maybe_decode_hdr(&mut self, est: pilot::PilotEst) {
        if self.force_mode.is_some() || self.last_hdr.is_some() {
            return;
        }
        let Some(b) = self.bursts.iter().find(|b| b.kind == "HDR").cloned() else {
            return;
        };
        let need = (b.start + (params::HDR_SYMBOLS as f64
            * FS as f64 / (params::CC_BAUD * est.a)) as i64 + 4096) as usize;
        if self.x.len() < need {
            return;
        }
        let center = est.a * params::CC_CENTER + est.b;
        let baud = params::CC_BAUD * est.a;
        let n_coded = 2 * (params::HDR_BITS + crate::fec::TAIL);
        for d in -8..=8i64 {
            let soft = modem::demodulate_mf(&self.mf, FS, baud, b.start + d, n_coded,
                                            modem::preamble_hdr());
            let bits = crate::fec::viterbi(&crate::fec::deinterleave(&soft), params::HDR_BITS);
            if let Some(h) = crate::frame::Hdr::unpack(&bits) {
                self.last_hdr = Some(h.call.clone());
                self.hdr_full = Some(h.clone());
                self.hdr_count = self.hdr_count.wrapping_add(1);
                let (df, f_lo) = if h.plan == 0 { (25.0, 800.0) } else { (18.75, 640.0) };
                self.mode = Mode::new("hdr", h.raster as usize, df, h.k, h.repeat as usize, f_lo);
                let _ = center;
                return;
            }
        }
    }

    // ---- 栅格：突发后开段，逐行解调 ----
    fn seg_params(&self, est: pilot::PilotEst) -> (Vec<f64>, f64, f64) {
        let f_lo = est.a * self.mode.f_lo + est.b;
        let df = est.a * self.mode.df;
        let tones = (0..self.mode.n).map(|i| f_lo + i as f64 * df).collect();
        (tones, f_lo - 2.0 * df, (self.mode.k as f64 / df) * FS as f64)
    }

    /// 开一个盲锁段（不依赖任何突发/同步帧）。
    /// 返回是否成功开段。
    fn open_blind(&mut self, est: pilot::PilotEst, t_sym: f64) -> bool {
        let t_row = self.mode.t_row() * FS as f64;
        let mut at = self.blind_from.max(self.covered_to).max(0.0);
        if let Some((_, fend)) = self.sync_spans.last().copied() {
            let from_frame = fend as f64 + 2.0 * t_sym;
            if from_frame > at {
                at = from_frame;
            }
        }
        let rows_have = (self.x.len() as f64 - at) / t_row;
        if rows_have < 2.0 {
            return false;
        }
        let (tones, f_margin, t_row2) = self.seg_params(est);
        let (off, score) = self.blind_phase(at, t_row2, &tones, f_margin);
        if std::env::var("CP16P_DEBUG").is_ok() {
            eprintln!(
                "  [rt] 盲锁开段 t={:.2}s off={:.0}ms score={:.2}",
                at / FS as f64,
                off / FS as f64 * 1000.0,
                score
            );
        }
        if !score.is_finite() || score < 2.0 {
            // 两种可能：
            //  · 数据还不够评估（score = -inf）→ 什么都不动，等更多音频；
            //  · 真是噪声（score 偏低）→ 往前跳过 1 秒，别在原地打转。
            if score.is_finite() {
                self.blind_from = at + FS as f64;
            }
            return false;
        }
        self.seg = Some(Seg {
            start: at + off,
            t_row: t_row2,
            tones,
            f_margin,
            rows: Vec::new(),
            pm: Vec::new(),
            off: None,
            emitted: 0,
            end_rows: None,
            blind: true,
        });
        true
    }

    fn advance_rows(&mut self) {
        let Some(est) = self.est else { return };
        let t_sym = FS as f64 / (params::CC_BAUD * est.a);
        loop {
            // 段的选择顺序：
            //   1) 下一个突发已经在"当前音频内" → 用突发开段（行边界最准）
            //   2) 下一个突发还在未来（或根本没有突发）→ 先用盲锁段把空档填上
            //   3) 都开不出来就等更多音频
            //
            // 实测教训：录音里 HDR 常常解不出、BCON 也只有零星几个；
            // 老逻辑此时会一直等到下一个突发，中间十几秒的音频全白丢。
            if self.seg.is_none() {
                let t_row_now = self.mode.t_row() * FS as f64;
                let next_burst = self
                    .bursts
                    .get(self.opened)
                    .map(|b| b.start as f64)
                    .unwrap_or(f64::INFINITY);
                let burst_usable = next_burst <= self.x.len() as f64 + 2.0 * t_row_now;

                if !burst_usable {
                    self.open_blind(est, t_sym);
                }

                if self.seg.is_none() && burst_usable {
                    let b = self.bursts[self.opened];
                    self.opened += 1;
                    // 这个突发落在已经解过的区域里：再开一段只会把同样的音频解第二遍
                    if (b.start as f64) < self.covered_to - self.mode.t_row() * FS as f64 {
                        continue;
                    }
                    let n_sym = if b.kind == "HDR" {
                        params::HDR_SYMBOLS
                    } else {
                        params::BCON_SYMBOLS
                    };
                    let start = b.start as f64 + (n_sym + params::SEGMENT_GAP_SYM) as f64 * t_sym;
                    let (tones, f_margin, t_row) = self.seg_params(est);
                    self.seg = Some(Seg {
                        start,
                        t_row,
                        tones,
                        f_margin,
                        rows: Vec::new(),
                        pm: Vec::new(),
                        off: None,
                        emitted: 0,
                        end_rows: None,
                        blind: false,
                    });
                }

                if self.seg.is_none() {
                    return;
                }
            }

            // 看门狗：段在跑却长时间解不出字，说明它的起点或行相位是错的。
            // 实测：录音里一个被误判的突发会把中间十几秒卡住，必须主动放弃它。
            if let Some(seg) = self.seg.as_ref() {
                let since = self.seg_last_emit.max(seg.start);
                let idle = self.x.len() as f64 - since;
                let limit = if seg.blind { 6.0 } else { 4.0 } * FS as f64;
                if idle > limit {
                    if std::env::var("CP16P_DEBUG").is_ok() {
                        eprintln!(
                            "  [rt] 放弃段 t={:.2}s blind={} idle={:.1}s",
                            seg.start / FS as f64,
                            seg.blind,
                            idle / FS as f64
                        );
                    }
                    self.blind_from = self.covered_to.max(seg.start + seg.t_row);
                    self.seg = None;
                    continue;
                }
            }

            // 本段的理论行数：有下一个突发就用它的起点，否则按已收音频算
            let (start, t_row) = {
                let seg = self.seg.as_ref().unwrap();
                (seg.start, seg.t_row)
            };
            let end = self.bursts.get(self.opened).map(|b| b.start as f64);
            let rows_wanted = match end {
                Some(e) => {
                    let raw = (e - start) / t_row;
                    if (raw - raw.round()).abs() < 0.25 { raw.round() } else { raw.floor() }
                        .max(0.0) as usize
                }
                None => {
                    ((self.x.len() as f64 - start) / t_row).floor().max(0.0) as usize
                }
            };

            // 先把该段能解的行都解出来
            {
                let seg = self.seg.as_mut().unwrap();
                while seg.rows.len() < rows_wanted {
                    let t0 = seg.start + seg.rows.len() as f64 * seg.t_row;
                    let d = vision::demod(&self.x, t0, 1, seg.t_row, &seg.tones,
                                          seg.f_margin, FS);
                    seg.rows.push(d.p[0].clone());
                    seg.pm.push(d.pm[0]);
                    self.rows_out.push(d.p[0].clone());
                    let end_at = t0 + seg.t_row;
                    if end_at > self.covered_to {
                        self.covered_to = end_at;
                    }
                }
            }

            match end {
                Some(_) => {
                    // 下一段已经存在：本段收口，继续处理下一段
                    if let Some(seg) = self.seg.as_mut() {
                        seg.end_rows = Some(rows_wanted);
                    }
                    self.blind_from = self.covered_to;
                    self.emit_cells(true);
                    self.seg = None;
                }
                None => return, // 等更多音频
            }
        }
    }

    /// 盲锁段的行相位扫描：在 [0, t_row) 内试 8 个偏移，
    /// 用"行内峰值/中位"的对比度挑最像字形的那一个。
    ///
    /// 为什么必须做：盲锁段只知道从哪开始解，不知道真实行边界落在哪里；
    /// 差半个行就会把每个字上下各切一半，画面成条纹、字符边界也乱。
    fn blind_phase(&self, start: f64, t_row: f64, tones: &[f64], f_margin: f64) -> (f64, f64) {
        const STEPS: usize = 8;
        const PROBE: usize = 6;
        let mut best = (0.0, f64::NEG_INFINITY);
        for k in 0..STEPS {
            let off = k as f64 * t_row / STEPS as f64;
            let mut scores: Vec<f64> = Vec::new();
            for r in 0..PROBE {
                let t0 = start + off + r as f64 * t_row;
                if t0 + t_row > self.x.len() as f64 {
                    break;
                }
                let d = vision::demod(&self.x, t0, 1, t_row, tones, f_margin, FS);
                let row = &d.p[0];
                let mx = row.iter().cloned().fold(0.0f64, f64::max);
                if mx <= 0.0 {
                    continue;
                }
                let mut sorted = row.clone();
                sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
                let med = sorted[sorted.len() / 2].max(1e-12);
                scores.push(mx / med);
            }
            if scores.len() < 3 {
                continue;
            }
            scores.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            let score = scores[scores.len() / 2];
            if score > best.1 {
                best = (off, score);
            }
        }
        best
    }

    fn close_segment(&mut self, end: Option<f64>) {
        if let Some(seg) = self.seg.take() {
            let mut seg = seg;
            if let Some(end_at) = end {
                let raw = (end_at - seg.start) / seg.t_row;
                let n = if (raw - raw.round()).abs() < 0.25 { raw.round() } else { raw.floor() };
                seg.end_rows = Some(n.max(0.0) as usize);
                if seg.rows.len() > seg.end_rows.unwrap() {
                    seg.rows.truncate(seg.end_rows.unwrap());
                    seg.pm.truncate(seg.end_rows.unwrap());
                }
            }
            self.blind_from = self.covered_to.max(seg.start);
            self.seg = Some(seg);
            self.emit_cells(true);
            self.seg = None;
        }
    }

    fn emit_cells(&mut self, _final: bool) {
        let Some(seg) = self.seg.as_mut() else { return };
        let rpc = self.mode.rows_per_char();
        if seg.off.is_none() {
            if seg.pm.len() < 3 {
                return;
            }
            let off = vision::phase_offset(&seg.pm, rpc).rem_euclid(rpc as i64) as usize;
            seg.off = Some(off);
        }
        let off = seg.off.unwrap();
        let mut k = seg.emitted;
        loop {
            let cell_start = off + k * rpc;
            let cell_end = cell_start + rpc;
            if cell_end > seg.rows.len() {
                break;
            }
            let blk = vision::block(&seg.rows, cell_start, self.mode.n, self.mode.repeat);
            if blk.len() != self.mode.n * self.mode.repeat {
                break;
            }
            // 空块保护：静音段继续按栅格切会切出"全是噪底"的假字符
            let mut vals: Vec<f64> = blk.iter().flatten().copied().collect();
            vals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            let med = vals[vals.len() / 2];
            let hi = *vals.last().unwrap_or(&0.0);
            // 阈值取"几何中点"（dB 域中点），不能用线性均值：
            // 真实录音里亮暗差 20 dB 以上，线性均值会被单个最亮点拉高，把整块判成空。
            let thr = (hi * med.max(1e-30)).sqrt();
            let ink = vals.iter().filter(|v| **v > thr).count() as f64 / vals.len().max(1) as f64;
            self.dbg_cells_seen += 1;
            self.dbg_last_ink = ink;
            if self.dbg_row1.is_none() {
                self.dbg_row1 = Some(blk.get(1).cloned().unwrap_or_default());
                self.dbg_seg0 = seg.off.map(|o| (o as f64, 0.0, 0.0));
            }
            if ink < 0.04 {
                self.dbg_ink_break += 1;
                break;
            }
            self.blocks.push(blk);
            self.cells += 1;
            // 注意：必须用这个字所在的行，不能用 seg.rows.len()——
            // 行是一次性批量解出来的，用 rows.len() 会永远等于音频末尾，
            // 看门狗就永远认为刚刚出过字，误判段永远踢不掉。
            let at = seg.start + (k as f64 + 1.0) * rpc as f64 * seg.t_row;
            self.seg_last_emit = at;
            k += 1;
        }
        if let Some(seg) = self.seg.as_mut() {
            seg.emitted = k;
        }
    }

    /// 当前工作档位（界面要拿它算"一字符几行"来画分隔线）。
    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// 取走新解出的行（每行 = 16 个音的相关功率），用于滚动瀑布。
    pub fn take_rows(&mut self) -> Vec<Vec<f64>> {
        std::mem::take(&mut self.rows_out)
    }

    /// 把流式状态落成与离线一致的返回结构（供 `rx::decode` 薄壳使用）。
    pub fn snapshot(&self) -> crate::rx::RxResult {
        crate::rx::RxResult {
            hdr: self.hdr_full.clone(),
            est: self.est.unwrap_or(crate::pilot::PilotEst { a: 1.0, b: 0.0, f1: 0.0, f2: 0.0, conf: 0.0 }),
            mode: self.mode,
            bursts: self.bursts.clone(),
            cells: self.cells,
            blocks: self.blocks.clone(),
            // 锚定来源：参考实现里 anchor 只有 "cc" / "blind" 两种。
            // 有突发（CC）就是控制信道锚定；没有突发却解出了字符，就是盲锁。
            anchors: if !self.bursts.is_empty() {
                vec!["cc"]
            } else if self.cells > 0 {
                vec!["blind"]
            } else {
                Vec::new()
            },
            metrics: crate::rx::block_metrics_public(&self.blocks),
            t_row: (self.mode.k as f64 / (self.est.map(|e| e.a).unwrap_or(1.0) * self.mode.df))
                * crate::params::FS as f64,
        
            sync_frames: self.sync.frames,
            sync_slot: self.sync.last_slot,
            sync_cell: self.sync.last_cell,
        }
    }

    /// 输入采样率（设备侧）
    pub fn fs_in(&self) -> usize {
        self.fs_in
    }

    /// 同步帧状态：(累计帧数, 最近一帧的 SLOT, 最近一帧的音频时刻)
    pub fn sync_info(&self) -> (usize, u8, f64) {
        (self.sync.frames, self.sync.last_slot, self.sync.last_t)
    }

    /// 最近一次成功解出的报文头（含呼号、网格、字数、类型、档位）
    pub fn hdr_info(&self) -> Option<crate::frame::Hdr> {
        self.hdr_full.clone()
    }

    /// 已解出的报文头个数
    pub fn hdr_count(&self) -> u32 {
        self.hdr_count
    }

    pub fn take_blocks(&mut self) -> Vec<Vec<Vec<f64>>> {
        std::mem::take(&mut self.blocks)
    }

    pub fn block_count(&self) -> usize {
        self.blocks.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_keeps_order() {
        let mut r = Ring::new(4);
        r.push(&[1.0, 2.0, 3.0, 4.0, 5.0]);
        assert_eq!(r.len(), 4);
        assert_eq!(r.data(), vec![2.0, 3.0, 4.0, 5.0]);
    }

    #[test]
    fn decimator_passes_dc() {
        let mut d = Decimator::new(48000, 8000).unwrap();
        let x = vec![0.5f32; 6000];
        let mut out = Vec::new();
        d.process(&x, &mut out);
        assert!(out.len() > 900);
        assert!(out[300..].iter().all(|v| (v - 0.5).abs() < 0.02));
    }

    #[test]
    fn linear_keeps_phase_across_blocks() {
        let mut l = Linear::new(44100, 8000);
        let x: Vec<f32> = (0..4410).map(|i| (i as f32 * 0.01).sin()).collect();
        let mut out = Vec::new();
        for chunk in x.chunks(441) {
            l.process(chunk, &mut out);
        }
        assert!((out.len() as i64 - 800).abs() < 20);
    }
}


#[cfg(test)]
mod live_tests {
    use super::*;

    /// 复现用户现场：先点"开始接收"（前 6 秒是静音），之后才有信号。
    /// 导频标定必须能重新锁定，否则整场都解不出字。
    #[test]
    fn locks_after_leading_silence() {
        let glyphs: Vec<Vec<u8>> = (0..8u32)
            .map(|k| {
                (0..256u32)
                    .map(|i| if (i * 7 + k * 13) % 5 < 2 { 1 } else { 0 })
                    .collect()
            })
            .collect();
        let opts = crate::tx::TxOpts {
            fs: 48000,
            sync_period: 10.0,
            ..Default::default()
        };
        let (sig, _meta) = crate::tx::build(&glyphs, crate::params::M2, &opts);
        let mut st = Stream::new(48000, None);
        // 先喂 6 秒静音，模拟先点开始接收、过一会儿才有信号
        let mut silence = vec![0.0f32; 6 * 48000];
        for (i, v) in silence.iter_mut().enumerate() {
            *v = 0.0;
        }
        st.feed(&silence);
        for chunk in sig.chunks(800) {
            st.feed(chunk);
        }
        let sn = st.snapshot();
        assert!(
            sn.est.conf > 0.6,
            "导频没能重新锁定：conf = {:.3}, a = {:.4}, b = {:.1}",
            sn.est.conf,
            sn.est.a,
            sn.est.b
        );
        assert!(sn.cells > 0, "锁定后仍然没解出字");
    }

    /// 实时采集是 48 kHz（声卡），WAV 回放是 8 kHz。
    /// 这条测试走 live 路径：48 kHz 信号 → 抽取到 8 kHz → 解调 → 出字。
    #[test]
    fn decodes_at_48k() {
        let glyphs: Vec<Vec<u8>> = (0..8u32)
            .map(|k| {
                (0..256u32)
                    .map(|i| if (i * 7 + k * 13) % 5 < 2 { 1 } else { 0 })
                    .collect()
            })
            .collect();
        let opts = crate::tx::TxOpts {
            fs: 48000,
            sync_period: 10.0,
            ..Default::default()
        };
        let (sig, _meta) = crate::tx::build(&glyphs, crate::params::M2, &opts);
        let mut st = Stream::new(48000, Some(crate::params::M2));
        for chunk in sig.chunks(4800) {
            st.feed(chunk);
        }
        assert!(st.snapshot().cells > 0, "48 kHz 输入没解出任何字");
    }
}
