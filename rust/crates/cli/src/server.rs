//! 内置 HTTP 服务 + 界面后端。
//!
//! 架构决定（2026-11 改）：
//!   界面不再是 GDI 自绘窗口，而是**本地服务 + 浏览器应用窗口**。
//!   理由很实际：GDI 自绘的观感被用户连续否掉两次，而"能做出好看界面"的
//!   技术栈（HTML/CSS/Canvas）本来就在每台 Windows 上；反过来说，
//!   Tauri 那条路要 Node + 400 个 crate + 链接器折腾，收益却是同一个 Web 界面。
//!   于是发行物 = 一个目录、两个 exe：引擎（本服务）+ 双击启动器。
//!
//! 路由：
//!   GET  /                     内置页面（唯一前端资源，无外部依赖）
//!   GET  /api/ping             存活探测（启动器靠它认出"这是我们自己的引擎"）
//!   GET  /api/info             协议常量 / 档位表 / 字库信息
//!   GET  /api/devices          输入设备列表（真实电台的 USB 声卡也在里面）
//!   POST /api/audio            ?action=start|stop&device=名&loopback=1
//!   GET  /api/status           当前会话状态（设备/采样率/回调数/电平）
//!   GET  /api/events           SSE：逐行推送解码行（实时与回放共用一条流）
//!   POST /api/decode           请求体是 WAV 字节，按实时节奏回放进同一个解码器
//!   GET  /api/encode           ?text=&mode=&call=&grid=&sync=1 返回 WAV 字节
//!   GET  /api/glyphs           ?text= 返回点阵（发信前检查有没有缺字）
//!   GET  /api/settings         记住的偏好（设备 / 档位 / 呼号 / 网格）
//!   POST /api/settings         改偏好并落盘
//!   POST /api/quit             退出引擎（界面里的「退出」按钮）

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

/// 声道策略：带 audio 特性时直接用音频 crate 的枚举，否则用一个占位枚举，
/// 这样 `--no-default-features` 的构建也能编过。
#[cfg(feature = "audio")]
use cp16plus_audio::ChannelMode as ChMode;
#[cfg(not(feature = "audio"))]
#[derive(Clone, Copy)]
enum ChMode {
    Auto,
    Mix,
    Index(usize),
}

use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::fft;
use cp16plus_core::params::{self, FS};
use cp16plus_core::rt::Stream;
use cp16plus_core::{rx, vision, wav};

/// 字库编进 exe：换机器、换目录都不用带文件。
pub const FONT_BYTES: &[u8] = include_bytes!("../../../../fonts/hzk16.cpf");
const UI: &str = include_str!("ui.html");

// ---------------------------------------------------------------- 共享状态

#[derive(Clone)]
struct Status {
    kind: &'static str, // idle | live | replay
    device: String,
    fs: u32,
    channels: u16,
    blocks: u64,
    level_db: f64,
    cells: usize,
    t: f64,
    hdr: String,
    msg: String,
    n: usize,
    rpc: usize,
    mode: String,
}

impl Default for Status {
    fn default() -> Self {
        Status {
            kind: "idle",
            device: String::new(),
            fs: 0,
            channels: 0,
            blocks: 0,
            level_db: -120.0,
            cells: 0,
            t: 0.0,
            hdr: String::new(),
            msg: String::new(),
            n: 16,
            rpc: 17,
            mode: params::DEFAULT_MODE.to_string(),
        }
    }
}

#[derive(Clone)]
struct Event {
    id: u64,
    t: f64,
    cells: usize,
    hdr: String,
    /// 新解出的行（每行 = n 个音的相关功率）。滚动瀑布就靠它画。
    rows: Vec<Vec<f32>>,
    level_db: f64,
    blocks: u64,
    /// tick = 普通增量；summary = 会话结束汇总；error = 出错
    kind: &'static str,
    /// 预构建的 JSON 片段（summary / error 用）
    extra: String,
}

/// 解调参数快照（诊断用）：导频标定、档位、行定时
#[derive(Clone)]
struct Probe {
    a: f64,
    b: f64,
    conf: f64,
    mode: String,
    t_row: f64,
    /// 解调时实际用的 16 个音频率（已验证过 a/b 是否合理）
    tones: Vec<f64>,
}

impl Default for Probe {
    fn default() -> Self {
        Probe { a: 1.0, b: 0.0, conf: 0.0, mode: String::new(), t_row: 0.0, tones: Vec::new() }
    }
}

/// 机读层当前"草稿"：最近一次解出的报文头 + 同步帧状态。
/// 只是给界面显示和"保存通联日志"预填用，**不落库**。
#[derive(Clone, Default)]
struct LinkInfo {
    /// 每解出一个新报文头自增；界面据它判断"是不是新的一轮"
    seq: u64,
    call: String,
    grid: String,
    len: u16,
    typ: u8,
    mode: String,
    /// 报文头对应的音频时刻（秒）
    t: f64,
    /// 同步帧累计数
    frames: usize,
    /// SLOT 与本地 UTC 的差值（毫秒，10 s 步进）
    offset_ms: f64,
    /// 最近一帧同步帧的音频时刻
    sync_t: f64,
    /// 已消费过的报文头计数（清空草稿后只有"更新的一帧"才会重新填）
    seen_count: u32,
    /// 最近一次收到同步帧的墙钟（毫秒），用于判断同步是否过期
    sync_wall: u64,
}

fn link_slot(l: &LinkInfo) -> String {
    if l.frames == 0 {
        return "none".to_string();
    }
    // 25 秒没再收到同步帧就降级（周期是 10 秒，连丢两帧基本就是断了）
    if l.sync_wall > 0 && now_ms().saturating_sub(l.sync_wall) > 25_000 {
        return "warn".to_string();
    }
    "ok".to_string()
}

struct Shared {
    events: Mutex<Vec<Event>>,
    next_id: AtomicU64,
    capturing: AtomicBool,
    stop: AtomicBool,
    /// 机读层草稿（不落库）
    link: Mutex<LinkInfo>,
    /// 最近解出的行数据（诊断用，环形缓冲 64 行）
    rows_dbg: Mutex<Vec<Vec<f64>>>,
    /// 解调参数快照（诊断用）
    probe: Mutex<Probe>,
    /// 平均频谱（0–4 kHz，512 bin，dB），诊断用
    spec: Mutex<Vec<f32>>,
    /// 采集电平：削顶百分比 / 峰值 / 当前声道（采集线程每 0.5 s 抄进来）
    clip_pct: Mutex<f64>,
    peak: Mutex<f32>,
    ch_used: AtomicUsize,
    /// 会话代号：新会话自增，旧线程看到代号变了就退出
    generation: AtomicU64,
    status: Mutex<Status>,
}

const MAX_EVENTS: usize = 900;

// ---------------------------------------------------------------- 偏好持久化

/// 记住的东西：本台呼号/网格、输入设备、发射时序补偿、保存目录、默认消息、调试开关。
/// 存在 `%LOCALAPPDATA%\CP16Plus\settings.json`；不可写时退到 exe 旁边（绿色部署）。
#[derive(Clone)]
struct Settings {
    device: String,
    /// 发射用输出设备（电台常接在一块特定 USB 声卡上）
    out_device: String,
    loopback: bool,
    mode: String,
    call: String,
    grid: String,
    /// 生成物（WAV / 瀑布图 / ADIF / 日志）落到哪个目录
    out_dir: String,
    /// "保存瀑布图"默认截取多长（秒）
    waterfall_secs: u32,
    /// 发射时序补偿（毫秒，可负）：射频链路 + 声卡的总延迟
    tx_offset_ms: i32,
    /// 当前工作频率（kHz）。只用于通联日志/ADIF，不参与调制解调。
    freq_khz: u32,
    /// 存档区显示几个 10 秒块（1–4，界面上限由版面决定）
    archive_blocks: u32,
    /// 电台：开启 CAT
    rig_enabled: bool,
    /// hamlib 型号号
    rig_model: u32,
    rig_port: String,
    rig_baud: u32,
    /// 发射前提前拉 PTT / 播完延时松开（毫秒）
    rig_lead_ms: u32,
    rig_tail_ms: u32,
    /// 空闲时读频率的间隔（毫秒）
    rig_poll_ms: u32,
    /// 调试模式：界面才显示生成 WAV / 自检 / 同步帧开关 / 详细参数
    debug: bool,
    /// 默认消息（标签, 正文）
    messages: Vec<(String, String)>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            device: String::new(),
            out_device: String::new(),
            loopback: false,
            mode: params::DEFAULT_MODE.to_string(),
            call: "BG1ABC".into(),
            grid: "OM89AB".into(),
            out_dir: String::new(), // 空 = 用默认（文档目录\CP16Plus）
            waterfall_secs: 30,
            tx_offset_ms: 0,
            freq_khz: 14070,
            archive_blocks: 3,
            rig_enabled: false,
            rig_model: 1035,      // Yaesu FT-991：常见默认，用户可改
            rig_port: String::new(),
            rig_baud: 38400,
            rig_lead_ms: 150,
            rig_tail_ms: 150,
            rig_poll_ms: 1000,
            debug: false,
            messages: Vec::new(),
        }
    }
}

/// 默认保存目录：`%USERPROFILE%\Documents\CP16Plus`
fn default_out_dir() -> String {
    if let Ok(up) = std::env::var("USERPROFILE") {
        return PathBuf::from(up)
            .join("Documents")
            .join("CP16Plus")
            .display()
            .to_string();
    }
    ".".into()
}

impl Settings {
    /// 用户没设过就用默认目录
    fn out_dir(&self) -> String {
        if self.out_dir.trim().is_empty() {
            default_out_dir()
        } else {
            self.out_dir.clone()
        }
    }

    /// 按呼号/网格生成几条默认消息（改完呼号就能一键发 CQ）
    fn default_messages(&self) -> Vec<(String, String)> {
        defaults_for(&self.call, &self.grid)
    }
}

fn settings_candidates() -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Ok(base) = std::env::var("LOCALAPPDATA") {
        v.push(PathBuf::from(base).join("CP16Plus").join("settings.json"));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            v.push(dir.join("settings.json"));
        }
    }
    v
}

/// 极简字段读取：文件是我们自己写的扁平 JSON，不值得为此引入 serde。
fn json_field_str(s: &str, key: &str) -> Option<String> {
    let pat = format!("\"{key}\"");
    let at = s.find(&pat)?;
    let rest = s[at + pat.len()..].trim_start();
    let rest = rest.strip_prefix(':')?.trim_start();
    let rest = rest.strip_prefix('"')?;
    // 必须按转义规则扫描：否则 "D:\\Tools" 会被逐次加倍（历史上把 out_dir 越写越长）
    let mut out = String::new();
    let mut it = rest.chars();
    while let Some(c) = it.next() {
        match c {
            '\\' => match it.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('r') => out.push('\r'),
                Some('"') => out.push('"'),
                Some('\\') => out.push('\\'),
                Some('/') => out.push('/'),
                Some(other) => { out.push('\\'); out.push(other); }
                None => break,
            },
            '"' => return Some(out),
            c => out.push(c),
        }
    }
    None
}

/// Windows 路径归一：把连续反斜杠压成一个（UNC 前缀 `\\server\share` 的前两个保留）
fn clean_path(p: &str) -> String {
    let p = p.trim();
    if p.is_empty() {
        return String::new();
    }
    let unc = p.starts_with("\\\\");
    let mut out = String::with_capacity(p.len());
    let mut prev_bs = false;
    for (i, c) in p.chars().enumerate() {
        if c == '\\' {
            if prev_bs && !(unc && i < 2) {
                continue;
            }
            prev_bs = true;
        } else {
            prev_bs = false;
        }
        out.push(c);
    }
    out
}

fn json_field_bool(s: &str, key: &str) -> Option<bool> {
    let pat = format!("\"{key}\"");
    let at = s.find(&pat)?;
    let rest = s[at + pat.len()..].trim_start();
    let rest = rest.strip_prefix(':')?.trim_start();
    if rest.starts_with("true") {
        Some(true)
    } else if rest.starts_with("false") {
        Some(false)
    } else {
        None
    }
}

fn json_field_i64(s: &str, key: &str) -> Option<i64> {
    let pat = format!("\"{key}\"");
    let at = s.find(&pat)?;
    let rest = s[at + pat.len()..].trim_start();
    let rest = rest.strip_prefix(':')?.trim_start();
    let end = rest.find(|c: char| !(c.is_ascii_digit() || c == '-' || c == '+')).unwrap_or(rest.len());
    rest[..end].parse().ok()
}

/// 取 `"key":[...]` 里每个 `{...}` 花括号块（够我们读 messages / qso 用）。
fn json_objects(s: &str, key: &str) -> Vec<String> {
    let pat = format!("\"{key}\"");
    let Some(at) = s.find(&pat) else { return Vec::new() };
    let rest = &s[at + pat.len()..];
    let Some(open) = rest.find('[') else { return Vec::new() };
    let body = &rest[open + 1..];
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut start: Option<usize> = None;
    for (i, c) in body.char_indices() {
        match c {
            '{' => {
                if depth == 0 {
                    start = Some(i);
                }
                depth += 1;
            }
            '}' => {
                depth -= 1;
                if depth == 0 {
                    if let Some(st) = start {
                        out.push(body[st..=i].to_string());
                    }
                    start = None;
                }
            }
            ']' if depth == 0 => break,
            _ => {}
        }
    }
    out
}

