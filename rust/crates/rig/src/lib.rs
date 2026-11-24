//! 电台控制（CAT）：直接链接 Hamlib 4（`libhamlib-4.dll`）。
//!
//! 为什么选这条路（用户定的路线 B-1）：
//!   · JTDX 用的就是 Hamlib 4.6，270+ 型号的协议细节已经写好了，我们不必重造；
//!   · 直接链 DLL 而不是拉起 rigctld：少一个进程、少一个 TCP 端口，
//!     调用是同步的、时序可控（PTT 提前量能精确到毫秒）；
//!   · 动态链接：DLL 是独立文件，用户可以自行替换成别的 Hamlib 构建。
//!
//! 这里只包一层我们要用到的 API：开设备、读写频率、PTT。其余（模式/带宽/表头）
//! 需要时再加。

use std::ffi::{c_char, c_int, CString};

// ---------------------------------------------------------------- FFI

#[repr(C)]
pub struct RigOpaque {
    _p: [u8; 0],
}

#[cfg(has_hamlib)]
#[link(name = "hamlib-4")]
extern "C" {
    fn rig_init(model: u32) -> *mut RigOpaque;
    fn rig_open(rig: *mut RigOpaque) -> c_int;
    fn rig_close(rig: *mut RigOpaque) -> c_int;
    fn rig_cleanup(rig: *mut RigOpaque) -> c_int;
    fn rig_set_conf(rig: *mut RigOpaque, token: c_int, val: *const c_char) -> c_int;
    fn rig_get_conf(rig: *mut RigOpaque, token: c_int, val: *mut c_char) -> c_int;
    fn rig_token_lookup(rig: *mut RigOpaque, name: *const c_char) -> c_int;
    fn rig_set_freq(rig: *mut RigOpaque, vfo: u32, freq: f64) -> c_int;
    fn rig_get_freq(rig: *mut RigOpaque, vfo: u32, freq: *mut f64) -> c_int;
    fn rig_set_ptt(rig: *mut RigOpaque, vfo: u32, ptt: c_int) -> c_int;
    fn rig_get_ptt(rig: *mut RigOpaque, vfo: u32, ptt: *mut c_int) -> c_int;
    fn rig_load_all_backends() -> c_int;
    fn rig_get_caps_cptr(model: u32, what: c_int) -> *const c_char;
    fn rig_get_caps_int(model: u32, what: c_int) -> u64;
    fn rig_get_info(rig: *mut RigOpaque) -> *const c_char;
}

/// RIG_VFO_CURR = 1<<29（见 hamlib rig.h）
const VFO_CURR: u32 = 1 << 29;
/// ptt_t：OFF=0, ON=1
const PTT_OFF: c_int = 0;
const PTT_ON: c_int = 1;
/// rig_get_caps_* 的枚举值（见 hamlib rig.h: rig_caps_cptr / rig_caps_int）
const CAPS_MFG: c_int = 1;
const CAPS_MODEL_NAME: c_int = 2;
const CAPS_RIG_MODEL: c_int = 1;

pub fn available() -> bool {
    cfg!(has_hamlib)
}

/// 读一个 hamlib 配置项（拿不到就返回空串）
#[cfg(has_hamlib)]
unsafe fn conf_get(rig: *mut RigOpaque, key: &str) -> String {
    let k = CString::new(key).unwrap();
    let token = rig_token_lookup(rig, k.as_ptr());
    let mut buf = [0 as c_char; 256];
    if rig_get_conf(rig, token, buf.as_mut_ptr()) != 0 {
        return String::new();
    }
    std::ffi::CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned()
}

/// 串口名规整：
/// · 去掉首尾空白和可能带的引号；
/// · 去掉 `\\.\` 前缀——hamlib 自己会加（它会打印 `\\.\COMx`），
///   重复前缀会打不开；
/// · 网络后端（`127.0.0.1:4532`）保持原样。
fn normalize_port(p: &str) -> String {
    let mut s = p.trim().trim_matches('"').to_string();
    if let Some(rest) = s.strip_prefix(r"\\?\") {
        s = rest.to_string();
    }
    if let Some(rest) = s.strip_prefix(r"\\.\") {
        s = rest.to_string();
    }
    s
}

