//! 实时采集（cpal 后端）+ 发射播放（同一个 cpal，输出设备可选）。
//!
//! 为什么用 cpal 而不是自写 WASAPI：真实使用场景里必须**能选输入设备**
//! （电台的 USB 声卡、SDR 的虚拟声卡，都不是"默认麦克风"），
//! 而设备枚举/格式协商/重采样这些细节 cpal 已经处理得足够稳。
//! 采集线程只做一件事：交错样本 → 单声道 f32 → 通道。
//!
//! 发射侧：引擎按 UTC 整十秒对齐后调用 `play()`，阻塞到音频播完。
//! 输出设备同样可选——电台通常接在一块特定的 USB 声卡上。

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

pub struct SourceConfig {
    /// None = 系统默认输入
    pub device: Option<String>,
    /// None = 设备默认采样率
    pub sample_rate: Option<u32>,
    /// 保留字段（cpal 不提供回环；用虚拟声卡设备名代替）
    pub loopback: bool,
    /// 声道策略：默认自动选一路（避免双声道平均的梳状滤波）
    pub channel: ChannelMode,
}

impl Default for SourceConfig {
    fn default() -> Self {
        SourceConfig { device: None, sample_rate: None, loopback: false, channel: ChannelMode::Auto }
    }
}

pub struct Capture {
    _stream: cpal::Stream,
    rx: Receiver<Vec<f32>>,
    pub fs: u32,
    pub channels: u16,
    pub name: String,
    /// 电平/削顶/声道计量
    pub meter: std::sync::Arc<Meter>,
    /// 实际使用的声道（自动模式下会变）
    pub ch_used: usize,
}

impl Capture {
    pub fn recv(&self) -> Option<Vec<f32>> {
        self.rx.recv_timeout(std::time::Duration::from_millis(500)).ok()
    }
}

/// 列出全部输入设备名（给界面做设备选择）。
pub fn list_devices() -> Vec<String> {
    let host = cpal::default_host();
    match host.input_devices() {
        Ok(devs) => devs.filter_map(|d| d.name().ok()).collect(),
        Err(_) => Vec::new(),
    }
}

/// 列出全部输出设备名（发射侧选择用）。
pub fn list_output_devices() -> Vec<String> {
    let host = cpal::default_host();
    match host.output_devices() {
        Ok(devs) => devs.filter_map(|d| d.name().ok()).collect(),
        Err(_) => Vec::new(),
    }
}

pub fn default_device_name() -> Option<String> {
    cpal::default_host().default_input_device().and_then(|d| d.name().ok())
}

pub fn default_output_device_name() -> Option<String> {
    cpal::default_host().default_output_device().and_then(|d| d.name().ok())
}


// ---------------------------------------------------------------- 采集前处理
//
// 为什么需要这些：
//  1) 双声道直接平均会产生**梳状滤波**：两路有距离 Δ 时，在 f = c/(2Δ) 的奇次倍处出零点。
//     Δ=6 cm → 2.8 kHz，Δ=12 cm → 1.4 kHz，加上外壳声学路径完全可以落进 800–1200 Hz
//     的字形带，把某些音挖掉 15–20 dB。而瀑布图靠上下文仍然"看得见"，所以一直没被发现。
//     → 默认改为"自动选一路更响的"，并允许强制指定某一路或恢复混音。
//  2) 麦克风普遍带直流偏置（实测 DC 处比中位高 60+ dB），白占动态范围。
//     → 加一级 ~30 Hz 的一阶高通（对 450 Hz 导频只有 0.03 dB 影响）。
//  3) 没有电平/削顶信息，用户无法判断"信号弱"还是"解错了"。
//     → 回调里统计 RMS / 峰值 / 削顶比例，走 /api/status 暴露出来（界面暂不动）。

/// 声道策略
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ChannelMode {
    /// 自动：前 0.5 秒量一下两路能量，选更响的一路；之后每 5 秒复查，只有另一路强 6 dB 以上才切换
    Auto,
    /// 混音（旧行为，保留用于对照实验）
    Mix,
    /// 固定用第 n 路
    Index(usize),
}

pub struct Downmix {
    ch: usize,
    mode: ChannelMode,
    chosen: usize,
    acc: [f64; 2],
    frames: usize,
    cal_len: usize,
    ready: bool,
    /// 发生过切换（供上层打日志）
    pub switched: bool,
}