fn settings_load() -> Settings {
    let mut set = Settings::default();
    for path in settings_candidates() {
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        if let Some(v) = json_field_str(&text, "device") {
            set.device = v;
        }
        if let Some(v) = json_field_str(&text, "out_device") {
            set.out_device = v;
        }
        if let Some(v) = json_field_bool(&text, "loopback") {
            set.loopback = v;
        }
        if let Some(v) = json_field_str(&text, "mode") {
            if params::by_name(&v).is_some() {
                set.mode = v;
            }
        }
        if let Some(v) = json_field_str(&text, "call") {
            if !v.is_empty() {
                set.call = v;
            }
        }
        if let Some(v) = json_field_str(&text, "grid") {
            if !v.is_empty() {
                set.grid = v;
            }
        }
        if let Some(v) = json_field_str(&text, "out_dir") {
            set.out_dir = clean_path(&v);
        }
        if let Some(v) = json_field_i64(&text, "waterfall_secs") {
            if v >= 5 && v <= 3600 {
                set.waterfall_secs = v as u32;
            }
        }
        if let Some(v) = json_field_i64(&text, "tx_offset_ms") {
            set.tx_offset_ms = v.clamp(-5000, 5000) as i32;
        }
        if let Some(v) = json_field_i64(&text, "archive_blocks") {
            if v >= 1 && v <= 4 {
                set.archive_blocks = v as u32;
            }
        }
        if let Some(v) = json_field_i64(&text, "freq_khz") {
            if v > 0 {
                set.freq_khz = v as u32;
            }
        }
        if let Some(v) = json_field_bool(&text, "rig_enabled") {
            set.rig_enabled = v;
        }
        if let Some(v) = json_field_i64(&text, "rig_model") {
            if v > 0 {
                set.rig_model = v as u32;
            }
        }
        if let Some(v) = json_field_str(&text, "rig_port") {
            set.rig_port = v;
        }
        if let Some(v) = json_field_i64(&text, "rig_baud") {
            if v > 0 {
                set.rig_baud = v as u32;
            }
        }
        if let Some(v) = json_field_i64(&text, "rig_lead_ms") {
            set.rig_lead_ms = v.clamp(0, 2000) as u32;
        }
        if let Some(v) = json_field_i64(&text, "rig_tail_ms") {
            set.rig_tail_ms = v.clamp(0, 2000) as u32;
        }
        if let Some(v) = json_field_i64(&text, "rig_poll_ms") {
            set.rig_poll_ms = v.clamp(0, 10000) as u32;
        }
        if let Some(v) = json_field_bool(&text, "debug") {
            set.debug = v;
        }
        for obj in json_objects(&text, "messages") {
            let label = json_field_str(&obj, "label").unwrap_or_default();
            let text_of = json_field_str(&obj, "text").unwrap_or_default();
            if !label.is_empty() || !text_of.is_empty() {
                set.messages.push((label, text_of));
            }
        }
        break;
    }
    if set.messages.is_empty() {
        set.messages = set.default_messages();
    }
    set
}

/// 引擎自己的数据目录（settings.json / qso.json）。
/// 判据必须是"真的能写进去"——只 `create_dir_all` 成功不代表能写文件
/// （受限目录、漫游配置、沙箱里都会出现"目录建得出来、文件写不进去"）。
fn settings_dir() -> PathBuf {
    static D: OnceLock<PathBuf> = OnceLock::new();
    D.get_or_init(|| {
        for path in settings_candidates() {
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
                let probe = dir.join(".cp16-write-probe");
                if std::fs::write(&probe, b"1").is_ok() {
                    let _ = std::fs::remove_file(&probe);
                    return dir.to_path_buf();
                }
            }
        }
        std::env::temp_dir()
    })
    .clone()
}

fn settings_store(set: &Settings) {
    let msgs: Vec<String> = set
        .messages
        .iter()
        .map(|(l, t)| format!("{{\"label\":\"{}\",\"text\":\"{}\"}}", esc(l), esc(t)))
        .collect();
    let body = format!(
        "{{\"device\":\"{}\",\"out_device\":\"{}\",\"loopback\":{},\"mode\":\"{}\",\"call\":\"{}\",\"grid\":\"{}\",\
          \"out_dir\":\"{}\",\"waterfall_secs\":{},\"tx_offset_ms\":{},\"freq_khz\":{},\"archive_blocks\":{},\"rig_enabled\":{},\"rig_model\":{},\
          \"rig_port\":\"{}\",\"rig_baud\":{},\"rig_lead_ms\":{},\"rig_tail_ms\":{},\"rig_poll_ms\":{},\
          \"debug\":{},\"messages\":[{}]}}\n",
        esc(&set.device),
        esc(&set.out_device),
        set.loopback,
        esc(&set.mode),
        esc(&set.call),
        esc(&set.grid),
        esc(&set.out_dir),
        set.waterfall_secs,
        set.tx_offset_ms,
        set.freq_khz,
        set.archive_blocks,
        set.rig_enabled,
        set.rig_model,
        esc(&set.rig_port),
        set.rig_baud,
        set.rig_lead_ms,
        set.rig_tail_ms,
        set.rig_poll_ms,
        set.debug,
        msgs.join(",")
    );
    for path in settings_candidates() {
        if let Some(dir) = path.parent() {
            if std::fs::create_dir_all(dir).is_err() {
                continue;
            }
        }
        if std::fs::write(&path, &body).is_ok() {
            return;
        }
    }
}

fn settings() -> &'static Mutex<Settings> {
    static S: OnceLock<Mutex<Settings>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(settings_load()))
}

fn settings_json(s: &Settings) -> String {
    let msgs: Vec<String> = s
        .messages
        .iter()
        .map(|(l, t)| format!("{{\"label\":\"{}\",\"text\":\"{}\"}}", esc(l), esc(t)))
        .collect();
    format!(
        "{{\"device\":\"{}\",\"out_device\":\"{}\",\"loopback\":{},\"mode\":\"{}\",\"call\":\"{}\",\"grid\":\"{}\",\
          \"out_dir\":\"{}\",\"out_dir_effective\":\"{}\",\"waterfall_secs\":{},\
          \"tx_offset_ms\":{},\"freq_khz\":{},\"archive_blocks\":{},\"rig_enabled\":{},\"rig_model\":{},\
          \"rig_port\":\"{}\",\"rig_baud\":{},\"rig_lead_ms\":{},\"rig_tail_ms\":{},\"rig_poll_ms\":{},\
          \"debug\":{},\"messages\":[{}]}}",
        esc(&s.device),
        esc(&s.out_device),
        s.loopback,
        esc(&s.mode),
        esc(&s.call),
        esc(&s.grid),
        esc(&s.out_dir),
        esc(&s.out_dir()),
        s.waterfall_secs,
        s.tx_offset_ms,
        s.freq_khz,
        s.archive_blocks,
        s.rig_enabled,
        s.rig_model,
        esc(&s.rig_port),
        s.rig_baud,
        s.rig_lead_ms,
        s.rig_tail_ms,
        s.rig_poll_ms,
        s.debug,
        msgs.join(",")
    )
}

/// 消息列表在 URL 里的编码：`标签~正文;标签~正文`（为了省掉 JSON 依赖，
/// 正文里的 `;` 和 `~` 会被替换成空格——CQ 报文里本来也不会用到）。
fn parse_messages(raw: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for item in raw.split(';') {
        if item.trim().is_empty() {
            continue;
        }
        let mut it = item.splitn(2, '~');
        let label = it.next().unwrap_or("").trim().to_string();
        let text = it.next().unwrap_or("").trim().to_string();
        if label.is_empty() && text.is_empty() {
            continue;
        }
        out.push((label, text));
        if out.len() >= 12 {
            break;
        }
    }
    out
}

/// 只更新传进来的字段（None / 空串 = 这一项不动）。
/// 默认消息模板：改完呼号/网格，这几条跟着变（用户自己加/改过的不动）
fn defaults_for(call: &str, grid: &str) -> Vec<(String, String)> {
    let call = if call.is_empty() { "N0CALL" } else { call };
    let grid = if grid.is_empty() { "----" } else { grid };
    vec![
        ("CQ".to_string(), format!("CQ CQ CQ DE {call} {call} PSE K")),
        ("呼号".to_string(), format!("DE {call}")),
        ("网格".to_string(), format!("QTH {grid}")),
        ("TEST".to_string(), format!("TEST DE {call}")),
        ("73".to_string(), format!("73 DE {call} SK")),
    ]
}

/// 只更新传进来的字段（None / 空串 = 这一项不动）。
#[derive(Default)]
struct Patch {
    device: Option<String>,
    out_device: Option<String>,
    loopback: Option<bool>,
    mode: Option<String>,
    call: Option<String>,
    grid: Option<String>,
    out_dir: Option<String>,
    waterfall_secs: Option<u32>,
    tx_offset_ms: Option<i32>,
    freq_khz: Option<u32>,
    archive_blocks: Option<u32>,
    rig_enabled: Option<bool>,
    rig_model: Option<u32>,
    rig_port: Option<String>,
    rig_baud: Option<u32>,
    rig_lead_ms: Option<u32>,
    rig_tail_ms: Option<u32>,
    rig_poll_ms: Option<u32>,
    debug: Option<bool>,
    messages: Option<Vec<(String, String)>>,
    /// 按呼号/网格重建默认消息
    regen_messages: bool,
}

impl Patch {
    fn none() -> Self {
        Patch::default()
    }
}

/// 从查询串里读出想要的字段（空串视为"没传"）。
fn patch_from_query(q: &str) -> Patch {
    let mut p = Patch::default();
    let dev = query_param(q, "device");
    if !dev.is_empty() {
        p.device = Some(dev);
    }
    let out_dev = query_param(q, "out_device");
    if !out_dev.is_empty() {
        p.out_device = Some(out_dev);
    }
    let mode = query_param(q, "mode");
    if !mode.is_empty() {
        p.mode = Some(mode);
    }
    let call = query_param(q, "call");
    if !call.is_empty() {
        p.call = Some(call.trim().to_string());
    }
    let grid = query_param(q, "grid");
    if !grid.is_empty() {
        p.grid = Some(grid.trim().to_string());
    }
    let out_dir = query_param(q, "out_dir");
    if !out_dir.is_empty() {
        p.out_dir = Some(out_dir);
    }
    let secs = query_param(q, "waterfall_secs");
    if !secs.is_empty() {
        p.waterfall_secs = secs.parse().ok();
    }
    let off = query_param(q, "tx_offset_ms");
    if !off.is_empty() {
        p.tx_offset_ms = off.parse().ok();
    }
    let fk = query_param(q, "freq_khz");
    if !fk.is_empty() {
        p.freq_khz = fk.parse().ok();
    }
    let ab = query_param(q, "archive_blocks");
    if !ab.is_empty() {
        p.archive_blocks = ab.parse().ok();
    }
    if !query_param(q, "rig_model").is_empty() {
        p.rig_model = query_param(q, "rig_model").parse().ok();
    }
    if !query_param(q, "rig_port").is_empty() {
        p.rig_port = Some(query_param(q, "rig_port"));
    }
    if !query_param(q, "rig_baud").is_empty() {
        p.rig_baud = query_param(q, "rig_baud").parse().ok();
    }
    if !query_param(q, "rig_lead_ms").is_empty() {
        p.rig_lead_ms = query_param(q, "rig_lead_ms").parse().ok();
    }
    if !query_param(q, "rig_tail_ms").is_empty() {
        p.rig_tail_ms = query_param(q, "rig_tail_ms").parse().ok();
    }
    if !query_param(q, "rig_poll_ms").is_empty() {
        p.rig_poll_ms = query_param(q, "rig_poll_ms").parse().ok();
    }
    match query_param(q, "rig_enabled").as_str() {
        "1" => p.rig_enabled = Some(true),
        "0" => p.rig_enabled = Some(false),
        _ => {}
    }
    let lb = query_param(q, "loopback");
    match lb.as_str() {
        "1" => p.loopback = Some(true),
        "0" => p.loopback = Some(false),
        _ => {}
    }
    let dbg = query_param(q, "debug");
    match dbg.as_str() {
        "1" => p.debug = Some(true),
        "0" => p.debug = Some(false),
        _ => {}
    }
    let msgs = query_param(q, "messages");
    if !msgs.is_empty() {
        p.messages = Some(parse_messages(&msgs));
    }
    if query_param(q, "regen_messages") == "1" {
        p.regen_messages = true;
    }
    p
}

fn settings_update(p: Patch) -> String {
    let mut s = settings().lock().unwrap();
    if let Some(v) = p.device {
        if !v.is_empty() {
            s.device = v;
        }
    }
    if let Some(v) = p.out_device {
        if !v.is_empty() {
            s.out_device = v;
        }
    }
    if let Some(v) = p.loopback {
        s.loopback = v;
    }
    if let Some(v) = p.mode {
        if params::by_name(&v).is_some() {
            s.mode = v;
        }
    }
    let before = (s.call.clone(), s.grid.clone());
    if let Some(v) = p.call {
        s.call = v;
    }
    if let Some(v) = p.grid {
        s.grid = v;
    }
    if let Some(v) = p.out_dir {
        s.out_dir = clean_path(&v);
    }
    if let Some(v) = p.waterfall_secs {
        s.waterfall_secs = v.clamp(5, 3600);
    }
    if let Some(v) = p.tx_offset_ms {
        s.tx_offset_ms = v.clamp(-5000, 5000);
    }
    if let Some(v) = p.freq_khz {
        if v > 0 {
            s.freq_khz = v;
        }
    }
    if let Some(v) = p.archive_blocks {
        s.archive_blocks = v.clamp(1, 4);
    }
    let mut rig_touch = false;
    if let Some(v) = p.rig_enabled {
        s.rig_enabled = v;
        rig_touch = true;
    }
    if let Some(v) = p.rig_model {
        s.rig_model = v;
        rig_touch = true;
    }
    if let Some(v) = p.rig_port {
        s.rig_port = v;
        rig_touch = true;
    }
    if let Some(v) = p.rig_baud {
        s.rig_baud = v;
        rig_touch = true;
    }
    if let Some(v) = p.rig_lead_ms {
        s.rig_lead_ms = v;
    }
    if let Some(v) = p.rig_tail_ms {
        s.rig_tail_ms = v;
    }
    if let Some(v) = p.rig_poll_ms {
        s.rig_poll_ms = v;
    }
    if let Some(v) = p.debug {
        s.debug = v;
    }
    let _ = rig_touch;
    if let Some(v) = p.messages {
        if !v.is_empty() {
            s.messages = v;
        }
    }
    // 改完呼号/网格：如果那几条消息还是"上一套自动生成的"，就跟着重建；
    // 用户自己加过/改过的不动。
    let changed = before != (s.call.clone(), s.grid.clone());
    if p.regen_messages {
        s.messages = s.default_messages();
    } else if changed && s.messages == defaults_for(&before.0, &before.1) {
        s.messages = s.default_messages();
    }
    let snapshot = s.clone();
    drop(s);
    settings_store(&snapshot);
    settings_json(&snapshot)
}