/// hamlib 会把串口补成 `\\.\COMx`，比较时把前缀和大小写都归一
fn paths_equivalent(a: &str, b: &str) -> bool {
    fn norm(s: &str) -> String {
        s.trim()
            .trim_start_matches(r"\\?\")
            .trim_start_matches(r"\\.\")
            .to_ascii_uppercase()
    }
    norm(a) == norm(b)
}

/// 带"实际生效参数"的错误信息：CAT 出问题时这几个值最关键
fn err_with(code: c_int, what: &str, path: &str, speed: &str) -> String {
    let base = err(code, what);
    if path.is_empty() && speed.is_empty() {
        base
    } else {
        format!("{base}［实际串口：{path}，速率：{speed}］")
    }
}

fn err(code: c_int, what: &str) -> String {
    // hamlib 的负错误码；常见几个翻译一下，其余带原值
    let hint = match -code {
        2 => "配置无效（串口/波特率？）",
        5 => "通信超时（电台没开、线没插、波特率不对）",
        6 => "IO 错误：串口打不开（端口不存在、被别的程序占用，或驱动没装好）",
        9 => "电台拒绝了这条命令",
        11 => "这个型号不支持该功能",
        12 => "VFO 不可目标",
        16 => "VFO 非法",
        _ => "未知错误",
    };
    format!("{what} 失败：{hint}（hamlib 码 {code}）")
}

// ---------------------------------------------------------------- 配置与句柄

#[derive(Clone, Debug, Default)]
pub struct RigConfig {
    /// hamlib 型号号（见 models()）
    pub model: u32,
    /// 串口名，例如 "COM3"；网络后端可写 "127.0.0.1:4532"
    pub port: String,
    pub baud: u32,
}

pub struct Rig {
    handle: *mut RigOpaque,
    pub model: u32,
    pub port: String,
    pub baud: u32,
    /// hamlib 里实际生效的串口名（可能被 hamlib 补成 \\.\COMx 或退回默认值）
    pub path_effective: String,
    /// 实际生效的串口速率
    pub speed_effective: String,
}

// hamlib 的 RIG 句柄只在创建它的线程里用（我们就是这么用的），但为了能放进 Mutex 里
// 跨线程访问（UI 线程读频率、发射线程拉 PTT），这里声明 Send。
unsafe impl Send for Rig {}
unsafe impl Sync for Rig {}

impl Rig {
    pub fn open(cfg: &RigConfig) -> Result<Rig, String> {
        if !available() {
            return Err("没有链接 Hamlib（缺 libhamlib-4.dll）".into());
        }
        #[cfg(has_hamlib)]
        unsafe {
            rig_load_all_backends();
            let handle = rig_init(cfg.model);
            if handle.is_null() {
                return Err(format!("不认识这个型号号：{}", cfg.model));
            }
            // 注意：rig_set_conf 的第二个参数是 token_t（整数），
            // 不是字符串——这里必须先用 rig_token_lookup 换成令牌。
            // 之前把字符串指针当令牌传，set_conf 一直返回 -1 被忽略，
            // hamlib 就用默认的 COM1 去开，于是任何电台都报 -6。
            let set = |key: &str, val: &str| -> (c_int, c_int) {
                let k = CString::new(key).unwrap();
                let v = CString::new(val).unwrap();
                let token = rig_token_lookup(handle, k.as_ptr());
                let rc = rig_set_conf(handle, token, v.as_ptr());
                (token, rc)
            };
            let mut warns: Vec<String> = Vec::new();
            let wanted = normalize_port(&cfg.port);
            let looks_serial = wanted.to_ascii_uppercase().starts_with("COM");
            // 只有"看着像串口"或"host:port"才去设 rig_pathname；
            // 像 Dummy / 某些 SDR 后端根本没有端口概念，硬设会把 open 弄成 -2。
            let looks_net = wanted.contains(':');
            if !wanted.is_empty() && (looks_serial || looks_net) {
                let (tok, rc) = set("rig_pathname", &wanted);
                let eff = conf_get(handle, "rig_pathname");
                if looks_serial && !paths_equivalent(&eff, &wanted) {
                    // 串口类电台但设置没生效——这正是"永远去开 COM1"那类事故，
                    // 不能放过去，否则报错会指向一个我们根本没选的端口。
                    rig_cleanup(handle);
                    return Err(format!(
                        "串口没能设进去（token={tok}, rc={rc}，实际是 {eff:?}，想要 {wanted:?}）"
                    ));
                }
                if rc != 0 {
                    warns.push(format!("该后端不接受 rig_pathname（rc={rc}）"));
                }
            }
            if cfg.baud > 0 {
                let (_tok, rc) = set("serial_speed", &cfg.baud.to_string());
                if rc != 0 {
                    // 网络/Dummy 后端本来就没有串口速率，忽略即可
                    warns.push(format!("该后端不接受 serial_speed（rc={rc}）"));
                }
            }
            let path_effective = conf_get(handle, "rig_pathname");
            let speed_effective = conf_get(handle, "serial_speed");
            let rc = rig_open(handle);
            if rc != 0 {
                rig_cleanup(handle);
                return Err(err_with(rc, "打开电台", &path_effective, &speed_effective));
            }
            let _ = &warns;
            let path_effective = conf_get(handle, "rig_pathname");
            let speed_effective = conf_get(handle, "serial_speed");
            Ok(Rig {
                handle,
                model: cfg.model,
                port: cfg.port.clone(),
                baud: cfg.baud,
                path_effective,
                speed_effective,
            })
        }
        #[cfg(not(has_hamlib))]
        {
            let _ = cfg;
            Err("没有链接 Hamlib".into())
        }
    }

    /// 打开后探一次：能读到频率说明串口/波特率/型号都对上了。
    /// 读超时（hamlib -5）通常就是波特率不对——X6100 默认 19200。
    pub fn probe(&self) -> Result<f64, String> {
        self.read_freq()
    }

    pub fn read_freq(&self) -> Result<f64, String> {
        #[cfg(has_hamlib)]
        unsafe {
            let mut f: f64 = 0.0;
            let rc = rig_get_freq(self.handle, VFO_CURR, &mut f);
            if rc != 0 {
                return Err(err_with(rc, "读频率", &self.path_effective, &self.speed_effective));
            }
            Ok(f)
        }
        #[cfg(not(has_hamlib))]
        {
            Err("没有链接 Hamlib".into())
        }
    }

    pub fn set_freq(&self, hz: f64) -> Result<(), String> {
        #[cfg(has_hamlib)]
        unsafe {
            let rc = rig_set_freq(self.handle, VFO_CURR, hz);
            if rc != 0 {
                return Err(err_with(rc, "下发频率", &self.path_effective, &self.speed_effective));
            }
            Ok(())
        }
        #[cfg(not(has_hamlib))]
        {
            let _ = hz;
            Err("没有链接 Hamlib".into())
        }
    }

    pub fn set_ptt(&self, on: bool) -> Result<(), String> {
        #[cfg(has_hamlib)]
        unsafe {
            let rc = rig_set_ptt(self.handle, VFO_CURR, if on { PTT_ON } else { PTT_OFF });
            if rc != 0 {
                return Err(err_with(rc, if on { "拉 PTT" } else { "松 PTT" }, &self.path_effective, &self.speed_effective));
            }
            Ok(())
        }
        #[cfg(not(has_hamlib))]
        {
            let _ = on;
            Err("没有链接 Hamlib".into())
        }
    }

    pub fn read_ptt(&self) -> Result<bool, String> {
        #[cfg(has_hamlib)]
        unsafe {
            let mut p: c_int = -1;
            let rc = rig_get_ptt(self.handle, VFO_CURR, &mut p);
            if rc != 0 {
                return Err(err(rc, "读 PTT"));
            }
            Ok(p != 0)
        }
        #[cfg(not(has_hamlib))]
        {
            Err("没有链接 Hamlib".into())
        }
    }

    pub fn info(&self) -> String {
        #[cfg(has_hamlib)]
        unsafe {
            let p = rig_get_info(self.handle);
            if p.is_null() {
                String::new()
            } else {
                std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned()
            }
        }
        #[cfg(not(has_hamlib))]
        {
            String::new()
        }
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        #[cfg(has_hamlib)]
        unsafe {
            // 关软件/重连前先松 PTT：绝不能让电台挂在发射状态
            let _ = rig_set_ptt(self.handle, VFO_CURR, PTT_OFF);
            rig_close(self.handle);
            rig_cleanup(self.handle);
        }
    }
}

// ---------------------------------------------------------------- 列表

/// 所有已知型号：(型号号, 厂商, 型号名)。Hamlib 的型号号在很多区间里是稀疏的，
/// 所以直接在 0..=30000 里探一遍 `rig_get_caps_cptr`——这是纯内存查表，很快。
pub fn models() -> Vec<(u32, String, String)> {
    if !available() {
        return Vec::new();
    }
    #[cfg(has_hamlib)]
    unsafe {
        rig_load_all_backends();
        let mut out = Vec::new();
        for m in 0u32..=30000 {
            let id = rig_get_caps_int(m, CAPS_RIG_MODEL);
            if id == u64::MAX || id != m as u64 {
                continue;
            }
            let name = cstr(rig_get_caps_cptr(m, CAPS_MODEL_NAME));
            if name.is_empty() {
                continue;
            }
            let mfg = cstr(rig_get_caps_cptr(m, CAPS_MFG));
            out.push((m, mfg, name));
        }
        out
    }
    #[cfg(not(has_hamlib))]
    {
        Vec::new()
    }
}

#[cfg(has_hamlib)]
unsafe fn cstr(p: *const c_char) -> String {
    if p.is_null() {
        String::new()
    } else {
        std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned()
    }
}

/// 串口列表：读 HKLM\HARDWARE\DEVICEMAP\SERIALCOMM（这是 Windows 给所有串口登记的地方）。
pub fn serial_ports() -> Vec<String> {
    #[cfg(windows)]
    {
        use std::ffi::OsString;
        use std::os::windows::ffi::OsStringExt;
        const HKEY_LOCAL_MACHINE: isize = 0x8000_0002u32 as isize;
        const KEY_READ: u32 = 0x20019;
        extern "system" {
            fn RegOpenKeyExW(hkey: isize, sub: *const u16, opt: u32, sam: u32, out: *mut isize) -> i32;
            fn RegEnumValueW(
                hkey: isize,
                idx: u32,
                name: *mut u16,
                name_len: *mut u32,
                reserved: *mut u32,
                ty: *mut u32,
                data: *mut u8,
                data_len: *mut u32,
            ) -> i32;
            fn RegCloseKey(hkey: isize) -> i32;
        }
        fn wide(s: &str) -> Vec<u16> {
            s.encode_utf16().chain(std::iter::once(0)).collect()
        }
        let sub = wide(r"HARDWARE\DEVICEMAP\SERIALCOMM");
        let mut hkey: isize = 0;
        let rc = unsafe { RegOpenKeyExW(HKEY_LOCAL_MACHINE, sub.as_ptr(), 0, KEY_READ, &mut hkey) };
        if rc != 0 {
            return Vec::new();
        }
        let mut out = Vec::new();
        for idx in 0..64u32 {
            let mut name = [0u16; 256];
            let mut name_len = name.len() as u32;
            let mut ty: u32 = 0;
            let mut data = [0u8; 512];
            let mut data_len = data.len() as u32;
            let rc = unsafe {
                RegEnumValueW(
                    hkey,
                    idx,
                    name.as_mut_ptr(),
                    &mut name_len,
                    std::ptr::null_mut(),
                    &mut ty,
                    data.as_mut_ptr(),
                    &mut data_len,
                )
            };
            if rc != 0 {
                break;
            }
            let units: Vec<u16> = data[..data_len as usize]
                .chunks_exact(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .take_while(|c| *c != 0)
                .collect();
            let s = OsString::from_wide(&units).to_string_lossy().into_owned();
            if !s.is_empty() {
                out.push(s);
            }
        }
        unsafe { RegCloseKey(hkey) };
        out.sort();
        out.dedup();
        out
    }
    #[cfg(not(windows))]
    {
        Vec::new()
    }
}