impl Downmix {
    pub fn new(ch: usize, mode: ChannelMode, fs: u32) -> Self {
        let chosen = match mode {
            ChannelMode::Index(i) => i.min(ch.saturating_sub(1)),
            _ => 0,
        };
        Downmix {
            ch,
            mode,
            chosen,
            acc: [0.0; 2],
            frames: 0,
            cal_len: ((fs as f64 * 0.5) as usize).max(64),
            ready: matches!(mode, ChannelMode::Mix | ChannelMode::Index(_)),
            switched: false,
        }
    }

    pub fn chosen(&self) -> usize {
        self.chosen
    }

    pub fn push(&mut self, frame: &[f32], out: &mut Vec<f32>) {
        let ch = self.ch;
        if ch == 0 {
            out.push(0.0);
            return;
        }
        if ch == 1 {
            out.push(frame[0]);
            return;
        }
        match self.mode {
            ChannelMode::Mix => {
                let s: f32 = frame.iter().take(ch).sum();
                out.push(s / ch as f32);
            }
            ChannelMode::Index(i) => out.push(frame[i.min(ch - 1)]),
            ChannelMode::Auto => {
                let a = frame[0] as f64;
                let b = frame[1.min(ch - 1)] as f64;
                self.acc[0] += a * a;
                self.acc[1] += b * b;
                self.frames += 1;
                if !self.ready {
                    out.push(frame[self.chosen.min(ch - 1)]);
                    if self.frames >= self.cal_len {
                        self.chosen = if self.acc[1] > self.acc[0] * 2.0 { 1 } else { 0 };
                        self.ready = true;
                        self.acc = [0.0; 2];
                        self.frames = 0;
                        self.switched = true;
                    }
                } else {
                    out.push(frame[self.chosen.min(ch - 1)]);
                    // 每 5 秒复查一次：只有另一路强 6 dB 以上才切，避免来回跳
                    let window = self.cal_len * 10;
                    if self.frames >= window {
                        let other = 1 - self.chosen.min(1);
                        if self.acc[other] > self.acc[self.chosen.min(1)] * 4.0 {
                            self.chosen = other;
                            self.switched = true;
                        }
                        self.acc = [0.0; 2];
                        self.frames = 0;
                    }
                }
            }
        }
    }
}

/// 一阶高通（去直流 + 低频隆隆声）。截止 ~30 Hz，对 450 Hz 导频影响可忽略。
pub struct DcBlock {
    a: f32,
    x1: f32,
    y1: f32,
    primed: bool,
}

impl DcBlock {
    pub fn new(fs: u32) -> Self {
        let fc = 30.0f32;
        let a = 1.0 - 2.0 * std::f32::consts::PI * fc / fs.max(1) as f32;
        DcBlock { a: a.clamp(0.9, 0.99999), x1: 0.0, y1: 0.0, primed: false }
    }
    #[inline]
    pub fn process(&mut self, x: f32) -> f32 {
        // 第一个样点只用来上电：否则设备启动的直流跳变会被高通整成一个
        // 幅度 >1 的尖峰（实测峰值 2.6），既可能触发假削顶告警，也污染开头。
        if !self.primed {
            self.primed = true;
            self.x1 = x;
            return 0.0;
        }
        let y = x - self.x1 + self.a * self.y1;
        self.x1 = x;
        self.y1 = y;
        y
    }
}

/// 采集电平计量：回调里累加，泵线程/HTTP 侧读。
#[derive(Default)]
pub struct Meter {
    pub clipped: std::sync::atomic::AtomicU64,
    pub total: std::sync::atomic::AtomicU64,
    /// 峰值（f32 位模式）
    pub peak_bits: std::sync::atomic::AtomicU32,
    /// 当前使用的声道
    pub ch_used: std::sync::atomic::AtomicUsize,
    /// 自动切换次数
    pub switches: std::sync::atomic::AtomicU64,
}

impl Meter {
    pub fn clip_pct(&self) -> f64 {
        let t = self.total.load(std::sync::atomic::Ordering::Relaxed);
        if t == 0 {
            return 0.0;
        }
        let c = self.clipped.load(std::sync::atomic::Ordering::Relaxed);
        c as f64 * 100.0 / t as f64
    }
    pub fn peak(&self) -> f32 {
        f32::from_bits(self.peak_bits.load(std::sync::atomic::Ordering::Relaxed))
    }
}