// ---------------------------------------------------------------- 通联日志（ADIF）

#[derive(Clone, Default)]
struct Qso {
    call: String,
    grid: String,
    /// 对方 QTH（城市/省份），可选
    qth: String,
    /// 我方 / 对方 信号报告（RST 习惯，如 59、579）
    rst_sent: String,
    rst_rcvd: String,
    /// 本机呼号 / 本机网格（ADIF 的 STATION_CALLSIGN / MY_GRIDSQUARE）
    my_call: String,
    my_grid: String,
    /// MHz，ADIF 的 FREQ 就是这个单位
    freq: String,
    /// YYYYMMDD
    date: String,
    /// HHMM（UTC）
    time: String,
    mode: String,
    note: String,
}

fn qso_path() -> PathBuf {
    settings_dir().join("qso.json")
}

fn qso_load() -> Vec<Qso> {
    let Ok(text) = std::fs::read_to_string(qso_path()) else { return Vec::new() };
    json_objects(&text, "qso")
        .into_iter()
        .map(|o| Qso {
            call: json_field_str(&o, "call").unwrap_or_default(),
            grid: json_field_str(&o, "grid").unwrap_or_default(),
            qth: json_field_str(&o, "qth").unwrap_or_default(),
            rst_sent: json_field_str(&o, "rst_sent").unwrap_or_default(),
            rst_rcvd: json_field_str(&o, "rst_rcvd").unwrap_or_default(),
            my_call: json_field_str(&o, "my_call").unwrap_or_default(),
            my_grid: json_field_str(&o, "my_grid").unwrap_or_default(),
            freq: json_field_str(&o, "freq").unwrap_or_default(),
            date: json_field_str(&o, "date").unwrap_or_default(),
            time: json_field_str(&o, "time").unwrap_or_default(),
            mode: json_field_str(&o, "mode").unwrap_or_default(),
            note: json_field_str(&o, "note").unwrap_or_default(),
        })
        .collect()
}

fn qso_save(list: &[Qso]) {
    let items: Vec<String> = list
        .iter()
        .map(|q| {
            format!(
                "{{\"call\":\"{}\",\"grid\":\"{}\",\"qth\":\"{}\",\"rst_sent\":\"{}\",\
                  \"rst_rcvd\":\"{}\",\"my_call\":\"{}\",\"my_grid\":\"{}\",\
                  \"freq\":\"{}\",\"date\":\"{}\",\"time\":\"{}\",\
                  \"mode\":\"{}\",\"note\":\"{}\"}}",
                esc(&q.call),
                esc(&q.grid),
                esc(&q.qth),
                esc(&q.rst_sent),
                esc(&q.rst_rcvd),
                esc(&q.my_call),
                esc(&q.my_grid),
                esc(&q.freq),
                esc(&q.date),
                esc(&q.time),
                esc(&q.mode),
                esc(&q.note)
            )
        })
        .collect();
    let body = format!("{{\"qso\":[{}]}}\n", items.join(","));
    if let Err(e) = std::fs::write(qso_path(), body) {
        // 写不进去（受限目录等）：至少让控制台/日志留痕，别静默丢数据
        let _ = writeln!(std::io::stderr(), "通联日志写入失败：{e}");
    }
}

fn qso_json(list: &[Qso]) -> String {
    let items: Vec<String> = list
        .iter()
        .map(|q| {
            format!(
                "{{\"call\":\"{}\",\"grid\":\"{}\",\"qth\":\"{}\",\
                  \"rst_sent\":\"{}\",\"rst_rcvd\":\"{}\",\
                  \"my_call\":\"{}\",\"my_grid\":\"{}\",\
                  \"freq\":\"{}\",\"date\":\"{}\",\"time\":\"{}\",\
                  \"mode\":\"{}\",\"note\":\"{}\"}}",
                esc(&q.call),
                esc(&q.grid),
                esc(&q.qth),
                esc(&q.rst_sent),
                esc(&q.rst_rcvd),
                esc(&q.my_call),
                esc(&q.my_grid),
                esc(&q.freq),
                esc(&q.date),
                esc(&q.time),
                esc(&q.mode),
                esc(&q.note)
            )
        })
        .collect();
    format!("{{\"ok\":true,\"qso\":[{}]}}", items.join(","))
}

fn adif_field(name: &str, value: &str) -> String {
    format!("<{}:{}>{} ", name.to_ascii_uppercase(), value.chars().count(), value)
}

fn qso_adif(list: &[Qso]) -> String {
    let mut s = String::new();
    // ADIF 要求从头部字段开始，前面不能放自由文本（否则部分日志软件会整份拒绝）
    s.push_str(&adif_field("adif_ver", "3.1.4"));
    s.push('\n');
    s.push_str(&adif_field("programid", "CP16Plus"));
    s.push_str(&format!("<programversion:{}>", env!("CARGO_PKG_VERSION").chars().count()));
    s.push_str(env!("CARGO_PKG_VERSION"));
    s.push(' ');
    s.push_str("<EOH>\n");
    for q in list {
        // ADIF 里 CALL 是必填项：呼号还空着的行（界面里刚新增、还没填）不导出
        if q.call.trim().is_empty() {
            continue;
        }
        s.push_str(&adif_field("call", &q.call));
        if !q.grid.is_empty() {
            s.push_str(&adif_field("gridsquare", &q.grid));
        }
        if !q.freq.is_empty() {
            s.push_str(&adif_field("freq", &q.freq));
        }
        if !q.date.is_empty() {
            s.push_str(&adif_field("qso_date", &q.date));
        }
        if !q.time.is_empty() {
            s.push_str(&adif_field("time_on", &q.time));
        }
        // MODE 用自定义标识 CP16（ADIF 允许未知 mode 字符串，多数日志软件按原样收下）
        if !q.qth.is_empty() {
            s.push_str(&adif_field("qth", &q.qth));
        }
        if !q.rst_sent.is_empty() {
            s.push_str(&adif_field("rst_sent", &q.rst_sent));
        }
        if !q.rst_rcvd.is_empty() {
            s.push_str(&adif_field("rst_rcvd", &q.rst_rcvd));
        }
        if !q.my_call.is_empty() {
            s.push_str(&adif_field("station_callsign", &q.my_call));
        }
        if !q.my_grid.is_empty() {
            s.push_str(&adif_field("my_gridsquare", &q.my_grid));
        }
        s.push_str(&adif_field("mode", "CP16"));
        if !q.mode.is_empty() {
            s.push_str(&adif_field("submode", &q.mode));
        }
        if !q.note.is_empty() {
            s.push_str(&adif_field("comment", &q.note));
        }
        s.push_str("<EOR>\n");
    }
    s
}

/// UTC 日期 + 时分（ADIF 要 YYYYMMDD / HHMM）
fn utc_now_hm() -> (String, String) {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let days = secs.div_euclid(86400);
    let sod = secs.rem_euclid(86400);
    let (y, m, d) = civil_from_days(days);
    (
        format!("{y:04}{m:02}{d:02}"),
        format!("{:02}{:02}", sod / 3600, (sod % 3600) / 60),
    )
}

/// 文件名用的时间戳（UTC）：`20261104-152330`
fn utc_stamp() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let days = secs.div_euclid(86400);
    let sod = secs.rem_euclid(86400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}{m:02}{d:02}-{:02}{:02}{:02}",
        sod / 3600,
        (sod % 3600) / 60,
        sod % 60
    )
}

/// 把生成物写到"设置里那个保存目录"，返回落盘后的完整路径。
fn save_in_out_dir(name: &str, bytes: &[u8]) -> Result<String, String> {
    let dir = settings().lock().unwrap().out_dir();
    if std::fs::create_dir_all(&dir).is_err() {
        return Err(format!("建不了保存目录：{dir}"));
    }
    let safe: String = name
        .chars()
        .map(|c| if "/\\:*?\"<>|".contains(c) { '_' } else { c })
        .collect();
    let path = PathBuf::from(&dir).join(&safe);
    std::fs::write(&path, bytes).map_err(|e| format!("写文件失败 {}: {e}", path.display()))?;
    Ok(path.display().to_string())
}

/// 从 1970-01-01 起的天数 → (年, 月, 日)（Howard Hinnant 的算法）
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

// ---------------------------------------------------------------- 电台（CAT）

/// 当前连着的电台。None = 没连（或没开）。
fn rig_slot() -> &'static Mutex<Option<cp16plus_rig::Rig>> {
    static R: OnceLock<Mutex<Option<cp16plus_rig::Rig>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(None))
}

/// 最近一次电台操作的结果，给界面显示
fn rig_status() -> &'static Mutex<String> {
    static S: OnceLock<Mutex<String>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(String::new()))
}

fn rig_set_status(msg: &str) {
    *rig_status().lock().unwrap() = msg.to_string();
}

/// 按设置里的参数（重新）连接。返回给用户看的消息。
fn rig_reconnect() -> String {
    let cfg = {
        let s = settings().lock().unwrap();
        (s.rig_enabled, s.rig_model, s.rig_port.clone(), s.rig_baud)
    };
    let gen = rig_bump_gen();
    let mut slot = rig_slot().lock().unwrap();
    *slot = None; // 先关掉旧的（Drop 里会 rig_close/cleanup）
    *rig_cache().lock().unwrap() = (0.0, false, 0);
    if !cfg.0 {
        rig_set_status("未启用");
        return "电台控制未启用".into();
    }
    if !cp16plus_rig::available() {
        rig_set_status("缺少 libhamlib-4.dll");
        return "缺少 libhamlib-4.dll，电台功能不可用".into();
    }
    // 打开 + 读一次频率（探测）。波特率不对时 open 会成功、读会超时，
    // 所以这里按候选波特率挨个试一遍，试通了就把设置改成它。
    fn try_open(model: u32, port: &str, baud: u32) -> Result<(cp16plus_rig::Rig, f64), String> {
        let cfg = cp16plus_rig::RigConfig { model, port: port.to_string(), baud };
        let rig = cp16plus_rig::Rig::open(&cfg)?;
        match rig.probe() {
            Ok(hz) => Ok((rig, hz)),
            Err(e) => Err(e),
        }
    }
    let mut used_baud = cfg.3;
    let opened = match try_open(cfg.1, &cfg.2, cfg.3) {
        Ok(v) => Ok(v),
        Err(first_err) => {
            let mut ok = None;
            for b in [19200u32, 9600, 38400, 57600, 115200, 4800] {
                if b == cfg.3 {
                    continue;
                }
                if let Ok((r, hz)) = try_open(cfg.1, &cfg.2, b) {
                    used_baud = b;
                    ok = Some((r, hz, b));
                    break;
                }
            }
            match ok {
                Some((r, hz, _b)) => Ok((r, hz)),
                None => Err(first_err),
            }
        }
    };
    match opened {
        Ok((r, _hz)) => {
            // 打开成功 ≠ 通得了：立刻读一次频率验证波特率/型号
            if used_baud != cfg.3 {
                // 自动匹配到了别的波特率：写回设置，下次直接用
                let mut st = settings().lock().unwrap();
                st.rig_baud = used_baud;
                let snapshot = st.clone();
                drop(st);
                settings_store(&snapshot);
            }
            let msg = format!(
                "已连接：{} @ {} {}（{} Hz）{}",
                r.info(),
                r.path_effective,
                r.speed_effective,
                _hz as i64,
                if used_baud != cfg.3 { " · 波特率已按实测自动设为该值" } else { "" }
            );
            *slot = Some(r);
            rig_set_status(&msg);
            rig_cache().lock().unwrap().0 = _hz;
            drop(slot);
            rig_start_poll(gen);
            msg
        }
        Err(e) => {
            rig_set_status(&e);
            format!("打开电台失败：{e}")
        }
    }
}

/// 在已连接的电台对象上跑一段操作（没连就直接返回 None）
fn rig_with<T>(f: impl FnOnce(&cp16plus_rig::Rig) -> T) -> Option<T> {
    let slot = rig_slot().lock().unwrap();
    slot.as_ref().map(f)
}

fn rig_ptt(on: bool) -> bool {
    match rig_with(|r| r.set_ptt(on)) {
        Some(Ok(())) => {
            rig_cache().lock().unwrap().1 = on;
            true
        }
        Some(Err(e)) => {
            rig_set_status(&e);
            false
        }
        None => false,
    }
}

/// 后台轮询缓存：(频率 Hz, PTT, 最近更新时间 ms)
fn rig_cache() -> &'static Mutex<(f64, bool, u64)> {
    static C: OnceLock<Mutex<(f64, bool, u64)>> = OnceLock::new();
    C.get_or_init(|| Mutex::new((0.0, false, 0)))
}

/// 连上之后起一个只看不动的轮询线程：定期读频率/PTT 填缓存。
/// 一旦被禁用或重连（gen 变化）就自己退出。
fn rig_start_poll(gen: u64) {
    let poll_ms = {
        let s = settings().lock().unwrap();
        s.rig_poll_ms.max(200)
    };
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_millis(poll_ms as u64));
        if rig_gen() != gen || !settings().lock().unwrap().rig_enabled {
            return;
        }
        // 发射期间**绝不**发 CAT 查询：CI-V 台子在发射时收到读频率命令，
        // 有些型号会直接掉 PTT（协谷 X6100 实测发射 10 秒左右自动停）。
        if tx_state().lock().unwrap().state != "idle" {
            continue;
        }
        let freq = rig_with(|r| r.read_freq()).and_then(|v| v.ok()).unwrap_or(0.0);
        let mut c = rig_cache().lock().unwrap();
        if freq > 0.0 {
            c.0 = freq;
        }
        c.2 = now_ms();
    });
}

fn rig_gen() -> u64 {
    static G: OnceLock<AtomicU64> = OnceLock::new();
    if let Some(g) = G.get() {
        g.load(Ordering::SeqCst)
    } else {
        0
    }
}

fn rig_bump_gen() -> u64 {
    static G: OnceLock<AtomicU64> = OnceLock::new();
    let g = G.get_or_init(|| AtomicU64::new(1));
    g.fetch_add(1, Ordering::SeqCst) + 1
}

fn rig_json(with_lists: bool) -> String {
    let s = settings().lock().unwrap().clone();
    let connected = rig_slot().lock().unwrap().is_some();
    let (freq, ptt, _) = *rig_cache().lock().unwrap();
    let msg = rig_status().lock().unwrap().clone();
    let mut out = format!(
        "{{\"available\":{},\"connected\":{},\"enabled\":{},\"model\":{},\"port\":\"{}\",\
          \"baud\":{},\"freq_hz\":{},\"ptt\":{},\"msg\":\"{}\",\"lead_ms\":{},\"tail_ms\":{},\"poll_ms\":{}}}",
        cp16plus_rig::available(),
        connected,
        s.rig_enabled,
        s.rig_model,
        esc(&s.rig_port),
        s.rig_baud,
        num(freq, 0),
        ptt,
        esc(&msg),
        s.rig_lead_ms,
        s.rig_tail_ms,
        s.rig_poll_ms
    );
    if with_lists {
        let models: Vec<String> = cp16plus_rig::models()
            .into_iter()
            .map(|(m, mfg, name)| format!("{{\"model\":{m},\"mfg\":\"{}\",\"name\":\"{}\"}}", esc(&mfg), esc(&name)))
            .collect();
        let ports: Vec<String> = cp16plus_rig::serial_ports()
            .into_iter()
            .map(|p| format!("\"{}\"", esc(&p)))
            .collect();
        out.pop(); // 去掉结尾的 }
        out.push_str(&format!(",\"models\":[{}],\"ports\":[{}]}}", models.join(","), ports.join(",")));
    }
    out
}

// ---------------------------------------------------------------- 本机发射

/// 发射状态机：idle → waiting（等整十秒）→ playing（正在发）→ idle
#[derive(Clone)]
struct TxState {
    state: &'static str,
    /// 目标发射时刻（Unix 毫秒，UTC 整十秒 + 补偿）
    start_ms: u64,
    /// 报文时长（秒）
    seconds: f64,
    offset_ms: i32,
    device: String,
    text: String,
    msg: String,
}

impl Default for TxState {
    fn default() -> Self {
        TxState {
            state: "idle",
            start_ms: 0,
            seconds: 0.0,
            offset_ms: 0,
            device: String::new(),
            text: String::new(),
            msg: String::new(),
        }
    }
}

fn tx_state() -> &'static Mutex<TxState> {
    static S: OnceLock<Mutex<TxState>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(TxState::default()))
}

/// 发射取消标志：等待整十秒阶段直接退出；播放阶段由音频回调看见后吐静音，
/// 发射期间重发 PTT 的次数（给界面/排障看：>0 说明确实发生过重发）
fn tx_ptt_reassert() -> &'static std::sync::atomic::AtomicU32 {
    static C: OnceLock<std::sync::atomic::AtomicU32> = OnceLock::new();
    C.get_or_init(|| std::sync::atomic::AtomicU32::new(0))
}

/// 于是阻塞中的 `play()` 会在一个缓冲区内返回——这就是"中途打断"。
fn tx_abort() -> Arc<AtomicBool> {
    static A: OnceLock<Arc<AtomicBool>> = OnceLock::new();
    A.get_or_init(|| Arc::new(AtomicBool::new(false))).clone()
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 下一个整十秒（UTC）+ 时差补偿；至少要留 1.5 s 给编码和声卡起流。
fn next_slot_ms(offset_ms: i32) -> u64 {
    let now = now_ms() as i64;
    let period = 10_000i64;
    let off = offset_ms as i64;
    let mut target = ((now - off).div_euclid(period) + 1) * period + off;
    if target - now < 1500 {
        target += period;
    }
    target.max(0) as u64
}

fn tx_status_json() -> String {
    let st = tx_state().lock().unwrap().clone();
    format!(
        "{{\"state\":\"{}\",\"start_ms\":{},\"now_ms\":{},\"seconds\":{},\
          \"offset_ms\":{},\"device\":\"{}\",\"text\":\"{}\",\"msg\":\"{}\",\"reassert\":{}}}",
        st.state,
        st.start_ms,
        now_ms(),
        num(st.seconds, 2),
        st.offset_ms,
        esc(&st.device),
        esc(&st.text),
        esc(&st.msg),
        tx_ptt_reassert().load(Ordering::SeqCst)
    )
}

/// 开始一次发射：先按整十秒排期，到点才真正放音。
fn start_tx(text: &str, mode: &str, call: &str, grid: &str) -> String {
    if text.trim().is_empty() {
        return "报文是空的".into();
    }
    {
        let st = tx_state().lock().unwrap();
        if st.state != "idle" {
            return format!("上一次发射还没结束（{}）", st.state);
        }
    }
    // 本机发射一律叠加同步帧（调试模式才允许关）
    let (out_dev, offset) = {
        let st = settings().lock().unwrap();
        (st.out_device.clone(), st.tx_offset_ms)
    };
    let bytes = match encode_wav(text, mode, call, grid, true) {
        Ok(b) => b,
        Err(e) => return e,
    };
    let parsed = match wav::parse(&bytes) {
        Ok(w) => w,
        Err(e) => return format!("内部编码失败：{e}"),
    };
    let x = parsed.samples;
    let seconds = x.len() as f64 / FS as f64;
    let target = next_slot_ms(offset);
    {
        let mut st = tx_state().lock().unwrap();
        *st = TxState {
            state: "waiting",
            start_ms: target,
            seconds,
            offset_ms: offset,
            device: out_dev.clone(),
            text: text.to_string(),
            msg: "等待整十秒".into(),
        };
    }
    tx_abort().store(false, Ordering::SeqCst);

    std::thread::spawn(move || {
        // 等到整十秒（每 20 ms 瞄一眼，顺便看有没有被取消）
        loop {
            if tx_abort().load(Ordering::SeqCst) {
                let mut st = tx_state().lock().unwrap();
                st.state = "idle";
                st.msg = "已取消".into();
                return;
            }
            let now = now_ms();
            if now >= target {
                break;
            }
            std::thread::sleep(Duration::from_millis(((target - now).min(20)).max(1)));
        }
        {
            let mut st = tx_state().lock().unwrap();
            st.state = "playing";
            st.msg = "发射中".into();
        }
        // 电台联动：提前 rig_lead_ms 拉 PTT（给射频链路留上升时间）
        let (lead, tail) = {
            let s = settings().lock().unwrap();
            (s.rig_lead_ms, s.rig_tail_ms)
        };
        let ptt_on = rig_ptt(true);
        if ptt_on && lead > 0 {
            std::thread::sleep(Duration::from_millis(lead as u64));
        }
        // PTT 保活：有些电台（尤其协谷 X6100 走 CAT PTT）会在发射中途自己松掉，
        // 这里在发射期间每 2.5 秒重发一次 set_ptt(on)，软件侧不掉链子。
        let keep_ptt = Arc::new(AtomicBool::new(ptt_on));
        if ptt_on {
            tx_ptt_reassert().store(0, Ordering::SeqCst);
            let ka = keep_ptt.clone();
            std::thread::spawn(move || {
                while ka.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(2500));
                    if !ka.load(Ordering::SeqCst) {
                        break;
                    }
                    if tx_state().lock().unwrap().state == "playing" {
                        rig_ptt(true);
                        tx_ptt_reassert().fetch_add(1, Ordering::SeqCst);
                    } else {
                        break;
                    }
                }
            });
        }
        #[cfg(feature = "audio")]
        {
            let dev = if out_dev.trim().is_empty() { None } else { Some(out_dev.as_str()) };
            let cancel = tx_abort();
            match cp16plus_audio::play(&x, FS as u32, dev, cancel.clone()) {
                Ok(name) => {
                    let aborted = cancel.load(Ordering::SeqCst);
                    let mut st = tx_state().lock().unwrap();
                    st.state = "idle";
                    st.device = name.clone();
                    st.msg = if aborted {
                        format!("已中断（{name}）")
                    } else {
                        format!("发射完成 · {name}")
                    };
                }
                Err(e) => {
                    let mut st = tx_state().lock().unwrap();
                    st.state = "idle";
                    st.msg = format!("发射失败：{e}");
                }
            }
        }
        #[cfg(not(feature = "audio"))]
        {
            let _ = &x;
            let mut st = tx_state().lock().unwrap();
            st.state = "idle";
            st.msg = "这个版本没有编入发射功能（用 -Audio 构建）".into();
        }
        // 播完（或被中断）都要松 PTT，这里不能提前 return
        keep_ptt.store(false, Ordering::SeqCst);
        if ptt_on {
            if tail > 0 {
                std::thread::sleep(Duration::from_millis(tail as u64));
            }
            rig_ptt(false);
        }
    });
    let wait = (target as f64 - now_ms() as f64) / 1000.0;
    format!(
        "已排期：{wait:.1} s 后开始（整十秒对齐，时差补偿 {offset} ms，时长 {seconds:.1} s）"
    )
}

fn shared() -> &'static Arc<Shared> {
    static S: OnceLock<Arc<Shared>> = OnceLock::new();
    S.get_or_init(|| {
        Arc::new(Shared {
            events: Mutex::new(Vec::new()),
            next_id: AtomicU64::new(1),
            capturing: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            generation: AtomicU64::new(1),
            status: Mutex::new(Status::default()),
            rows_dbg: Mutex::new(Vec::new()),
        probe: Mutex::new(Probe::default()),
        spec: Mutex::new(Vec::new()),
        clip_pct: Mutex::new(0.0),
        peak: Mutex::new(0.0),
        ch_used: AtomicUsize::new(0),
        link: Mutex::new(LinkInfo::default()),
        })
    })
}

// ---------------------------------------------------------------- JSON 小工具

/// 字节 → base64（原始频谱一帧 512 字节，编码后 ~684 字符，比 JSON 数组省一半）
fn base64(bytes: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((bytes.len() + 2) / 3 * 4);
    for c in bytes.chunks(3) {
        let b = [c[0], *c.get(1).unwrap_or(&0), *c.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if c.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
}

fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o
}

fn num(v: f64, prec: usize) -> String {
    if !v.is_finite() {
        return "0".into();
    }
    format!("{v:.prec$}")
}

fn status_json(s: &Status) -> String {
    let link = shared().link.lock().unwrap().clone();
    let hdr_obj = if link.call.is_empty() {
        "null".to_string()
    } else {
        format!(
            "{{\"call\":\"{}\",\"grid\":\"{}\",\"len\":{},\"type\":{},\"mode\":\"{}\",\"t\":{}}}",
            esc(&link.call),
            esc(&link.grid),
            link.len,
            link.typ,
            esc(&link.mode),
            num(link.t, 2)
        )
    };
    format!(
        "{{\"kind\":\"{}\",\"device\":\"{}\",\"fs\":{},\"channels\":{},\"blocks\":{},\
         \"level\":{},\"cells\":{},\"t\":{},\"hdr\":\"{}\",\"msg\":\"{}\",\"n\":{},\
         \"rpc\":{},\"mode\":\"{}\",\"capturing\":{},\
         \"sync\":\"{}\",\"offset_ms\":{},\"sync_frames\":{},\"hdr_seq\":{},\"hdr_obj\":{},\
         \"clip_pct\":{},\"peak\":{},\"ch_used\":{}}}",
        s.kind,
        esc(&s.device),
        s.fs,
        s.channels,
        s.blocks,
        num(s.level_db, 1),
        s.cells,
        num(s.t, 2),
        esc(&s.hdr),
        esc(&s.msg),
        s.n,
        s.rpc,
        esc(&s.mode),
        s.kind != "idle",
        link_slot(&link),
        num(link.offset_ms, 0),
        link.frames,
        link.seq,
        hdr_obj,
        num(*shared().clip_pct.lock().unwrap(), 3),
        num(*shared().peak.lock().unwrap() as f64, 3),
        shared().ch_used.load(Ordering::Relaxed)
    )
}

fn event_json(e: &Event) -> String {
    let mut o = String::with_capacity(64 + e.rows.len() * (e.rows.first().map_or(8, |r| r.len()) * 10));
    o.push_str(&format!(
        "{{\"id\":{},\"t\":{},\"cells\":{},\"hdr\":\"{}\",\"lvl\":{},\"blocks\":{},\"kind\":\"{}\"",
        e.id,
        num(e.t, 2),
        e.cells,
        esc(&e.hdr),
        num(e.level_db, 1),
        e.blocks,
        e.kind
    ));
    if !e.rows.is_empty() {
        o.push_str(",\"rows\":[");
        for (i, r) in e.rows.iter().enumerate() {
            if i > 0 {
                o.push(',');
            }
            o.push('[');
            for (j, v) in r.iter().enumerate() {
                if j > 0 {
                    o.push(',');
                }
                // 科学计数法：值域跨好几个数量级时也不会被截断成 0
                o.push_str(&format!("{:.4e}", *v as f64));
            }
            o.push(']');
        }
        o.push(']');
    }
    if !e.extra.is_empty() {
        o.push(',');
        o.push_str(&e.extra);
    }
    o.push('}');
    o
}

/// 打开着几个"原始瀑布"窗口。>0 才算频谱，否则零开销。
fn raw_watch() -> &'static AtomicUsize {
    static R: OnceLock<AtomicUsize> = OnceLock::new();
    R.get_or_init(|| AtomicUsize::new(0))
}