/// 回调里对每个块做统一处理：下混 → 去直流 → 统计 → 送出
fn process_block(
    data: &[f32],
    dm: &mut Downmix,
    dc: &mut DcBlock,
    meter: &Meter,
) -> Vec<f32> {
    use std::sync::atomic::Ordering;
    let mut mono = Vec::with_capacity(data.len() / dm.ch.max(1) + 1);
    let mut frames: Vec<f32> = Vec::with_capacity(2);
    let ch = dm.ch.max(1);
    for frame in data.chunks(ch) {
        frames.clear();
        frames.extend_from_slice(frame);
        dm.push(&frames, &mut mono);
    }
    let mut peak = meter.peak();
    let mut clipped = 0u64;
    for v in mono.iter_mut() {
        *v = dc.process(*v);
        let a = v.abs();
        if a > peak {
            peak = a;
        }
        if a > 0.999 {
            clipped += 1;
        }
    }
    meter.peak_bits.store(peak.to_bits(), Ordering::Relaxed);
    meter.clipped.fetch_add(clipped, Ordering::Relaxed);
    meter.total.fetch_add(mono.len() as u64, Ordering::Relaxed);
    meter.ch_used.store(dm.chosen(), Ordering::Relaxed);
    if dm.switched {
        dm.switched = false;
        meter.switches.fetch_add(1, Ordering::Relaxed);
    }
    mono
}

pub fn open(cfg: &SourceConfig) -> Result<Capture, String> {
    let host = cpal::default_host();
    let device = match &cfg.device {
        Some(name) => host
            .input_devices()
            .map_err(|e| format!("枚举输入设备失败: {e}"))?
            .find(|d| d.name().map(|n| n == *name).unwrap_or(false))
            .ok_or_else(|| format!("找不到输入设备 {name}"))?,
        None => host.default_input_device().ok_or("没有可用的默认输入设备")?,
    };
    let name = device.name().unwrap_or_else(|_| "?".into());

    let supported = device
        .default_input_config()
        .map_err(|e| format!("读设备配置失败: {e}"))?;
    let fs = supported.sample_rate().0;
    let channels = supported.channels();
    let cfg_built: cpal::StreamConfig = supported.config();

    let (tx, rx): (Sender<Vec<f32>>, Receiver<Vec<f32>>) = channel();
    let ch = channels as usize;
    let err_fn = |e| eprintln!("采集错误: {e}");

    // 每个回调闭包各持一份（cpal 要求 'static，且三种样本格式互斥）
    let meter = std::sync::Arc::new(Meter::default());
    let mk = || {
        (
            Downmix::new(ch, cfg.channel, fs),
            DcBlock::new(fs),
            meter.clone(),
        )
    };
    let (mut dm, mut dc, meter_f32) = mk();
    let (mut dm16, mut dc16, meter_i16) = mk();
    let (mut dm_u16, mut dc_u16, meter_u16) = mk();

    // 设备可能给 i16/u16/f32，三种都接
    let stream = match supported.sample_format() {
        cpal::SampleFormat::F32 => device.build_input_stream(
            &cfg_built,
            move |data: &[f32], _: &_| {
                let mono = process_block(data, &mut dm, &mut dc, &meter_f32);
                let _ = tx.send(mono);
            },
            err_fn,
            None,
        ),
        cpal::SampleFormat::I16 => device.build_input_stream(
            &cfg_built,
            move |data: &[i16], _: &_| {
                let f: Vec<f32> = data.iter().map(|v| *v as f32 / 32768.0).collect();
                let mono = process_block(&f, &mut dm16, &mut dc16, &meter_i16);
                let _ = tx.send(mono);
            },
            err_fn,
            None,
        ),
        cpal::SampleFormat::U16 => device.build_input_stream(
            &cfg_built,
            move |data: &[u16], _: &_| {
                let f: Vec<f32> = data.iter().map(|v| (*v as i32 - 32768) as f32 / 32768.0).collect();
                let mono = process_block(&f, &mut dm_u16, &mut dc_u16, &meter_u16);
                let _ = tx.send(mono);
            },
            err_fn,
            None,
        ),
        other => return Err(format!("不支持的样本格式 {other:?}")),
    }
    .map_err(|e| format!("打开输入流失败: {e}"))?;

    stream.play().map_err(|e| format!("启动采集失败: {e}"))?;
    let ch_used = match cfg.channel {
        ChannelMode::Index(i) => i.min(channels.saturating_sub(1) as usize),
        _ => 0,
    };
    Ok(Capture { _stream: stream, rx, fs, channels, name, meter, ch_used })
}