/// 原始频谱：窗长按"64 ms"折成样点数（**必须用设备实际采样率**，
/// 早期写死 8 kHz，导致 48 kHz 采集时频率被压成 1/6，信号挤在最左边）。
/// 步进 20 ms → 50 行/秒，字形一行（80 ms）能落到 4 行像素上。
const RAW_WIN_SECS: f64 = 0.064;
const RAW_HOP_SECS: f64 = 0.020;
/// 一帧往外送多少个频点（覆盖 0..4 kHz）
const RAW_BINS: usize = 512;
/// 显示上限（协议频段 + 余量；SSB 通带内足够）
const RAW_MAX_HZ: usize = 4000;

/// 原始瀑布的行率（帧/秒）。1 帧 = 1 像素行；调高只增加时间采样，不拉伸像素。
fn raw_fps() -> &'static std::sync::atomic::AtomicUsize {
    static F: OnceLock<std::sync::atomic::AtomicUsize> = OnceLock::new();
    F.get_or_init(|| std::sync::atomic::AtomicUsize::new(100))
}

fn push_event(sh: &Arc<Shared>, e: Event) {
    let mut q = sh.events.lock().unwrap();
    q.push(e);
    if q.len() > MAX_EVENTS {
        let drop_n = q.len() - MAX_EVENTS;
        q.drain(0..drop_n);
    }
}

// ---------------------------------------------------------------- 会话泵

enum Feed {
    Data(Vec<f32>),
    Idle,
    Eof,
}

struct Ctx {
    cells: usize,
    hdr: String,
    blocks: u64,
    level: f64,
    n: usize,
    rpc: usize,
    mode: String,
    last_t: f64,
    last_push: Instant,
    force_tick: f64,
    /// 诊断用平均频谱的累加缓冲
    spec_acc: Vec<f32>,
    /// 上次算频谱的时刻
    spec_last: Instant,
    /// 行归一化用的平滑峰值
    row_scale: f64,
}

fn rms_db(block: &[f32]) -> f64 {
    if block.is_empty() {
        return -120.0;
    }
    let mut acc = 0.0f64;
    for v in block {
        acc += (*v as f64) * (*v as f64);
    }
    let rms = (acc / block.len() as f64).sqrt();
    if rms <= 1e-9 {
        return -120.0;
    }
    20.0 * rms.log10()
}

/// 唯一的解码泵：实时采集和 WAV 回放都走这里，事件格式完全一致。
fn run_pump<F: FnMut() -> Feed>(sh: Arc<Shared>, my: u64, mut st: Stream, mut next: F) {
    let m = st.mode();
    let fs_raw = st.fs_in() as f64;
    let raw_win = ((fs_raw * RAW_WIN_SECS) as usize).max(64);
    // 注意：步进不能在这里定死 —— 行率是运行期可改的（界面滑杆），
    // 每一帧按当前 fps 现算，否则改了下拉框也不生效。

    // 零填充到 2 倍（不小于 2 的幂）：窗长决定时间分辨率，FFT 长度决定频率栅格
    let mut raw_fft = raw_win.next_power_of_two();
    if raw_fft < raw_win * 2 {
        raw_fft *= 2;
    }
    let raw_window = fft::hann(raw_win);
    let mut raw_buf: Vec<f32> = Vec::with_capacity(raw_win * 4);
    let mut ctx = Ctx {
        cells: 0,
        hdr: String::new(),
        blocks: 0,
        level: -120.0,
        n: m.n,
        rpc: 1 + m.n,
        mode: m.name.to_string(),
        last_t: -1.0,
        last_push: Instant::now(),
        force_tick: 0.12,
        spec_acc: Vec::new(),
        spec_last: Instant::now(),
        row_scale: 0.0,
    };
    loop {
        if sh.generation.load(Ordering::SeqCst) != my || sh.stop.load(Ordering::SeqCst) {
            break;
        }
        match next() {
            Feed::Eof => break,
            Feed::Idle => continue,
            Feed::Data(block) => {
                ctx.blocks += 1;
                let lvl = rms_db(&block);
                ctx.level = if ctx.level <= -119.0 { lvl } else { ctx.level * 0.75 + lvl * 0.25 };
                // 原始瀑布：只在有窗口在看的时候才算
                if raw_watch().load(Ordering::Relaxed) > 0 {
                    raw_buf.extend_from_slice(&block);
                    let fps = raw_fps().load(Ordering::Relaxed).max(10);
                    let raw_hop = ((fs_raw as usize) / fps).max(1);
                    while raw_buf.len() >= raw_win && raw_buf.len() >= raw_hop {
                        let (spec, peak) =
                            fft::frame_db_zp(&raw_buf[..raw_win], &raw_window, raw_fft);
                        let bins = fft::slice_band(
                            &spec,
                            fs_raw,
                            RAW_BINS,
                            0.0,
                            RAW_MAX_HZ as f64,
                        );
                        let rms = rms_db(&raw_buf[..raw_win]);
                        let b64 = base64(&bins);
                        let id = sh.next_id.fetch_add(1, Ordering::Relaxed);
                        push_event(
                            &sh,
                            Event {
                                id,
                                t: st.audio_seconds(),
                                cells: 0,
                                hdr: String::new(),
                                rows: Vec::new(),
                                level_db: rms,
                                blocks: 0,
                                kind: "raw",
                                extra: format!(
                                    "\"flo\":0,\"fhi\":{},\"n\":{},\"peak\":{},\"db\":\"{}\"",
                                    RAW_MAX_HZ,
                                    RAW_BINS,
                                    num(peak, 1),
                                    b64
                                ),
                            },
                        );
                        raw_buf.drain(..raw_hop);
                    }
                } else if !raw_buf.is_empty() {
                    raw_buf.clear();
                }
                // 诊断用平均频谱：每 250 ms 一帧，指数平均
                ctx.spec_acc.extend_from_slice(&block);
                let spec_win = ((st.fs_in() as f64 * RAW_WIN_SECS) as usize).max(64);
                if ctx.spec_acc.len() >= spec_win && ctx.spec_last.elapsed() >= Duration::from_millis(250) {
                    let win = fft::hann(spec_win);
                    let mut fft_len = spec_win.next_power_of_two();
                    if fft_len < spec_win * 2 {
                        fft_len *= 2;
                    }
                    let (sp, _) = fft::frame_db_zp(&ctx.spec_acc[..spec_win], &win, fft_len);
                    let bins = fft::slice_band_f32(&sp, st.fs_in() as f64, 512, 0.0, 4000.0);
                    if let Ok(mut a) = sh.spec.lock() {
                        if a.len() != bins.len() {
                            *a = bins.clone();
                        } else {
                            // 峰值保持（慢衰减）：诊断包常在播放结束后才导出，
                            // 用指数平均会只剩底噪，看不出信号到底在哪些频率上
                            for (x, y) in a.iter_mut().zip(bins.iter()) {
                                *x = (*x * 0.999).max(*y);
                            }
                        }
                    }
                    ctx.spec_acc.clear();
                    ctx.spec_last = Instant::now();
                } else if ctx.spec_acc.len() > spec_win * 8 {
                    ctx.spec_acc.clear();
                }

                // 每 2 秒看一次输入电平：太低或削顶都写进 DEBUG 日志（界面暂不改）
                if ctx.blocks % 200 == 0 {
                    let pct = sh.clip_pct.lock().map(|v| *v).unwrap_or(0.0);
                    let peak = sh.peak.lock().map(|v| *v).unwrap_or(0.0);
                    let ch = sh.ch_used.load(Ordering::Relaxed);
                    if pct > 0.05 {
                        let _ = peak;
                        push_error(&format!(
                            "输入削顶 {pct:.2}%——请降低输入增益"
                        ));
                    } else if ctx.level < -55.0 {
                        push_error(&format!(
                            "输入电平偏低 {:.0} dBFS（第 {} 路）——建议提高发端音量或改用线路输入",
                            ctx.level,
                            ch + 1
                        ));
                    }
                }
                let ev = st.feed(&block);
                let rows = st.take_rows();
                if !rows.is_empty() {
                    if let Ok(mut d) = sh.rows_dbg.lock() {
                        d.extend(rows.iter().cloned());
                        let over = d.len().saturating_sub(64);
                        if over > 0 {
                            d.drain(..over);
                        }
                    }
                }
                let t = st.audio_seconds();

                // 机读层草稿：解到报文头就更新；同步帧状态每次刷新（约 4 Hz 足够）
                if ctx.blocks % 40 == 0 || ev.as_ref().map(|e| e.hdr.is_some()).unwrap_or(false) {
                    {
                        let sn = st.snapshot();
                        if let Ok(mut p) = sh.probe.lock() {
                            p.a = sn.est.a;
                            p.b = sn.est.b;
                            p.conf = sn.est.conf;
                            p.mode = sn.mode.name.to_string();
                            p.t_row = sn.t_row;
                            let f_lo = sn.est.a * sn.mode.f_lo + sn.est.b;
                            let df = sn.est.a * sn.mode.df;
                            p.tones = (0..sn.mode.n)
                                .map(|i| f_lo + i as f64 * df)
                                .collect();
                        }
                    }
                    let (frames, slot, sync_t) = st.sync_info();
                    let count = st.hdr_count();
                    let hdr = st.hdr_info();
                    let mut link = sh.link.lock().unwrap();
                    if let Some(h) = hdr {
                        // 只有解出过更新的一帧才算新草稿（清空后同一帧不会回填）
                        if count != link.seen_count {
                            link.seen_count = count;
                            link.seq += 1;
                            link.call = h.call.clone();
                            link.grid = h.grid.clone();
                            link.len = h.length;
                            link.typ = h.msg_type;
                            link.t = t;
                            link.mode = ctx.mode.clone();
                        }
                        let _ = &h;
                    }
                    link.frames = frames;
                    if frames > 0 {
                        link.sync_t = sync_t;
                        link.sync_wall = now_ms();
                    }
                    if frames > 0 {
                        // 时差 = 同步帧里的 SLOT 与本地 UTC 的差（10 s 一格，取最近的一格）
                        let utc_s = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_secs() as i64)
                            .unwrap_or(0);
                        let local_slot = (utc_s / 10).rem_euclid(60);
                        let diff = (slot as i64 - local_slot + 90).rem_euclid(60) - 30;
                        link.offset_ms = diff as f64 * 10_000.0;
                    }
                }
                if let Some(e) = &ev {
                    ctx.cells = e.cells;
                    if let Some(h) = &e.hdr {
                        ctx.hdr = h.clone();
                    }
                }
                let tick = t - ctx.last_t >= ctx.force_tick
                    || ctx.last_push.elapsed() >= Duration::from_millis(200);
                // 行的绝对值只有 1e-5 量级，直接序列化会被四舍五入抹成 0
                // （实测 100% 的像素变成 0.0000，主界面因此"缺像素/全黑"）。
                // 这里按"平滑峰值"归一化到 0–1：保留行与行之间的强弱，同时保证
                // 任何输入电平下都有足够有效位。（用逐行最大值会让空白行被放大成噪声，
                // 所以必须用跨行的平滑峰值。）
                let mut rows = rows;
                if !rows.is_empty() {
                    for row in rows.iter() {
                        let mx = row.iter().cloned().fold(0.0f64, f64::max);
                        ctx.row_scale = if ctx.row_scale <= 0.0 {
                            mx
                        } else {
                            mx.max(ctx.row_scale * 0.98)
                        };
                    }
                    let sc = ctx.row_scale.max(1e-30);
                    for row in rows.iter_mut() {
                        for v in row.iter_mut() {
                            *v /= sc;
                        }
                    }
                }

                if !rows.is_empty() || ev.is_some() || tick {
                    ctx.last_t = t;
                    ctx.last_push = Instant::now();
                    let id = sh.next_id.fetch_add(1, Ordering::Relaxed);
                    push_event(
                        &sh,
                        Event {
                            id,
                            t,
                            cells: ctx.cells,
                            hdr: ctx.hdr.clone(),
                            rows: rows.iter().map(|r| r.iter().map(|v| *v as f32).collect()).collect(),
                            level_db: ctx.level,
                            blocks: ctx.blocks,
                            kind: "tick",
                            extra: String::new(),
                        },
                    );
                    let mut s = sh.status.lock().unwrap();
                    s.blocks = ctx.blocks;
                    s.level_db = ctx.level;
                    s.cells = ctx.cells;
                    s.t = t;
                    s.hdr = ctx.hdr.clone();
                    s.n = ctx.n;
                    s.rpc = ctx.rpc;
                    s.mode = ctx.mode.clone();
                }
            }
        }
    }

    // 手动停止也要给一份汇总：用户就是想看"这次收到几个字"。
    // 只有"被新会话顶掉"时才安静退出（比较代号），此时 summary 属于新会话。
    let stopped_by_user = sh.stop.load(Ordering::SeqCst);
    if sh.generation.load(Ordering::SeqCst) != my && !stopped_by_user {
        return;
    }
    let _ = st.finish();
    let snap = st.snapshot();
    let extra = format!(
        "\"summary\":{{\"cells\":{},\"a\":{},\"b\":{},\"conf\":{},\"mode\":\"{}\",\
         \"hdr\":{},\"anchors\":[{}],\"metrics\":{{\"eta\":{},\"kappa\":{},\"nu\":{},\"J\":{}}},\
         \"sync_frames\":{}}}",
        snap.cells,
        num(snap.est.a, 9),
        num(snap.est.b, 4),
        num(snap.est.conf, 3),
        esc(snap.mode.name),
        match &snap.hdr {
            Some(h) => format!(
                "{{\"call\":\"{}\",\"grid\":\"{}\",\"length\":{},\"raster\":{},\"k\":{},\"repeat\":{}}}",
                esc(&h.call),
                esc(&h.grid),
                h.length,
                h.raster,
                h.k,
                h.repeat
            ),
            None => "null".to_string(),
        },
        snap.anchors.iter().map(|a| format!("\"{}\"", esc(a))).collect::<Vec<_>>().join(","),
        num(snap.metrics.eta, 3),
        num(snap.metrics.kappa, 3),
        num(snap.metrics.nu, 3),
        num(snap.metrics.j, 3),
        snap.sync_frames
    );
    let id = sh.next_id.fetch_add(1, Ordering::Relaxed);
    push_event(
        &sh,
        Event {
            id,
            t: st.audio_seconds(),
            cells: snap.cells,
            hdr: ctx.hdr.clone(),
            rows: Vec::new(),
            level_db: ctx.level,
            blocks: ctx.blocks,
            kind: "summary",
            extra,
        },
    );
    let mut s = sh.status.lock().unwrap();
    s.kind = if sh.capturing.load(Ordering::SeqCst) { "live" } else { "idle" };
    s.cells = snap.cells;
    if let Some(h) = &snap.hdr {
        s.hdr = format!("{} {}", h.call, h.grid);
    }
    if s.msg.is_empty() {
        s.msg = format!("会话结束，共 {} 字", snap.cells);
    }
    drop(s);

    // 机读层只做"草稿"：不再自动写日志，由用户点「保存通联日志」落库
}