// ---------------------------------------------------------------- 发射播放

/// 8 kHz 单声道 → 设备采样率/声道数（线性插值 + 6 点平滑，把镜像压下去）。
fn resample_for_output(x: &[f32], fs_in: u32, fs_out: u32, channels: usize) -> Vec<f32> {
    let ratio = fs_out as f64 / fs_in as f64;
    let n_out = ((x.len() as f64) * ratio).ceil() as usize + 8;
    let mut mono = Vec::with_capacity(n_out);
    for i in 0..n_out {
        let pos = i as f64 / ratio;
        let i0 = pos.floor() as usize;
        let frac = pos - i0 as f64;
        let a = *x.get(i0).unwrap_or(&0.0);
        let b = *x.get(i0 + 1).unwrap_or(&a);
        mono.push(a + (b - a) * frac as f32);
    }
    // 简易滑动平均（长度 ≈ 上采样倍数），压掉插值镜像
    let k = (ratio.round() as usize).max(1).min(8);
    if k > 1 {
        let mut acc = 0.0f32;
        let mut buf = vec![0.0f32; mono.len()];
        for i in 0..mono.len() {
            acc += mono[i];
            if i >= k {
                acc -= mono[i - k];
            }
            buf[i] = acc / k as f32;
        }
        mono = buf;
    }
    let mut out = Vec::with_capacity(mono.len() * channels);
    for v in mono {
        for _ in 0..channels {
            out.push(v);
        }
    }
    out
}

/// 播放期间的取样器：把整段音频按缓冲区需要逐块吐出去。
/// `cancel` 一置位就立刻吐静音并把游标推到末尾——这样阻塞中的 `play()` 会在
/// 一个缓冲区（十几毫秒）内返回，用来支持"发射中途打断"。
#[derive(Clone)]
struct Feeder {
    data: Arc<Vec<f32>>,
    pos: Arc<AtomicUsize>,
    cancel: Arc<std::sync::atomic::AtomicBool>,
}

impl Feeder {
    fn fill(&self, buf: &mut [f32]) {
        if self.cancel.load(Ordering::Relaxed) {
            for slot in buf.iter_mut() {
                *slot = 0.0;
            }
            self.pos.store(self.data.len(), Ordering::Relaxed);
            return;
        }
        let at = self.pos.load(Ordering::Relaxed);
        for (i, slot) in buf.iter_mut().enumerate() {
            let idx = at + i;
            *slot = if idx < self.data.len() { self.data[idx] } else { 0.0 };
        }
        self.pos.fetch_add(buf.len(), Ordering::Relaxed);
    }
}