fn begin_session(kind: &'static str, device: String, msg: String) -> u64 {
    let sh = shared();
    let my = sh.generation.fetch_add(1, Ordering::SeqCst) + 1;
    sh.stop.store(false, Ordering::SeqCst);
    sh.events.lock().unwrap().clear();
    // 注意：next_id 必须保持单调递增。否则前端拿着 Last-Event-ID 重连时
    // 会因为 id 回到 1 而漏掉整场会话（断线重连是实时采集的常态）。
    let mut s = sh.status.lock().unwrap();
    *s = Status { kind, device, msg, ..Status::default() };
    drop(s);
    // 新会话＝新草稿：上一场的报文头不该继续显示（hdr_count 也是从 0 重新数的）
    {
        let mut link = sh.link.lock().unwrap();
        link.seq += 1;
        link.seen_count = 0;
        link.call.clear();
        link.grid.clear();
        link.len = 0;
        link.typ = 0;
        link.t = 0.0;
        link.frames = 0;
        link.sync_wall = 0;
    }
    my
}

fn push_error(msg: &str) {
    let sh = shared();
    let id = sh.next_id.fetch_add(1, Ordering::Relaxed);
    push_event(
        sh,
        Event {
            id,
            t: 0.0,
            cells: 0,
            hdr: String::new(),
            rows: Vec::new(),
            level_db: -120.0,
            blocks: 0,
            kind: "error",
            extra: format!("\"error\":\"{}\"", esc(msg)),
        },
    );
    sh.status.lock().unwrap().msg = msg.to_string();
}

// ---------------------------------------------------------------- 采集

fn start_live(device: Option<String>, loopback: bool, channel: ChMode) -> String {
    let sh = shared();
    if sh.capturing.load(Ordering::SeqCst) {
        return "已在采集中".into();
    }
    // 没指定设备就沿用上次记住的那台（换机器/换声卡后它不存在了会自动退回默认设备）
    let device = device.or_else(|| {
        let d = settings().lock().unwrap().device.clone();
        if d.is_empty() { None } else { Some(d) }
    });
    let my = begin_session(
        "live",
        device.clone().unwrap_or_else(|| "默认输入".into()),
        "正在打开设备…".into(),
    );

    #[cfg(feature = "audio")]
    {
        let sh2 = sh.clone();
        let (tx_ready, rx_ready) = std::sync::mpsc::channel::<Result<(u32, u16, String), String>>();
        std::thread::spawn(move || {
            let cfg = cp16plus_audio::SourceConfig {
                device,
                sample_rate: None,
                loopback,
                channel,
            };
            // cpal 的 Stream 不是 Send，所以"打开设备"这一步必须在泵线程里做，
            // 只把字符串配置搬进来 —— 这也是之前"实时采集没数据"始终查不出的原因：
            // 旧写法把 Capture 搬进了线程，编译期就过不去（只是当时用 --no-audio 构建没暴露）。
            let cap = match cp16plus_audio::open(&cfg) {
                Ok(c) => c,
                Err(e) => {
                    let _ = tx_ready.send(Err(e));
                    return;
                }
            };
            let _ = tx_ready.send(Ok((cap.fs, cap.channels, cap.name.clone())));
            {
                let mut s = sh2.status.lock().unwrap();
                s.device = cap.name.clone();
                s.fs = cap.fs;
                s.channels = cap.channels;
                s.msg = "采集中".into();
            }
            // 打开成功就把这台设备记下来：下次启动不用再选
            settings_update(Patch {
                device: Some(cap.name.clone()),
                loopback: Some(loopback),
                ..Patch::none()
            });
            {
                let meter = cap.meter.clone();
                let sh3 = sh2.clone();
                std::thread::spawn(move || loop {
                    if !sh3.capturing.load(Ordering::SeqCst) {
                        break;
                    }
                    if let Ok(mut c) = sh3.clip_pct.lock() {
                        *c = meter.clip_pct();
                    }
                    if let Ok(mut p) = sh3.peak.lock() {
                        *p = meter.peak();
                    }
                    sh3.ch_used
                        .store(meter.ch_used.load(Ordering::Relaxed), Ordering::Relaxed);
                    std::thread::sleep(Duration::from_millis(500));
                });
            }
            sh2.capturing.store(true, Ordering::SeqCst);
            let st = Stream::new(cap.fs as usize, None);
            run_pump(sh2, my, st, || match cap.recv() {
                Some(b) => Feed::Data(b),
                None => Feed::Idle,
            });
        });
        match rx_ready.recv_timeout(Duration::from_secs(4)) {
            Ok(Ok((fs, ch, name))) => format!("开始采集：{name} · {fs} Hz · {ch} 声道"),
            Ok(Err(e)) => {
                push_error(&e);
                shared().status.lock().unwrap().kind = "idle";
                format!("打不开设备：{e}")
            }
            Err(_) => {
                push_error("设备打开超时");
                shared().status.lock().unwrap().kind = "idle";
                "设备打开超时".into()
            }
        }
    }
    #[cfg(not(feature = "audio"))]
    {
        let _ = (device, loopback, my);
        push_error("这个版本没有编入采集功能（用 -Audio 构建）");
        shared().status.lock().unwrap().kind = "idle";
        "这个版本没有编入采集功能（用 -Audio 构建）".into()
    }
}

fn stop_audio() -> String {
    let sh = shared();
    let was = sh.capturing.swap(false, Ordering::SeqCst);
    sh.stop.store(true, Ordering::SeqCst);
    sh.generation.fetch_add(1, Ordering::SeqCst);
    {
        let mut s = sh.status.lock().unwrap();
        s.kind = "idle";
        s.msg = if was { "已停止".into() } else { "本来就没在采集".into() };
        s.level_db = -120.0;
    }
    sh.status.lock().unwrap().msg.clone()
}

// ---------------------------------------------------------------- 回放

/// WAV 回放：按实时节奏把音频喂进同一个 Stream，事件格式与实时采集完全一致。
/// 用户明确要求"WAV 只是实时音频的替代品"，所以这里不允许批量解码后贴图。
fn start_replay(x: Vec<f32>, what: String) -> String {
    let sh = shared();
    let fs = FS;
    let secs = x.len() as f64 / fs as f64;
    let my = begin_session("replay", "WAV 回放".into(), format!("回放 {what}"));
    {
        let mut s = sh.status.lock().unwrap();
        s.fs = fs as u32;
        s.channels = 1;
    }
    let sh2 = sh.clone();
    std::thread::spawn(move || {
        let st = Stream::new(fs, None);
        let block = fs / 50; // 20 ms
        let mut at = 0usize;
        let t0 = Instant::now();
        run_pump(sh2, my, st, || {
            if at >= x.len() {
                return Feed::Eof;
            }
            let end = (at + block).min(x.len());
            let chunk = x[at..end].to_vec();
            at = end;
            let want = (at as f64 / fs as f64) - t0.elapsed().as_secs_f64();
            if want > 0.0 {
                std::thread::sleep(Duration::from_secs_f64(want.min(0.5)));
            }
            Feed::Data(chunk)
        });
    });
    format!("回放中：{secs:.1} s")
}

// ---------------------------------------------------------------- 编码 / 字库

fn font_count() -> usize {
    if FONT_BYTES.len() < 10 {
        return 0;
    }
    u32::from_le_bytes([FONT_BYTES[6], FONT_BYTES[7], FONT_BYTES[8], FONT_BYTES[9]]) as usize
}

fn encode_wav(text: &str, mode: &str, call: &str, grid: &str, sync: bool) -> Result<Vec<u8>, String> {
    use cp16plus_core::font::CpfFont;
    let font = CpfFont::load_bytes(FONT_BYTES.to_vec())?;
    let (glyphs, _kept) = font.text(text);
    if glyphs.is_empty() {
        return Err("这些字不在字库里（本版字库是 GB2312 一级+二级 16×16）".into());
    }
    let m = params::by_name(mode).ok_or_else(|| format!("未知档位 {mode}"))?;
    // 呼号/网格留空时用记住的那一份
    let (def_call, def_grid) = {
        let s = settings().lock().unwrap();
        (s.call.clone(), s.grid.clone())
    };
    let opts = cp16plus_core::tx::TxOpts {
        call: if call.is_empty() { def_call } else { call.into() },
        grid: if grid.is_empty() { def_grid } else { grid.into() },
        sync_period: if sync { 10.0 } else { 0.0 },
        ..Default::default()
    };
    let (x, _meta) = cp16plus_core::tx::build(&glyphs, m, &opts);
    Ok(wav::to_bytes(&x, FS as u32))
}

fn glyphs_json(text: &str) -> String {
    use cp16plus_core::font::CpfFont;
    let Ok(font) = CpfFont::load_bytes(FONT_BYTES.to_vec()) else {
        return "{\"ok\":false}".into();
    };
    let mut items = Vec::new();
    let mut missing = String::new();
    for ch in text.chars() {
        match font.raster(ch) {
            Some(bits) => {
                let rows: Vec<String> = (0..font.n)
                    .map(|y| {
                        (0..font.n)
                            .map(|x| if bits[y * font.n + x] != 0 { '1' } else { '0' })
                            .collect::<String>()
                    })
                    .collect();
                items.push(format!("{{\"ch\":\"{}\",\"rows\":[\"{}\"]}}", esc(&ch.to_string()), rows.join("\",\"")));
            }
            None => missing.push(ch),
        }
    }
    format!(
        "{{\"ok\":true,\"n\":{},\"glyphs\":[{}],\"missing\":\"{}\"}}",
        font.n,
        items.join(","),
        esc(&missing)
    )
}

fn info_json() -> String {
    let modes: Vec<String> = params::MODES
        .iter()
        .map(|m| {
            format!(
                "{{\"name\":\"{}\",\"n\":{},\"df\":{},\"k\":{},\"repeat\":{},\"row_s\":{},\
                 \"char_s\":{},\"f_lo\":{},\"f_hi\":{}}}",
                m.name,
                m.n,
                num(m.df, 2),
                m.k,
                m.repeat,
                num(m.t_row(), 4),
                num(m.t_row() * m.rows_per_char() as f64, 3),
                num(m.f_lo, 1),
                num(m.f_hi(), 1)
            )
        })
        .collect();
    format!(
        "{{\"version\":\"{}\",\"proto\":{},\"fs\":{},\"pilot\":[{},{}],\"cc_hz\":{},\"cc_baud\":{},\
         \"font\":{{\"n\":16,\"count\":{}}},\"modes\":[{}]}}",
        env!("CARGO_PKG_VERSION"),
        params::PROTO_VER,
        FS,
        num(params::PILOT1, 1),
        num(params::PILOT2, 1),
        num(params::CC_CENTER, 1),
        num(params::CC_BAUD, 1),
        font_count(),
        modes.join(",")
    )
}

// ---------------------------------------------------------------- HTTP

fn query_param(q: &str, key: &str) -> String {
    for kv in q.split('&') {
        let mut it = kv.splitn(2, '=');
        let k = it.next().unwrap_or("");
        if k == key {
            return urldecode(it.next().unwrap_or(""));
        }
    }
    String::new()
}

fn urldecode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => {
                let h = |c: u8| -> u8 {
                    match c {
                        b'0'..=b'9' => c - b'0',
                        b'a'..=b'f' => c - b'a' + 10,
                        b'A'..=b'F' => c - b'A' + 10,
                        _ => 0,
                    }
                };
                out.push(h(b[i + 1]) * 16 + h(b[i + 2]));
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn respond(stream: &mut TcpStream, status: &str, ctype: &str, body: &[u8]) {
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\n\
         Cache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body);
    let _ = stream.flush();
}

fn json_ok(stream: &mut TcpStream, body: &str) {
    respond(stream, "200 OK", "application/json; charset=utf-8", body.as_bytes());
}

fn handle(mut stream: TcpStream) {
    let _ = stream.set_nodelay(true);
    let mut reader = BufReader::new(match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    });
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() {
        return;
    }
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let target = parts.next().unwrap_or("/").to_string();

    let mut content_length = 0usize;
    let mut last_event_id = 0u64;
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h).is_err() || h.trim().is_empty() {
            break;
        }
        let lower = h.to_ascii_lowercase();
        if let Some(v) = lower.strip_prefix("content-length:") {
            content_length = v.trim().parse().unwrap_or(0);
        } else if let Some(v) = lower.strip_prefix("last-event-id:") {
            last_event_id = v.trim().parse().unwrap_or(0);
        }
    }

    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (target.clone(), String::new()),
    };

    match (method.as_str(), path.as_str()) {
        ("GET", "/") | ("GET", "/index.html") => {
            respond(&mut stream, "200 OK", "text/html; charset=utf-8", UI.as_bytes());
        }
        ("GET", "/favicon.ico") => {
            respond(&mut stream, "204 No Content", "image/x-icon", b"");
        }
        ("GET", "/api/ping") => {
            json_ok(
                &mut stream,
                &format!(
                    "{{\"ok\":true,\"app\":\"cp16plus-app\",\"version\":\"{}\",\"diag\":{}}}",
                    env!("CARGO_PKG_VERSION"),
                    cp16plus_core::DIAG_BUILD
                ),
            );
        }
        ("GET", "/api/info") => json_ok(&mut stream, &info_json()),
        ("GET", "/api/status") => {
            let s = shared().status.lock().unwrap().clone();
            json_ok(&mut stream, &status_json(&s));
        }
        ("GET", "/api/history") => {
            // 界面刚打开（或换页面回来）时先把已经发生的事补上：
            // 这样"窗口开着但会话已经跑了一半"不会看到一片空白。
            let sh = shared();
            let ev = sh.events.lock().unwrap();
            let mut o = String::from("{\"ok\":true,\"events\":[");
            for (i, e) in ev.iter().enumerate() {
                if i > 0 {
                    o.push(',');
                }
                o.push_str(&event_json(e));
            }
            o.push_str("]}");
            json_ok(&mut stream, &o);
        }
        ("GET", "/api/settings") => {
            let s = settings().lock().unwrap().clone();
            json_ok(&mut stream, &settings_json(&s));
        }
        ("POST", "/api/settings") => {
            let p = patch_from_query(&query);
            let want_freq = p.freq_khz;
            let out = settings_update(p);
            // 频率以主界面为准：改了就同步给电台（电台没连就只更新设置/日志）
            if let Some(khz) = want_freq {
                if rig_slot().lock().unwrap().is_some() {
                    let hz = khz as f64 * 1000.0;
                    match rig_with(|r| r.set_freq(hz)) {
                        Some(Ok(())) => {
                            rig_cache().lock().unwrap().0 = hz;
                            rig_set_status(&format!("已下发 {} kHz 到电台", khz));
                        }
                        Some(Err(e)) => rig_set_status(&format!("下发频率失败：{e}")),
                        None => {}
                    }
                }
            }
            json_ok(&mut stream, &out);
        }
        ("POST", "/api/save-wav") => {
            let text = query_param(&query, "text");
            let mut mode = query_param(&query, "mode");
            if mode.is_empty() {
                mode = params::DEFAULT_MODE.to_string();
            }
            let call = query_param(&query, "call");
            let grid = query_param(&query, "grid");
            // 本机发射一律叠加同步帧；只有调试模式才允许关掉
            let sync = if query_param(&query, "sync") == "0" { false } else { true };
            match encode_wav(&text, &mode, &call, &grid, sync) {
                Ok(bytes) => {
                    let name = format!("cp16plus-{}.wav", utc_stamp());
                    match save_in_out_dir(&name, &bytes) {
                        Ok(path) => json_ok(
                            &mut stream,
                            &format!("{{\"ok\":true,\"path\":\"{}\"}}", esc(&path)),
                        ),
                        Err(e) => respond(
                            &mut stream,
                            "500 Internal Server Error",
                            "text/plain; charset=utf-8",
                            e.as_bytes(),
                        ),
                    }
                }
                Err(e) => respond(
                    &mut stream,
                    "400 Bad Request",
                    "text/plain; charset=utf-8",
                    e.as_bytes(),
                ),
            }
        }
        ("POST", "/api/save-png") => {
            let mut body = vec![0u8; content_length];
            if reader.read_exact(&mut body).is_err() {
                respond(&mut stream, "400 Bad Request", "text/plain", b"short body");
                return;
            }
            let name = {
                let n = query_param(&query, "name");
                if n.is_empty() {
                    format!("cp16plus-{}.png", utc_stamp())
                } else {
                    n
                }
            };
            match save_in_out_dir(&name, &body) {
                Ok(path) => json_ok(&mut stream, &format!("{{\"ok\":true,\"path\":\"{}\"}}", esc(&path))),
                Err(e) => respond(
                    &mut stream,
                    "500 Internal Server Error",
                    "text/plain; charset=utf-8",
                    e.as_bytes(),
                ),
            }
        }
        ("POST", "/api/save-log") => {
            let mut body = vec![0u8; content_length];
            if reader.read_exact(&mut body).is_err() {
                respond(&mut stream, "400 Bad Request", "text/plain", b"short body");
                return;
            }
            let name = format!("cp16plus-debug-{}.txt", utc_stamp());
            match save_in_out_dir(&name, &body) {
                Ok(path) => json_ok(&mut stream, &format!("{{\"ok\":true,\"path\":\"{}\"}}", esc(&path))),
                Err(e) => respond(
                    &mut stream,
                    "500 Internal Server Error",
                    "text/plain; charset=utf-8",
                    e.as_bytes(),
                ),
            }
        }
        ("GET", "/api/qso") => {
            let list = qso_load();
            json_ok(&mut stream, &qso_json(&list));
        }
        ("POST", "/api/qso") => {
            // action=add|update|delete；update/delete 用 idx（列表下标，从 0 开始）
            let action = query_param(&query, "action");
            let idx: Option<usize> = query_param(&query, "idx").parse().ok();
            let mut list = qso_load();
            match action.as_str() {
                "add" => list.push(Qso {
                    call: query_param(&query, "call").trim().to_uppercase(),
                    grid: query_param(&query, "grid").trim().to_uppercase(),
                    qth: query_param(&query, "qth").trim().to_string(),
                    rst_sent: query_param(&query, "rst_sent").trim().to_string(),
                    rst_rcvd: query_param(&query, "rst_rcvd").trim().to_string(),
                    my_call: query_param(&query, "my_call").trim().to_uppercase(),
                    my_grid: query_param(&query, "my_grid").trim().to_uppercase(),
                    freq: query_param(&query, "freq").trim().to_string(),
                    date: query_param(&query, "date"),
                    time: query_param(&query, "time"),
                    mode: query_param(&query, "mode"),
                    note: query_param(&query, "note"),
                }),
                "update" => {
                    if let Some(i) = idx {
                        if let Some(q) = list.get_mut(i) {
                            let call = query_param(&query, "call");
                            if !call.is_empty() {
                                q.call = call.trim().to_uppercase();
                            }
                            let grid = query_param(&query, "grid");
                            if !grid.is_empty() {
                                q.grid = grid.trim().to_uppercase();
                            }
                            let freq = query_param(&query, "freq");
                            if !freq.is_empty() {
                                q.freq = freq.trim().to_string();
                            }
                            let date = query_param(&query, "date");
                            if !date.is_empty() {
                                q.date = date;
                            }
                            let time = query_param(&query, "time");
                            if !time.is_empty() {
                                q.time = time;
                            }
                            let mode = query_param(&query, "mode");
                            if !mode.is_empty() {
                                q.mode = mode;
                            }
                            let note = query_param(&query, "note");
                            if !note.is_empty() {
                                q.note = note;
                            }
                        }
                    }
                }
                "delete" => {
                    if let Some(i) = idx {
                        if i < list.len() {
                            list.remove(i);
                        }
                    }
                }
                _ => {}
            }
            qso_save(&list);
            json_ok(&mut stream, &qso_json(&list));
        }
        ("POST", "/api/qso/export") => {
            let list = qso_load();
            let text = qso_adif(&list);
            let name = format!("cp16plus-{}.adi", utc_stamp());
            match save_in_out_dir(&name, text.as_bytes()) {
                Ok(path) => json_ok(
                    &mut stream,
                    &format!(
                        "{{\"ok\":true,\"path\":\"{}\",\"count\":{}}}",
                        esc(&path),
                        list.len()
                    ),
                ),
                Err(e) => respond(
                    &mut stream,
                    "500 Internal Server Error",
                    "text/plain; charset=utf-8",
                    e.as_bytes(),
                ),
            }
        }
        ("POST", "/api/reveal") => {
            let path = query_param(&query, "path");
            if !path.is_empty() {
                let _ = std::process::Command::new("explorer")
                    .arg(format!("/select,{}", path))
                    .spawn();
            }
            json_ok(&mut stream, "{\"ok\":true}");
        }
        ("POST", "/api/open-dir") => {
            let dir = settings().lock().unwrap().out_dir();
            let _ = std::fs::create_dir_all(&dir);
            let _ = std::process::Command::new("explorer").arg(&dir).spawn();
            json_ok(&mut stream, "{\"ok\":true}");
        }
        ("GET", "/api/devices") => {
            #[cfg(feature = "audio")]
            {
                let devs = cp16plus_audio::list_devices();
                let outs = cp16plus_audio::list_output_devices();
                let def = cp16plus_audio::default_device_name().unwrap_or_default();
                let def_out = cp16plus_audio::default_output_device_name().unwrap_or_default();
                let (saved, saved_out) = {
                    let s = settings().lock().unwrap();
                    (s.device.clone(), s.out_device.clone())
                };
                let inputs: Vec<String> = devs
                    .iter()
                    .map(|d| format!("{{\"name\":\"{}\",\"default\":{}}}", esc(d), d == &def))
                    .collect();
                let outputs: Vec<String> = outs
                    .iter()
                    .map(|d| format!("{{\"name\":\"{}\",\"default\":{}}}", esc(d), d == &def_out))
                    .collect();
                json_ok(
                    &mut stream,
                    &format!(
                        "{{\"ok\":true,\"default\":\"{}\",\"saved\":\"{}\",\
                          \"default_out\":\"{}\",\"saved_out\":\"{}\",\
                          \"devices\":[{}],\"outputs\":[{}]}}",
                        esc(&def),
                        esc(&saved),
                        esc(&def_out),
                        esc(&saved_out),
                        inputs.join(","),
                        outputs.join(",")
                    ),
                );
            }
            #[cfg(not(feature = "audio"))]
            {
                json_ok(
                    &mut stream,
                    "{\"ok\":false,\"default\":\"\",\"saved\":\"\",\"default_out\":\"\",\
                      \"saved_out\":\"\",\"devices\":[],\"outputs\":[]}",
                );
            }
        }
        ("POST", "/api/tx") => {
            let text = query_param(&query, "text");
            let mut mode = query_param(&query, "mode");
            if mode.is_empty() {
                let s = settings().lock().unwrap();
                mode = s.mode.clone();
            }
            let msg = start_tx(&text, &mode, &query_param(&query, "call"), &query_param(&query, "grid"));
            json_ok(
                &mut stream,
                &format!("{{\"ok\":true,\"msg\":\"{}\",\"tx\":{}}}", esc(&msg), tx_status_json()),
            );
        }
        ("GET", "/api/tx/status") => json_ok(&mut stream, &tx_status_json()),
        ("GET", "/api/rig") => {
            let lists = query_param(&query, "lists") == "1";
            json_ok(&mut stream, &rig_json(lists));
        }
        ("GET", "/api/debug/rows") => {
            let sh = shared();
            let rows = sh.rows_dbg.lock().map(|d| d.clone()).unwrap_or_default();
            let probe = sh.probe.lock().map(|p| p.clone()).unwrap_or_default();
            let n = rows.first().map(|r| r.len()).unwrap_or(0);
            let mut body = String::from("{\"ok\":true");
            body.push_str(&format!(",\"n\":{n},\"rows\":["));
            for (i, r) in rows.iter().enumerate() {
                if i > 0 {
                    body.push(',');
                }
                body.push('[');
                for (k, v) in r.iter().enumerate() {
                    if k > 0 {
                        body.push(',');
                    }
                    body.push_str(&format!("{:.6}", v));
                }
                body.push(']');
            }
            body.push(']');
            body.push_str(&format!(
                ",\"est\":{{\"a\":{:.6},\"b\":{:.3},\"conf\":{:.3}}},\"mode\":\"{}\",\"t_row\":{:.6}",
                probe.a, probe.b, probe.conf, probe.mode, probe.t_row
            ));
            body.push_str(",\"tones\":[");
            for (i, t) in probe.tones.iter().enumerate() {
                if i > 0 {
                    body.push(',');
                }
                body.push_str(&format!("{:.2}", t));
            }
            body.push(']');
            let spec = sh.spec.lock().map(|v| v.clone()).unwrap_or_default();
            body.push_str(",\"spec\":[");
            for (i, v) in spec.iter().enumerate() {
                if i > 0 {
                    body.push(',');
                }
                body.push_str(&format!("{:.1}", v));
            }
            body.push(']');
            let st = sh.status.lock().map(|s| (s.fs, s.channels, s.device.clone())).unwrap_or((0, 0, String::new()));
            body.push_str(&format!(
                ",\"fs_in\":{},\"channels\":{},\"device\":\"{}\"",
                st.0,
                st.1,
                esc(&st.2)
            ));
            body.push('}');
            respond(&mut stream, "200 OK", "application/json; charset=utf-8", body.as_bytes());
        }
        ("POST", "/api/raw/rate") => {
            let fps: usize = query_param(&query, "fps").parse().unwrap_or(100);
            raw_fps().store(fps.clamp(20, 400), Ordering::Relaxed);
            json_ok(
                &mut stream,
                &format!("{{\"ok\":true,\"fps\":{}}}", raw_fps().load(Ordering::Relaxed)),
            );
        }
        ("POST", "/api/link/clear") => {
            // 保存日志后清空草稿：等下一帧新的报文头，避免上一条通联的信息继续显示
            let mut link = shared().link.lock().unwrap();
            link.seq += 1;
            link.call.clear();
            link.grid.clear();
            link.len = 0;
            link.typ = 0;
            link.t = 0.0;
            drop(link);
            json_ok(&mut stream, "{\"ok\":true}");
        }
        ("POST", "/api/raw/watch") => {
            let on = query_param(&query, "on") == "1";
            if on {
                raw_watch().fetch_add(1, Ordering::Relaxed);
            } else {
                let _ = raw_watch().fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                    Some(v.saturating_sub(1))
                });
            }
            json_ok(
                &mut stream,
                &format!("{{\"ok\":true,\"watch\":{}}}", raw_watch().load(Ordering::Relaxed)),
            );
        }
        ("POST", "/api/window") => {
            // 单独开一个应用窗口（原始瀑布图用）：同一个引擎、不同 hash
            let page = query_param(&query, "page");
            let host = stream
                .local_addr()
                .map(|a| format!("127.0.0.1:{}", a.port()))
                .unwrap_or_else(|_| "127.0.0.1:8770".to_string());
            let url = format!("http://{host}/#{}", if page.is_empty() { "raw".into() } else { page });
            match crate::browser::open_app_window(&url) {
                crate::browser::Launch::Window(_) | crate::browser::Launch::HandedOff => {
                    json_ok(&mut stream, "{\"ok\":true}")
                }
                crate::browser::Launch::NoBrowser => {
                    respond(&mut stream, "502 Bad Gateway", "text/plain; charset=utf-8",
                            "找不到 Edge/Chrome，无法开新窗口".as_bytes());
                }
            }
        }
        ("POST", "/api/rig") => {
            let p = patch_from_query(&query);
            let _ = settings_update(p);
            let msg = rig_reconnect();
            json_ok(
                &mut stream,
                &format!("{{\"ok\":true,\"msg\":\"{}\",\"rig\":{}}}", esc(&msg), rig_json(false)),
            );
        }
        ("POST", "/api/rig/read") => {
            let r: Option<Result<f64, String>> = rig_with(|r| r.read_freq());
            match r {
                Some(Ok(hz)) => {
                    rig_cache().lock().unwrap().0 = hz;
                    let khz = (hz / 1000.0).round() as u32;
                    let _ = settings_update(Patch { freq_khz: Some(khz), ..Patch::none() });
                    rig_set_status(&format!("读回 {:.3} MHz", hz / 1e6));
                    json_ok(&mut stream, &format!("{{\"ok\":true,\"freq_hz\":{},\"freq_khz\":{}}}", num(hz, 0), khz));
                }
                Some(Err(e)) => {
                    rig_set_status(&e);
                    respond(&mut stream, "502 Bad Gateway", "text/plain; charset=utf-8", e.as_bytes());
                }
                None => respond(&mut stream, "409 Conflict", "text/plain; charset=utf-8", "电台未连接".as_bytes()),
            }
        }
        ("POST", "/api/rig/set-freq") => {
            let hz: f64 = query_param(&query, "freq_hz").parse().unwrap_or(0.0);
            let khz = query_param(&query, "khz");
            let target = if hz > 0.0 {
                hz
            } else {
                khz.parse::<f64>().unwrap_or(0.0) * 1000.0
            };
            if target <= 0.0 {
                respond(&mut stream, "400 Bad Request", "text/plain", b"bad freq");
                return;
            }
            match rig_with(|r| r.set_freq(target)) {
                Some(Ok(())) => {
                    rig_cache().lock().unwrap().0 = target;
                    let _ = settings_update(Patch {
                        freq_khz: Some((target / 1000.0).round() as u32),
                        ..Patch::none()
                    });
                    json_ok(&mut stream, "{\"ok\":true}");
                }
                Some(Err(e)) => {
                    rig_set_status(&e);
                    respond(&mut stream, "502 Bad Gateway", "text/plain; charset=utf-8", e.as_bytes());
                }
                None => respond(&mut stream, "409 Conflict", "text/plain", b"not connected"),
            }
        }
        ("POST", "/api/rig/test") => {
            // CAT 自检：没连上就试着连；连上了就读一次频率/PTT，把过程写清楚
            let was_connected = rig_slot().lock().unwrap().is_some();
            let open_msg = if was_connected {
                "已在连接状态".to_string()
            } else {
                rig_reconnect()
            };
            let connected = rig_slot().lock().unwrap().is_some();
            let mut lines: Vec<String> = vec![open_msg];
            let mut ok = connected;
            if connected {
                match rig_with(|r| r.read_freq()) {
                    Some(Ok(hz)) => {
                        rig_cache().lock().unwrap().0 = hz;
                        lines.push(format!("读频率 OK：{:.3} MHz", hz / 1e6));
                    }
                    Some(Err(e)) => {
                        ok = false;
                        lines.push(format!("读频率失败：{e}"));
                    }
                    None => {
                        ok = false;
                        lines.push("读频率失败：电台未连接".into());
                    }
                }
                // PTT 只读当前状态，不主动发射
                match rig_with(|r| r.read_ptt()) {
                    Some(Ok(p)) => lines.push(format!("PTT 当前：{}", if p { "发射中" } else { "接收" })),
                    Some(Err(e)) => lines.push(format!("读 PTT 失败（多数电台不支持读，可忽略）：{e}")),
                    None => {}
                }
            }
            let msg = lines.join(" · ");
            rig_set_status(&msg);
            json_ok(
                &mut stream,
                &format!("{{\"ok\":{},\"msg\":\"{}\",\"rig\":{}}}", ok, esc(&msg), rig_json(false)),
            );
        }
        ("POST", "/api/rig/ptt") => {
            let on = query_param(&query, "on") == "1";
            match rig_with(|r| r.set_ptt(on)) {
                Some(Ok(())) => {
                    rig_cache().lock().unwrap().1 = on;
                    json_ok(&mut stream, "{\"ok\":true}")
                }
                Some(Err(e)) => {
                    rig_set_status(&e);
                    respond(&mut stream, "502 Bad Gateway", "text/plain; charset=utf-8", e.as_bytes());
                }
                None => respond(&mut stream, "409 Conflict", "text/plain", b"not connected"),
            }
        }
        ("POST", "/api/tx/stop") => {
            let phase = tx_state().lock().unwrap().state;
            tx_abort().store(true, Ordering::SeqCst);
            let msg = match phase {
                "waiting" => "已取消发射",
                "playing" => "已中断发射",
                _ => "当前没有在发射",
            };
            json_ok(&mut stream, &format!("{{\"ok\":true,\"msg\":\"{msg}\"}}"));
        }
        ("POST", "/api/audio") => {
            let action = query_param(&query, "action");
            let loopback = query_param(&query, "loopback") == "1";
            let device = {
                let d = query_param(&query, "device");
                if d.is_empty() || d == "@default" {
                    None
                } else {
                    Some(d)
                }
            };
            // 声道策略：auto（默认，自动挑更响的一路）/ mix（旧行为）/ 0 / 1
            let channel = match query_param(&query, "channel").as_str() {
                "mix" => ChMode::Mix,
                "0" => ChMode::Index(0),
                "1" => ChMode::Index(1),
                _ => ChMode::Auto,
            };
            let msg = if action == "start" {
                start_live(device, loopback, channel)
            } else {
                stop_audio()
            };
            json_ok(
                &mut stream,
                &format!("{{\"ok\":true,\"msg\":\"{}\"}}", esc(&msg)),
            );
        }
        ("POST", "/api/decode") => {
            let mut body = vec![0u8; content_length];
            if reader.read_exact(&mut body).is_err() {
                respond(&mut stream, "400 Bad Request", "text/plain", b"short body");
                return;
            }
            match wav::parse(&body) {
                Ok(w) => {
                    let x = if w.fs as usize == FS {
                        w.samples
                    } else {
                        wav::resample(&w.samples, w.fs, FS as u32)
                    };
                    let name = {
                        let n = query_param(&query, "name");
                        if n.is_empty() {
                            "上传的 WAV".to_string()
                        } else {
                            n
                        }
                    };
                    let secs = x.len() as f64 / FS as f64;
                    let msg = start_replay(x, name);
                    json_ok(
                        &mut stream,
                        &format!("{{\"ok\":true,\"seconds\":{},\"msg\":\"{}\"}}", num(secs, 2), esc(&msg)),
                    );
                }
                Err(e) => respond(
                    &mut stream,
                    "400 Bad Request",
                    "text/plain; charset=utf-8",
                    e.as_bytes(),
                ),
            }
        }
        ("GET", "/api/encode") => {
            let text = query_param(&query, "text");
            let mut mode = query_param(&query, "mode");
            if mode.is_empty() {
                mode = params::DEFAULT_MODE.to_string();
            }
            let call = query_param(&query, "call");
            let grid = query_param(&query, "grid");
            let sync = query_param(&query, "sync") == "1";
            match encode_wav(&text, &mode, &call, &grid, sync) {
                Ok(bytes) => respond(&mut stream, "200 OK", "audio/wav", &bytes),
                Err(e) => respond(
                    &mut stream,
                    "400 Bad Request",
                    "text/plain; charset=utf-8",
                    e.as_bytes(),
                ),
            }
        }
        ("GET", "/api/glyphs") => {
            let text = query_param(&query, "text");
            json_ok(&mut stream, &glyphs_json(&text));
        }
        ("POST", "/api/quit") => {
            json_ok(&mut stream, "{\"ok\":true,\"msg\":\"正在退出\"}");
            std::thread::spawn(|| {
                std::thread::sleep(Duration::from_millis(150));
                let sh = shared();
                sh.stop.store(true, Ordering::SeqCst);
                sh.generation.fetch_add(1, Ordering::SeqCst);
                std::process::exit(0);
            });
        }
        ("GET", "/api/events") => {
            let _ = stream.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream; charset=utf-8\r\n\
                  Cache-Control: no-cache, no-store\r\nConnection: keep-alive\r\n\
                  X-Accel-Buffering: no\r\n\r\n",
            );
            let sh = shared();
            let mut last = last_event_id;
            let mut last_ping = Instant::now();
            loop {
                let batch: Vec<Event> = {
                    let ev = sh.events.lock().unwrap();
                    ev.iter().filter(|e| e.id > last).cloned().collect()
                };
                for e in &batch {
                    last = e.id;
                    if write!(stream, "id: {}\ndata: {}\n\n", e.id, event_json(e)).is_err() {
                        return;
                    }
                }
                if last_ping.elapsed() >= Duration::from_secs(5) {
                    if stream.write_all(b": ping\n\n").is_err() {
                        return;
                    }
                    last_ping = Instant::now();
                }
                if stream.flush().is_err() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(60));
            }
        }
        _ => respond(&mut stream, "404 Not Found", "text/plain", b"not found"),
    }
}