/// 阻塞播放一段 8 kHz 单声道音频；返回实际使用的输出设备名。
/// `cancel` 置位即中断（播放线程会在一个缓冲区内退出）。
pub fn play(
    x: &[f32],
    fs_in: u32,
    device: Option<&str>,
    cancel: Arc<std::sync::atomic::AtomicBool>,
) -> Result<String, String> {
    let host = cpal::default_host();
    let dev = match device {
        Some(name) if !name.is_empty() => host
            .output_devices()
            .map_err(|e| format!("枚举输出设备失败: {e}"))?
            .find(|d| d.name().map(|n| n == name).unwrap_or(false))
            .ok_or_else(|| format!("找不到输出设备 {name}"))?,
        _ => host.default_output_device().ok_or("没有可用的默认输出设备")?,
    };
    let name = dev.name().unwrap_or_else(|_| "?".into());
    let supported = dev
        .default_output_config()
        .map_err(|e| format!("读输出配置失败: {e}"))?;
    let cfg = supported.config();
    let fs_out = supported.sample_rate().0;
    let channels = cfg.channels as usize;
    let feeder = Feeder {
        data: Arc::new(resample_for_output(x, fs_in, fs_out, channels)),
        pos: Arc::new(AtomicUsize::new(0)),
        cancel,
    };
    let total = feeder.data.len();
    let err_fn = |e| eprintln!("播放错误: {e}");

    let f32_feeder = feeder.clone();
    let i16_feeder = feeder.clone();
    let stream = match supported.sample_format() {
        cpal::SampleFormat::F32 => dev.build_output_stream(
            &cfg,
            move |buf: &mut [f32], _: &_| f32_feeder.fill(buf),
            err_fn,
            None,
        ),
        cpal::SampleFormat::I16 => {
            let mut scratch: Vec<f32> = vec![0.0; 4096];
            dev.build_output_stream(
                &cfg,
                move |buf: &mut [i16], _: &_| {
                    let n = buf.len();
                    if scratch.len() < n {
                        scratch.resize(n, 0.0);
                    }
                    i16_feeder.fill(&mut scratch[..n]);
                    for i in 0..n {
                        buf[i] = (scratch[i].clamp(-1.0, 1.0) * 32767.0) as i16;
                    }
                },
                err_fn,
                None,
            )
        }
        other => return Err(format!("不支持的输出样本格式 {other:?}")),
    }
    .map_err(|e| format!("打开发射流失败: {e}"))?;

    stream.play().map_err(|e| format!("启动发射失败: {e}"))?;
    // 等播完（多等 120 ms，让设备缓冲排空）
    while feeder.pos.load(Ordering::Relaxed) < total {
        std::thread::sleep(std::time::Duration::from_millis(15));
    }
    std::thread::sleep(std::time::Duration::from_millis(120));
    drop(stream);
    Ok(name)
}


#[cfg(test)]
mod audio_proc_tests {
    use super::*;

    /// 去直流：喂常数 1.0，0.5 秒内必须收干净（否则白占动态范围）
    #[test]
    fn dc_block_kills_offset() {
        let fs = 48000;
        let mut dc = DcBlock::new(fs);
        let mut last = 0.0;
        for _ in 0..(fs / 2) {
            last = dc.process(1.0);
        }
        assert!(last.abs() < 1e-3, "残留下 DC {last}");
    }

    /// 去直流不能伤到 450 Hz 导频（幅度损失 < 1%）
    #[test]
    fn dc_block_keeps_pilot() {
        let fs = 48000u32;
        let mut dc = DcBlock::new(fs);
        let mut peak = 0.0f32;
        for i in 0..(fs as usize) {
            let t = i as f32 / fs as f32;
            let y = dc.process((2.0 * std::f32::consts::PI * 450.0 * t).sin());
            if i > fs as usize / 2 {
                peak = peak.max(y.abs());
            }
        }
        assert!(peak > 0.98, "450 Hz 被削到 {peak}");
    }

    /// 自动选声道：右路明显更响时必须选右路（避免双声道平均的梳状滤波）
    #[test]
    fn auto_picks_louder_channel() {
        let fs = 48000;
        let mut dm = Downmix::new(2, ChannelMode::Auto, fs);
        let mut out = Vec::new();
        // 先喂 1 秒：左 0.1、右 0.4
        for i in 0..fs {
            let t = i as f32 / fs as f32;
            let l = 0.1 * (2.0 * std::f32::consts::PI * 450.0 * t).sin();
            let r = 0.4 * (2.0 * std::f32::consts::PI * 450.0 * t).sin();
            out.clear();
            dm.push(&[l, r], &mut out);
        }
        assert_eq!(dm.chosen(), 1, "应当选中更响的右声道");
    }

    /// 混音模式仍可用（对照实验用）
    #[test]
    fn mix_mode_averages() {
        let mut dm = Downmix::new(2, ChannelMode::Mix, 48000);
        let mut out = Vec::new();
        dm.push(&[0.2, 0.4], &mut out);
        assert!((out[0] - 0.3).abs() < 1e-6);
    }

    /// 单声道设备直通
    #[test]
    fn mono_passthrough() {
        let mut dm = Downmix::new(1, ChannelMode::Auto, 48000);
        let mut out = Vec::new();
        dm.push(&[0.25], &mut out);
        assert!((out[0] - 0.25).abs() < 1e-6);
    }
}