// ---------------------------------------------------------------- 入口

/// 写控制台永不 panic：从启动器（无控制台）拉起时，标准输出可能是无效句柄，
/// 而 `println!` 在写失败时会 panic —— 那就变成"双击后什么都不发生"。
macro_rules! say {
    ($($a:tt)*) => {{
        use std::io::Write as _;
        let _ = writeln!(std::io::stdout(), $($a)*);
    }};
}

pub fn serve(port: u16, open: bool) -> i32 {
    if port == 0 {
        // 让系统给一个空闲端口（启动器用）
        let l = TcpListener::bind(("127.0.0.1", 0)).ok();
        let p = l.and_then(|l| l.local_addr().ok()).map(|a| a.port()).unwrap_or(8770);
        return serve(p, open);
    }
    let listener = match TcpListener::bind(("127.0.0.1", port)) {
        Ok(l) => l,
        Err(e) => {
            say!("端口 {port} 监听失败：{e}");
            return 2;
        }
    };
    let url = format!("http://127.0.0.1:{port}/");
    say!("CP16+ 已启动：{url}");
    say!("  · 左边选输入设备，点「开始接收」就能实时看瀑布");
    say!("  · 也可以把 WAV 拖进窗口回放（同一套逐行渲染）");
    say!("  · 发信页可以生成 WAV，或直接在本机做发→收自检");
    say!("按 Ctrl+C 退出。");
    if open {
        match crate::browser::open_ui(&url) {
            crate::browser::Launch::Window(mut child) => {
                // 应用窗口关掉 = 退出程序（这正是"双击即用"的预期）
                std::thread::spawn(move || {
                    let _ = child.wait();
                    std::process::exit(0);
                });
            }
            crate::browser::Launch::HandedOff => {
                say!("  （已交给正在运行的浏览器实例显示）");
            }
            crate::browser::Launch::NoBrowser => {
                say!("  （没找到 Edge/Chrome 或启动失败，已用默认浏览器打开）");
            }
        }
    }
    for s in listener.incoming().flatten() {
        std::thread::spawn(move || handle(s));
    }
    0
}

// 供其他模块复用：把解码结果渲染成 PNG（暂时保留视觉层工具链入口）。
#[allow(dead_code)]
pub fn render_blocks(res: &rx::RxResult, px: usize) -> (usize, usize, Vec<u8>) {
    let _ = (res, px, vision::row_starts(0.0, 1, 1.0));
    (1, 1, vec![0, 0, 0])
}
