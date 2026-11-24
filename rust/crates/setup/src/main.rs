//! CP16+ 安装包（自解压，单文件）。
//!
//! 为什么自己写而不用 Inno Setup / MSI：
//!   1) 这机器上没有 Inno Setup，装一套编译器比这个安装器本身还重；
//!   2) 我们要的只是"把 4 个文件放到一个目录 + 建两个快捷方式 + 写一条卸载项"，
//!      Inno 的脚本能力和我们要的复杂度不成比例；
//!   3) 自解压后仍然零依赖：安装器本身没有第三方 crate，产物约 150 KB。
//!
//! 打包方式（build.ps1 -Pack 里做）：把本 exe 复制一份，尾部追加
//!   [records][u64 payload_len][b"CP16PAY1"]
//! records = 若干条 (u32 名字长度, 名字 UTF-8, u64 长度, 内容)。
//! PE 允许尾部有多余字节，所以追加后它照样是个能双击的 exe。
//!
//! 行为：
//!   默认            安装到 %LOCALAPPDATA%\Programs\CP16Plus（免管理员），建快捷方式，启动程序
//!   --dir <路径>     指定安装目录
//!   --silent         不弹任何提示框
//!   --no-shortcut    不建快捷方式（测试用）
//!   --no-launch      装完不自动启动
//!   --uninstall      卸载（会先把自己复制到 TEMP 再删安装目录）

#![windows_subsystem = "windows"]

use std::ffi::OsStr;
use std::fs;
use std::io::Write;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

const MAGIC: &[u8; 8] = b"CP16PAY1";
const APP_NAME: &str = "CP16+ B0.1.5";
/// 给用户看的版本号（协议与软件分开标）
const PROTO_VER: &str = "V1";
const APP_VER: &str = "β0.1.5";

/// 首次展示的提示（用户提供的原文）
const NOTICE: &str = include_str!("../assets/notice.txt");
/// 需要用户同意才能继续的许可全文
const LICENSE: &str = include_str!("../assets/LICENSE-MIT.txt");
const LAUNCHER: &str = "CP16+ B0.1.5.exe";
const UNINSTALLER: &str = "卸载 CP16+ B0.1.5.exe";
const REG_SUBKEY: &str =
    r"Software\Microsoft\Windows\CurrentVersion\Uninstall\CP16Plus";
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

const MB_YESNO: u32 = 0x04;
const MB_OK: u32 = 0x00;
const MB_ICONINFO: u32 = 0x40;
const MB_ICONERR: u32 = 0x10;
/// 默认按钮 1（回车 = 第一个按钮）
const MB_DEFBUTTON1: u32 = 0x0000;
const IDYES: i32 = 6;

#[link(name = "user32")]
extern "system" {
    fn MessageBoxW(hwnd: isize, text: *const u16, caption: *const u16, ty: u32) -> i32;
}

// 注册表直接走 advapi32：不依赖 PATH 里的 reg.exe，也没有命令行转义问题。
#[link(name = "advapi32")]
extern "system" {
    fn RegCreateKeyExW(
        hkey: isize,
        sub: *const u16,
        reserved: u32,
        class: *const u16,
        options: u32,
        sam: u32,
        sa: *const core::ffi::c_void,
        out: *mut isize,
        disp: *mut u32,
    ) -> i32;
    fn RegSetValueExW(
        hkey: isize,
        name: *const u16,
        reserved: u32,
        ty: u32,
        data: *const u8,
        len: u32,
    ) -> i32;
    fn RegOpenKeyExW(
        hkey: isize,
        sub: *const u16,
        options: u32,
        sam: u32,
        out: *mut isize,
    ) -> i32;
    fn RegQueryValueExW(
        hkey: isize,
        name: *const u16,
        reserved: *mut u32,
        ty: *mut u32,
        data: *mut u8,
        len: *mut u32,
    ) -> i32;
    fn RegCloseKey(hkey: isize) -> i32;
    fn RegDeleteTreeW(hkey: isize, sub: *const u16) -> i32;
}

const HKEY_CURRENT_USER: isize = 0x8000_0001u32 as isize;
const HKEY_LOCAL_MACHINE: isize = 0x8000_0002u32 as isize;

/// 管理员装机器级（HKLM），否则落到当前用户（HKCU）
fn reg_hive() -> isize {
    if is_admin() {
        HKEY_LOCAL_MACHINE
    } else {
        HKEY_CURRENT_USER
    }
}
const KEY_WRITE: u32 = 0x2_0006; // KEY_SET_VALUE | KEY_CREATE_SUB_KEY | KEY_QUERY_VALUE
const KEY_READ: u32 = 0x2_0019;
const REG_SZ: u32 = 1;
const REG_DWORD: u32 = 4;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn msgbox(text: &str, ty: u32) -> i32 {
    let t = wide(text);
    let c = wide(APP_NAME);
    unsafe { MessageBoxW(0, t.as_ptr(), c.as_ptr(), ty) }
}

fn quiet() -> bool {
    std::env::args().any(|a| a == "--silent")
}

fn arg_value(key: &str) -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    for i in 0..args.len() {
        if args[i] == key {
            return args.get(i + 1).cloned();
        }
    }
    None
}

fn has_flag(flag: &str) -> bool {
    std::env::args().any(|a| a == flag)
}

/// 是否已经在管理员上下文里
fn is_admin() -> bool {
    #[link(name = "shell32")]
    extern "system" {
        fn IsUserAnAdmin() -> i32;
    }
    unsafe { IsUserAnAdmin() != 0 }
}

/// 以管理员身份重新启动自己（UAC 弹窗）。成功返回 true（本进程应退出）。
fn relaunch_as_admin() -> bool {
    use std::os::windows::ffi::OsStrExt;
    #[link(name = "shell32")]
    extern "system" {
        fn ShellExecuteW(
            hwnd: isize,
            op: *const u16,
            file: *const u16,
            params: *const u16,
            dir: *const u16,
            show: i32,
        ) -> isize;
    }
    fn w(s: &str) -> Vec<u16> {
        std::ffi::OsStr::new(s).encode_wide().chain(std::iter::once(0)).collect()
    }
    let exe = exe_path();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let params = args
        .iter()
        .map(|a| if a.contains(' ') { format!("\"{a}\"") } else { a.clone() })
        .collect::<Vec<_>>()
        .join(" ");
    let op = w("runas");
    let file = w(&exe.display().to_string());
    let par = w(&params);
    let dir = w(&exe.parent().map(|p| p.display().to_string()).unwrap_or_default());
    let rc = unsafe { ShellExecuteW(0, op.as_ptr(), file.as_ptr(), par.as_ptr(), dir.as_ptr(), 1) };
    // >32 表示成功；<=32 通常是用户点了"否"（ERROR_CANCELLED=1223 会变成 5/2 等）
    log(&format!("relaunch_as_admin -> {rc}"));
    rc > 32
}

// ---------------------------------------------------------------- 原生文本窗口
//
// 为什么不用 PowerShell：`powershell -EncodedCommand <base64>` 是杀毒软件眼里
// 最典型的可疑特征之一（编码命令行＝加载器行为），会让整个安装包被误判。
// 这里用 user32 直接建一个只读多行 EDIT + 两个按钮：零子进程、零脚本。

use std::sync::atomic::AtomicBool;

static DLG_ACCEPT: AtomicBool = AtomicBool::new(false);

#[repr(C)]
struct WndClassW {
    style: u32,
    wnd_proc: Option<unsafe extern "system" fn(isize, u32, usize, isize) -> isize>,
    cls_extra: i32,
    wnd_extra: i32,
    instance: isize,
    icon: isize,
    cursor: isize,
    background: isize,
    menu_name: *const u16,
    class_name: *const u16,
}

#[repr(C)]
struct Msg {
    hwnd: isize,
    message: u32,
    wparam: usize,
    lparam: isize,
    time: u32,
    pt_x: i32,
    pt_y: i32,
    private: u32,
}

#[repr(C)]
struct Rect {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

unsafe extern "system" fn dlg_proc(hwnd: isize, msg: u32, wp: usize, lp: isize) -> isize {
    use std::sync::atomic::Ordering;
    const WM_COMMAND: u32 = 0x0111;
    const WM_CLOSE: u32 = 0x0010;
    const WM_DESTROY: u32 = 0x0002;
    match msg {
        WM_COMMAND => {
            match (wp & 0xFFFF) as i32 {
                1 => DLG_ACCEPT.store(true, Ordering::SeqCst),
                2 => DLG_ACCEPT.store(false, Ordering::SeqCst),
                _ => return 0,
            }
            DestroyWindow(hwnd);
            0
        }
        WM_CLOSE => {
            DLG_ACCEPT.store(false, Ordering::SeqCst);
            DestroyWindow(hwnd);
            0
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

#[link(name = "user32")]
extern "system" {
    fn RegisterClassW(cls: *const WndClassW) -> u16;
    fn CreateWindowExW(
        ex: u32,
        class: *const u16,
        name: *const u16,
        style: u32,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        parent: isize,
        menu: isize,
        inst: isize,
        param: *const core::ffi::c_void,
    ) -> isize;
    fn DefWindowProcW(hwnd: isize, msg: u32, wp: usize, lp: isize) -> isize;
    fn DestroyWindow(hwnd: isize) -> i32;
    fn ShowWindow(hwnd: isize, cmd: i32) -> i32;
    fn UpdateWindow(hwnd: isize) -> i32;
    fn GetMessageW(msg: *mut Msg, hwnd: isize, min: u32, max: u32) -> i32;
    fn TranslateMessage(msg: *const Msg) -> i32;
    fn DispatchMessageW(msg: *const Msg) -> isize;
    fn PostQuitMessage(code: i32);
    fn SetWindowTextW(hwnd: isize, text: *const u16) -> i32;
    fn GetSystemMetrics(idx: i32) -> i32;
    fn GetClientRect(hwnd: isize, rc: *mut Rect) -> i32;
    fn SetForegroundWindow(hwnd: isize) -> i32;
}

#[link(name = "gdi32")]
extern "system" {
    fn GetStockObject(idx: i32) -> isize;
}

/// 原生"正文 + 同意/不同意"窗口；返回 true 表示点了同意。
fn ps_text_dialog(title: &str, body: &str, ok_text: &str, cancel_text: &str) -> bool {
    use std::sync::atomic::Ordering;
    DLG_ACCEPT.store(false, Ordering::SeqCst);
    unsafe {
        let class = wide("CP16PlusTextDlg");
        let wc = WndClassW {
            style: 0x0003,
            wnd_proc: Some(dlg_proc),
            cls_extra: 0,
            wnd_extra: 0,
            instance: 0,
            icon: 0,
            cursor: 0,
            background: 6,
            menu_name: std::ptr::null(),
            class_name: class.as_ptr(),
        };
        RegisterClassW(&wc);

        let sw = GetSystemMetrics(0);
        let sh = GetSystemMetrics(1);
        let w = 880.min((sw - 100).max(520));
        let h = 640.min((sh - 100).max(400));
        let hwnd = CreateWindowExW(
            0x0004_0000,
            class.as_ptr(),
            wide(title).as_ptr(),
            0x00CF_0000,
            (sw - w) / 2,
            (sh - h) / 2,
            w,
            h,
            0,
            0,
            0,
            std::ptr::null(),
        );
        if hwnd == 0 {
            return false;
        }
        let mut rc = Rect { left: 0, top: 0, right: w, bottom: h };
        GetClientRect(hwnd, &mut rc);
        let cw = rc.right;
        let ch = rc.bottom;
        let pad = 16i32;
        let bh = 34i32;
        let font = GetStockObject(17);

        let edit = CreateWindowExW(
            0x0000_0200,
            wide("EDIT").as_ptr(),
            std::ptr::null(),
            0x5020_0844,
            pad,
            pad,
            cw - 2 * pad,
            ch - 3 * pad - bh,
            hwnd,
            0,
            0,
            std::ptr::null(),
        );
        SendMessageW(edit, 0x0030, font as usize, 1);
        SetWindowTextW(edit, wide(body).as_ptr());

        let ok = CreateWindowExW(
            0,
            wide("BUTTON").as_ptr(),
            wide(ok_text).as_ptr(),
            0x5000_0001,
            cw - 2 * pad - 330,
            ch - pad - bh,
            190,
            bh,
            hwnd,
            1,
            0,
            std::ptr::null(),
        );
        let cancel = CreateWindowExW(
            0,
            wide("BUTTON").as_ptr(),
            wide(cancel_text).as_ptr(),
            0x5000_0000,
            cw - pad - 130,
            ch - pad - bh,
            130,
            bh,
            hwnd,
            2,
            0,
            std::ptr::null(),
        );
        SendMessageW(ok, 0x0030, font as usize, 1);
        SendMessageW(cancel, 0x0030, font as usize, 1);

        ShowWindow(hwnd, 1);
        UpdateWindow(hwnd);
        SetForegroundWindow(hwnd);

        let mut msg: Msg = std::mem::zeroed();
        while GetMessageW(&mut msg, 0, 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    DLG_ACCEPT.load(Ordering::SeqCst)
}

fn exe_path() -> PathBuf {
    std::env::current_exe().unwrap_or_else(|_| PathBuf::from("cp16plus-setup.exe"))
}

/// 文件夹安装模式：把"安装程序所在目录"里的发行文件当作安装内容。
/// 跳过安装器自身、卸载器和已经存在的目标端生成物。
fn payload_from_dir() -> Option<Vec<(String, Vec<u8>)>> {
    let dir = exe_path().parent()?.to_path_buf();
    let me = exe_path();
    let mut out: Vec<(String, Vec<u8>)> = Vec::new();
    let entries = fs::read_dir(&dir).ok()?;
    for e in entries.flatten() {
        let p = e.path();
        if !p.is_file() || p == me {
            continue;
        }
        let name = p.file_name()?.to_string_lossy().to_string();
        if name.starts_with("卸载") || name.ends_with(".lnk") {
            continue;
        }
        let lower = name.to_lowercase();
        let keep = lower.ends_with(".exe")
            || lower.ends_with(".dll")
            || lower.ends_with(".ico")
            || lower.ends_with(".md")
            || lower.ends_with(".ps1");
        if !keep {
            continue;
        }
        if let Ok(bytes) = fs::read(&p) {
            out.push((name, bytes));
        }
    }
    // 例子目录也带上（存在才带）
    let ex_dir = dir.join("examples");
    if let Ok(it) = fs::read_dir(&ex_dir) {
        for e in it.flatten() {
            let p = e.path();
            if p.is_file() {
                if let (Some(n), Ok(bytes)) = (p.file_name(), fs::read(&p)) {
                    out.push((format!("examples/{}", n.to_string_lossy()), bytes));
                }
            }
        }
    }
    Some(out)
}

/// 默认装到 `%ProgramFiles%\CP16Plus`（安装需要管理员权限，所以走机器级目录）。
fn default_dir() -> PathBuf {
    let base = std::env::var("ProgramFiles")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(r"C:\Program Files"));
    base.join("CP16Plus")
}

// ---------------------------------------------------------------- 内嵌载荷

/// 返回 (载荷起始偏移, 记录表)。没有载荷就返回 None。
fn read_payload() -> Option<(usize, Vec<(String, Vec<u8>)>)> {
    let bytes = fs::read(exe_path()).ok()?;
    if bytes.len() < 16 {
        return None;
    }
    let tail = &bytes[bytes.len() - 16..];
    if &tail[8..16] != MAGIC {
        return None;
    }
    let len = u64::from_le_bytes(tail[0..8].try_into().ok()?) as usize;
    if len + 16 > bytes.len() {
        return None;
    }
    let start = bytes.len() - 16 - len;
    let body = &bytes[start..start + len];

    let mut out = Vec::new();
    let mut at = 0usize;
    while at + 4 <= body.len() {
        let nlen = u32::from_le_bytes(body[at..at + 4].try_into().ok()?) as usize;
        at += 4;
        if at + nlen + 8 > body.len() {
            return None;
        }
        let name = String::from_utf8_lossy(&body[at..at + nlen]).to_string();
        at += nlen;
        let size = u64::from_le_bytes(body[at..at + 8].try_into().ok()?) as usize;
        at += 8;
        if at + size > body.len() {
            return None;
        }
        out.push((name, body[at..at + size].to_vec()));
        at += size;
    }
    Some((start, out))
}

/// 安装器自身（去掉尾部载荷）的字节：用来在安装目录里放一个干净的卸载器。
fn self_without_payload() -> Vec<u8> {
    let bytes = fs::read(exe_path()).unwrap_or_default();
    match read_payload() {
        Some((start, _)) if start <= bytes.len() => bytes[..start].to_vec(),
        _ => bytes,
    }
}

// ---------------------------------------------------------------- 安装

/// 安装日志：装不上时让用户能发一个文件过来，而不是"点了没反应"。
fn log_path() -> PathBuf {
    let candidates = [
        std::env::var("TEMP").map(PathBuf::from).ok(),
        exe_path().parent().map(|p| p.to_path_buf()),
    ];
    for c in candidates.into_iter().flatten() {
        let probe = c.join(".cp16-log-probe");
        if fs::write(&probe, b"1").is_ok() {
            let _ = fs::remove_file(&probe);
            return c.join("cp16plus-setup.log");
        }
    }
    std::env::temp_dir().join("cp16plus-setup.log")
}

fn log(msg: &str) {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if let Ok(mut f) = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path())
    {
        let _ = writeln!(f, "[{stamp}] {msg}");
    }
}

fn run(cmd: &str, args: &[&OsStr]) -> bool {
    let out = Command::new(cmd)
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .output();
    match out {
        Ok(o) => {
            let code = o.status.code().unwrap_or(-1);
            let err = String::from_utf8_lossy(&o.stderr);
            let mut flat: Vec<String> = Vec::new();
            for a in args {
                flat.push(a.to_string_lossy().to_string());
            }
            log(&format!("run {cmd} {} -> {code} {}", flat.join(" "), err.trim()));
            o.status.success()
        }
        Err(e) => {
            log(&format!("run {cmd} failed to start: {e}"));
            false
        }
    }
}

fn reg_add(name: &str, value: &str) -> bool {
    reg_write_str(REG_SUBKEY, name, value)
}

fn reg_add_dword(name: &str, value: &str) -> bool {
    reg_write_dword(REG_SUBKEY, name, value.parse().unwrap_or(1))
}

fn reg_query_install_dir() -> Option<PathBuf> {
    for hive in [HKEY_LOCAL_MACHINE, HKEY_CURRENT_USER] {
        let key = wide(REG_SUBKEY);
        let mut hkey: isize = 0;
        let rc = unsafe { RegOpenKeyExW(hive, key.as_ptr(), 0, KEY_READ, &mut hkey) };
        if rc != 0 {
            continue;
        }
        let n = wide("InstallLocation");
        let mut ty: u32 = 0;
        let mut len: u32 = 0;
        let rc = unsafe {
            RegQueryValueExW(hkey, n.as_ptr(), std::ptr::null_mut(), &mut ty, std::ptr::null_mut(), &mut len)
        };
        if rc == 0 && len > 0 {
            let mut buf = vec![0u8; len as usize];
            let rc = unsafe {
                RegQueryValueExW(hkey, n.as_ptr(), std::ptr::null_mut(), &mut ty, buf.as_mut_ptr(), &mut len)
            };
            unsafe { RegCloseKey(hkey) };
            if rc == 0 {
                let units: Vec<u16> = buf
                    .chunks_exact(2)
                    .map(|c| u16::from_le_bytes([c[0], c[1]]))
                    .take_while(|c| *c != 0)
                    .collect();
                let p = String::from_utf16_lossy(&units);
                if !p.is_empty() {
                    return Some(PathBuf::from(p));
                }
            }
        } else {
            unsafe { RegCloseKey(hkey) };
        }
    }
    None
}

fn reg_write_str(sub: &str, name: &str, value: &str) -> bool {
    let mut data: Vec<u16> = value.encode_utf16().collect();
    data.push(0);
    let bytes: Vec<u8> = data.iter().flat_map(|c| c.to_le_bytes()).collect();
    reg_write_raw(sub, name, REG_SZ, &bytes)
}

fn reg_write_dword(sub: &str, name: &str, value: u32) -> bool {
    reg_write_raw(sub, name, REG_DWORD, &value.to_le_bytes())
}

fn reg_write_raw(sub: &str, name: &str, ty: u32, data: &[u8]) -> bool {
    let key = wide(sub);
    let mut hkey: isize = 0;
    let mut disp: u32 = 0;
    let rc = unsafe {
        RegCreateKeyExW(
            reg_hive(),
            key.as_ptr(),
            0,
            std::ptr::null(),
            0,
            KEY_WRITE,
            std::ptr::null(),
            &mut hkey,
            &mut disp,
        )
    };
    if rc != 0 {
        log(&format!("RegCreateKeyExW({sub}) -> {rc}"));
        return false;
    }
    let n = wide(name);
    let rc = unsafe {
        RegSetValueExW(hkey, n.as_ptr(), 0, ty, data.as_ptr(), data.len() as u32)
    };
    unsafe { RegCloseKey(hkey) };
    if rc != 0 {
        log(&format!("RegSetValueExW({name}) -> {rc}"));
    }
    rc == 0
}

fn reg_read_str(sub: &str, name: &str) -> Option<String> {
    let key = wide(sub);
    let mut hkey: isize = 0;
    let rc = unsafe { RegOpenKeyExW(reg_hive(), key.as_ptr(), 0, KEY_READ, &mut hkey) };
    if rc != 0 {
        return None;
    }
    let n = wide(name);
    let mut ty: u32 = 0;
    let mut len: u32 = 0;
    let rc = unsafe {
        RegQueryValueExW(hkey, n.as_ptr(), std::ptr::null_mut(), &mut ty, std::ptr::null_mut(), &mut len)
    };
    if rc != 0 || len == 0 {
        unsafe { RegCloseKey(hkey) };
        return None;
    }
    let mut buf = vec![0u8; len as usize];
    let rc = unsafe {
        RegQueryValueExW(hkey, n.as_ptr(), std::ptr::null_mut(), &mut ty, buf.as_mut_ptr(), &mut len)
    };
    unsafe { RegCloseKey(hkey) };
    if rc != 0 {
        return None;
    }
    let units: Vec<u16> = buf
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .take_while(|c| *c != 0)
        .collect();
    Some(String::from_utf16_lossy(&units))
}

fn reg_delete_tree() -> bool {
    let key = wide(REG_SUBKEY);
    let mut rc = unsafe { RegDeleteTreeW(HKEY_LOCAL_MACHINE, key.as_ptr()) };
    if rc != 0 {
        rc = unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, key.as_ptr()) };
    }
    log(&format!("RegDeleteTreeW -> {rc}"));
    rc == 0 || rc == 2 // 2 = ERROR_FILE_NOT_FOUND（本来就没装）
}


// ---------------------------------------------------------------- 已知文件夹 / 文件夹选择器
//
// 为什么不用环境变量拼路径：桌面经常被 OneDrive 重定向到
// `%USERPROFILE%\OneDrive\Desktop`（或"桌面"），按 `%USERPROFILE%\Desktop` 建出来的
// 快捷方式用户根本看不见——这就是"装完桌面找不到图标"的主因。
// 正确做法是问系统要 Known Folder。

#[repr(C)]
struct Guid {
    d1: u32,
    d2: u16,
    d3: u16,
    d4: [u8; 8],
}

const FOLDERID_DESKTOP: Guid = Guid {
    d1: 0xB4BFCC3A,
    d2: 0xDB2C,
    d3: 0x424C,
    d4: [0xB0, 0x29, 0x7F, 0xE9, 0x9A, 0x87, 0xC6, 0x41],
};
/// 每用户「开始菜单 → 程序」
const FOLDERID_PROGRAMS: Guid = Guid {
    d1: 0xA77F5D77,
    d2: 0x2E2B,
    d3: 0x44C3,
    d4: [0xA6, 0xA2, 0xAB, 0xA6, 0x01, 0x05, 0x4A, 0x51],
};
/// 所有用户「开始菜单 → 程序」（每用户写不进去时的兜底）
const FOLDERID_COMMON_PROGRAMS: Guid = Guid {
    d1: 0x0139D44E,
    d2: 0x6AFE,
    d3: 0x49F2,
    d4: [0x86, 0x90, 0x3D, 0xAF, 0xCA, 0xE6, 0xFF, 0xB8],
};
/// 所有用户的桌面
const FOLDERID_PUBLIC_DESKTOP: Guid = Guid {
    d1: 0xC4AA340D,
    d2: 0xF20F,
    d3: 0x4863,
    d4: [0xAF, 0xEF, 0xF8, 0x7E, 0xF2, 0xE6, 0xBA, 0x25],
};

#[repr(C)]
struct BrowseInfoW {
    hwnd_owner: isize,
    pidl_root: *mut core::ffi::c_void,
    display_name: *mut u16,
    title: *const u16,
    flags: u32,
    callback: Option<unsafe extern "system" fn(isize, u32, isize, isize) -> i32>,
    param: isize,
    image: i32,
}

#[link(name = "shell32")]
extern "system" {
    fn SHGetKnownFolderPath(
        rfid: *const Guid,
        flags: u32,
        token: isize,
        out: *mut *mut u16,
    ) -> i32;
    fn SHBrowseForFolderW(bi: *const BrowseInfoW) -> *mut core::ffi::c_void;
    fn SHGetPathFromIDListW(pidl: *const core::ffi::c_void, path: *mut u16) -> i32;
}

#[link(name = "ole32")]
extern "system" {
    fn CoInitializeEx(reserved: *const core::ffi::c_void, coinit: u32) -> i32;
    fn CoTaskMemFree(p: *const core::ffi::c_void);
}

#[link(name = "user32")]
extern "system" {
    fn SendMessageW(hwnd: isize, msg: u32, wparam: usize, lparam: isize) -> isize;
}

const BIF_RETURNONLYFSDIRS: u32 = 0x0001;
const BIF_NEWDIALOGSTYLE: u32 = 0x0040;
const BFFM_INITIALIZED: u32 = 1;
const BFFM_SETSELECTIONW: u32 = 0x0467;

fn known_folder(id: &Guid) -> Option<PathBuf> {
    unsafe {
        let mut p: *mut u16 = std::ptr::null_mut();
        if SHGetKnownFolderPath(id, 0, 0, &mut p) != 0 || p.is_null() {
            return None;
        }
        let s = std::ffi::CStr::from_ptr(p as *const i8)
            .to_string_lossy()
            .into_owned();
        // SHGetKnownFolderPath 返回的是 UTF-16，用 CStr 不对，这里按 UTF-16 解更稳
        let len = (0..).take_while(|i| *p.add(*i) != 0).count();
        let units = std::slice::from_raw_parts(p, len);
        let path = String::from_utf16_lossy(units);
        CoTaskMemFree(p as *const core::ffi::c_void);
        let _ = s;
        if path.is_empty() {
            None
        } else {
            Some(PathBuf::from(path))
        }
    }
}

/// 桌面：优先 Known Folder（能识别 OneDrive 重定向），退回 %USERPROFILE%\Desktop
/// 装成"所有用户"（管理员）时优先公共桌面，再退当前用户桌面
fn desktop_dirs() -> Vec<PathBuf> {
    let mut v = Vec::new();
    if is_admin() {
        if let Some(p) = known_folder(&FOLDERID_PUBLIC_DESKTOP) {
            v.push(p);
        }
    }
    if let Some(p) = desktop_dir() {
        v.push(p);
    }
    v
}

fn desktop_dir() -> Option<PathBuf> {
    if let Some(p) = known_folder(&FOLDERID_DESKTOP) {
        if p.is_dir() {
            return Some(p);
        }
    }
    for key in ["USERPROFILE", "OneDrive"] {
        if let Ok(base) = std::env::var(key) {
            for sub in ["Desktop", "桌面"] {
                let p = PathBuf::from(&base).join(sub);
                if p.is_dir() {
                    return Some(p);
                }
            }
        }
    }
    None
}

/// 开始菜单的程序目录：每用户 → 所有用户
fn start_menu_dir() -> Option<PathBuf> {
    if let Some(p) = known_folder(&FOLDERID_PROGRAMS) {
        return Some(p);
    }
    if let Ok(base) = std::env::var("APPDATA") {
        let p = PathBuf::from(base)
            .join("Microsoft")
            .join("Windows")
            .join("Start Menu")
            .join("Programs");
        if p.is_dir() {
            return Some(p);
        }
    }
    known_folder(&FOLDERID_COMMON_PROGRAMS)
}

/// 兜底用的公共开始菜单
fn common_start_menu_dir() -> Option<PathBuf> {
    known_folder(&FOLDERID_COMMON_PROGRAMS)
}

/// 系统自带的"选择文件夹"对话框（SHBrowseForFolderW）。
unsafe extern "system" fn browse_cb(hwnd: isize, msg: u32, _lp: isize, data: isize) -> i32 {
    if msg == BFFM_INITIALIZED && data != 0 {
        // data 指向以 0 结尾的宽字符串（我们传进去的初始目录）
        SendMessageW(hwnd, BFFM_SETSELECTIONW, 1, data);
    }
    0
}

fn pick_folder(initial: &Path) -> Option<PathBuf> {
    unsafe {
        // BIF_NEWDIALOGSTYLE 需要 COM 初始化
        let _ = CoInitializeEx(std::ptr::null(), 0x2); // COINIT_APARTMENTTHREADED
        let title: Vec<u16> = "选择 CP16+ 的安装目录".encode_utf16().chain(std::iter::once(0)).collect();
        let mut display = [0u16; 260];
        let init: Vec<u16> = initial.display().to_string().encode_utf16().chain(std::iter::once(0)).collect();
        let bi = BrowseInfoW {
            hwnd_owner: 0,
            pidl_root: std::ptr::null_mut(),
            display_name: display.as_mut_ptr(),
            title: title.as_ptr(),
            flags: BIF_RETURNONLYFSDIRS | BIF_NEWDIALOGSTYLE,
            callback: Some(browse_cb),
            param: init.as_ptr() as isize,
            image: 0,
        };
        let pidl = SHBrowseForFolderW(&bi);
        if pidl.is_null() {
            log("选择目录：用户取消");
            return None;
        }
        let mut buf = [0u16; 1024];
        let ok = SHGetPathFromIDListW(pidl, buf.as_mut_ptr());
        CoTaskMemFree(pidl);
        if ok == 0 {
            return None;
        }
        let len = buf.iter().position(|c| *c == 0).unwrap_or(0);
        let path = String::from_utf16_lossy(&buf[..len]);
        if path.is_empty() {
            None
        } else {
            Some(PathBuf::from(path))
        }
    }
}

/// 上次装到哪儿（下次当默认值）
fn last_dir_file() -> PathBuf {
    let base = std::env::var("LOCALAPPDATA").unwrap_or_else(|_| ".".into());
    PathBuf::from(base).join("CP16Plus").join("install_dir.txt")
}

fn last_dir() -> Option<PathBuf> {
    let p = fs::read_to_string(last_dir_file()).ok()?;
    let p = p.trim();
    if p.is_empty() {
        None
    } else {
        Some(PathBuf::from(p))
    }
}

fn remember_dir(dir: &Path) {
    let f = last_dir_file();
    if let Some(parent) = f.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(f, dir.display().to_string());
}

// ---------------------------------------------------------------- 快捷方式（COM，不经脚本）
const CLSID_SHELL_LINK: Guid = Guid { d1: 0x0002_1401, d2: 0, d3: 0, d4: [0xC0, 0, 0, 0, 0, 0, 0, 0x46] };
const IID_ISHELL_LINKW: Guid = Guid { d1: 0x0002_14F9, d2: 0, d3: 0, d4: [0xC0, 0, 0, 0, 0, 0, 0, 0x46] };
const IID_IPERSIST_FILE: Guid = Guid { d1: 0x0000_010B, d2: 0, d3: 0, d4: [0xC0, 0, 0, 0, 0, 0, 0, 0x46] };

#[link(name = "ole32")]
extern "system" {
    fn CoUninitialize();
    fn CoCreateInstance(
        clsid: *const Guid,
        outer: *mut core::ffi::c_void,
        ctx: u32,
        iid: *const Guid,
        out: *mut *mut core::ffi::c_void,
    ) -> i32;
}

type Hr = i32;

unsafe fn vt_slot(punk: *mut core::ffi::c_void, slot: usize) -> usize {
    let vtbl = *(punk as *const *const usize);
    *vtbl.add(slot)
}

unsafe fn vt_str1(punk: *mut core::ffi::c_void, slot: usize, a: &str) -> Hr {
    let f: unsafe extern "system" fn(*mut core::ffi::c_void, *const u16) -> Hr =
        std::mem::transmute(vt_slot(punk, slot));
    f(punk, wide(a).as_ptr())
}

unsafe fn vt_str2(punk: *mut core::ffi::c_void, slot: usize, a: &str, b: i32) -> Hr {
    let f: unsafe extern "system" fn(*mut core::ffi::c_void, *const u16, i32) -> Hr =
        std::mem::transmute(vt_slot(punk, slot));
    f(punk, wide(a).as_ptr(), b)
}

unsafe fn vt_release(punk: *mut core::ffi::c_void) {
    let f: unsafe extern "system" fn(*mut core::ffi::c_void) -> u32 =
        std::mem::transmute(vt_slot(punk, 2));
    f(punk);
}

/// 用 IShellLink 建快捷方式：不装 PowerShell、不落脚本、不执行外部程序。
fn make_shortcut(link: &Path, target: &Path, workdir: &Path) -> bool {
    unsafe {
        let _ = CoInitializeEx(std::ptr::null(), 0x2);
        let mut link_obj: *mut core::ffi::c_void = std::ptr::null_mut();
        let hr = CoCreateInstance(
            &CLSID_SHELL_LINK,
            std::ptr::null_mut(),
            1,
            &IID_ISHELL_LINKW,
            &mut link_obj,
        );
        if hr != 0 || link_obj.is_null() {
            log(&format!("CoCreateInstance(ShellLink) 失败：{hr:#x}"));
            return false;
        }
        let target_s = target.display().to_string();
        let work_s = workdir.display().to_string();
        let link_s = link.display().to_string();
        vt_str1(link_obj, 20, &target_s);                            // SetPath
        vt_str1(link_obj, 9, &work_s);                               // SetWorkingDirectory
        vt_str2(link_obj, 17, &format!("{target_s},0"), 0);          // SetIconLocation
        vt_str1(link_obj, 7, "CP16+ 频谱书写（瀑布图读字）");        // SetDescription

        let mut pf: *mut core::ffi::c_void = std::ptr::null_mut();
        let qi: unsafe extern "system" fn(
            *mut core::ffi::c_void,
            *const Guid,
            *mut *mut core::ffi::c_void,
        ) -> Hr = std::mem::transmute(vt_slot(link_obj, 0));
        let hr = qi(link_obj, &IID_IPERSIST_FILE, &mut pf);
        let mut ok = false;
        if hr == 0 && !pf.is_null() {
            let save: unsafe extern "system" fn(*mut core::ffi::c_void, *const u16, i32) -> Hr =
                std::mem::transmute(vt_slot(pf, 6));
            ok = save(pf, wide(&link_s).as_ptr(), 1) == 0;
            vt_release(pf);
        } else {
            log(&format!("QueryInterface(IPersistFile) 失败：{hr:#x}"));
        }
        vt_release(link_obj);
        CoUninitialize();
        let exists = link.is_file();
        log(&format!("shortcut {} -> ok={ok} exists={exists}", link.display()));
        exists
    }
}

/// 两条路都试：桌面（已知文件夹）与开始菜单（每用户 → 所有用户）
fn install_shortcuts(dir: &Path) -> (bool, bool) {
    let launcher = dir.join(LAUNCHER);
    let mut desktop_ok = false;
    let mut menu_ok = false;
    let desktops = desktop_dirs();
    if desktops.is_empty() {
        log("找不到桌面目录（已知文件夹与环境变量都失败）");
    }
    for dt in desktops {
        if desktop_ok {
            break;
        }
        if !dt.is_dir() {
            log(&format!("桌面目录不存在：{}", dt.display()));
            continue;
        }
        desktop_ok = make_shortcut(&dt.join(format!("{APP_NAME}.lnk")), &launcher, dir);
        if desktop_ok {
            log(&format!("桌面快捷方式：{}", dt.display()));
        }
    }
    let link = format!("{APP_NAME}.lnk");
    let menus = if is_admin() {
        [("所有用户", common_start_menu_dir()), ("每用户", start_menu_dir())]
    } else {
        [("每用户", start_menu_dir()), ("所有用户", common_start_menu_dir())]
    };
    for (tag, sm) in menus {
        let Some(sm) = sm else { continue };
        if menu_ok {
            break;
        }
        if fs::create_dir_all(&sm).is_err() {
            log(&format!("建不了开始菜单目录（{tag}）：{}", sm.display()));
            continue;
        }
        menu_ok = make_shortcut(&sm.join(&link), &launcher, dir);
        if menu_ok {
            log(&format!("开始菜单快捷方式（{tag}）：{}", sm.display()));
        }
    }
    (desktop_ok, menu_ok)
}

/// 选安装目录：
///   · `--dir` 或 `--silent` 直接用它/默认值；
///   · 否则弹一个 yes/no：默认目录 or 自己挑（系统文件夹对话框）。
fn choose_dir() -> Option<PathBuf> {
    if let Some(d) = arg_value("--dir") {
        return Some(PathBuf::from(d));
    }
    let suggested = last_dir().unwrap_or_else(default_dir);
    if quiet() {
        return Some(suggested);
    }
    let text = format!(
        "安装到默认位置吗？\n\n{}\n\n点「是」直接装；点「否」可以自己选安装目录。",
        suggested.display()
    );
    if msgbox(&text, MB_YESNO | MB_ICONINFO | MB_DEFBUTTON1) == IDYES {
        return Some(suggested);
    }
    // 自己挑：以刚才那个目录为起点，选中的目录就当作安装目录本身
    let picked = pick_folder(&suggested)?;
    Some(picked)
}

/// 首次提示页：说明 + 版本号 + 需要管理员权限
fn notice_body() -> String {
    format!(
        "{notice}\n\n\
         ─────────────────────────────\n\
         CP16+ 协议版本：{proto}\n\
         软件版本：{app}\n\
         许可：MIT（随包附带的 Hamlib 为 LGPL-2.1，见安装目录 COPYING.LIB-hamlib.txt）\n\
         安装需要管理员权限（会弹出 Windows 的 UAC 提示）\n\
         默认安装位置：{dir}（下一步可以自己选）\n\n\
         点击「同意并继续」进入许可阅读；点「不同意」退出安装。\n",
        notice = NOTICE.trim(),
        proto = PROTO_VER,
        app = APP_VER,
        dir = default_dir().display()
    )
}

fn show_notice_page() -> bool {
    let body = notice_body();
    ps_text_dialog(
        &format!("CP16+ 频谱书写  安装程序 · 协议 {PROTO_VER} · 软件 {APP_VER}"),
        &body,
        "同意并继续",
        "不同意，退出",
    )
}

/// 许可页：MIT 全文（含 Hamlib 的 LGPL 说明）
fn show_license_page() -> bool {
    ps_text_dialog(
        "许可协议（MIT）",
        LICENSE,
        "我已阅读并同意",
        "不同意，退出",
    )
}

fn do_install() {
    // 装机需要管理员：没提权就重新拉一次自己（UAC）
    if !is_admin() && !has_flag("--no-elevate") {
        log("需要管理员权限，尝试提权");
        if relaunch_as_admin() {
            return; // 交给提权后的实例
        }
        if !quiet() {
            msgbox(
                "安装需要管理员权限。\n请在 UAC 提示里点「是」，或右键安装包选择「以管理员身份运行」。",
                MB_OK | MB_ICONERR,
            );
        }
        return;
    }

    if !quiet() {
        if !show_notice_page() {
            log("用户在提示页选择退出");
            return;
        }
        if !show_license_page() {
            log("用户在许可页选择退出");
            return;
        }
    }

    let Some(chosen) = choose_dir() else {
        log("用户取消选择安装目录");
        return;
    };
    let dir = chosen;
    // 优先用自带载荷；没有载荷就用"同目录下的文件"（文件夹安装模式，
    // 不含自解压结构，杀软对这类安装器误报明显更少）
    let files = match read_payload() {
        Some((_, f)) => f,
        None => match payload_from_dir() {
            Some(f) if !f.is_empty() => f,
            _ => {
                if !quiet() {
                    msgbox(
                        "找不到安装内容。\n\n请把本程序放在发行包目录里（与 cp16plus.exe 同目录）运行，\n\
                         或使用自带载荷的安装包。",
                        MB_OK | MB_ICONERR,
                    );
                }
                return;
            }
        },
    };
    if !quiet() {
        let text = format!(
            "将把 {} 装到：\n{}\n\n（不需要管理员权限；卸载在「设置 → 应用」里）\n\n继续安装吗？",
            APP_NAME,
            dir.display()
        );
        if msgbox(&text, MB_YESNO | MB_ICONINFO) != IDYES {
            return;
        }
    }

    if let Err(e) = fs::create_dir_all(&dir) {
        msgbox(&format!("建不了安装目录：\n{}\n\n{e}", dir.display()), MB_OK | MB_ICONERR);
        return;
    }
    // 装完后 InstallLocation / 快捷方式里都要绝对路径；
    // canonicalize 在 Windows 上会带上 \\?\ 前缀，去掉它，免得注册表和快捷方式里出现怪路径
    let dir = fs::canonicalize(&dir).unwrap_or(dir);
    let dir = {
        let s = dir.display().to_string();
        let trimmed = s
            .strip_prefix(r"\\?\")
            .or_else(|| s.strip_prefix(r"\\?\UNC\"))
            .map(|t| PathBuf::from(t))
            .unwrap_or_else(|| dir.clone());
        trimmed
    };
    remember_dir(&dir);
    log(&format!("安装目录：{}", dir.display()));

    // 先停掉正在运行的引擎/启动器。否则 cp16plus.exe 被占用，
    // 下面的 fs::write 会失败而被**静默跳过**——用户装完新版，界面却还是旧的。
    kill_running();
    std::thread::sleep(std::time::Duration::from_millis(350));

    let mut written = 0usize;
    let mut failed: Vec<String> = Vec::new();
    for (name, data) in &files {
        let rel = name.replace('\\', "/");
        let path = dir.join(rel.replace('/', std::path::MAIN_SEPARATOR_STR));
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if fs::write(&path, data).is_ok() {
            written += 1;
        } else {
            failed.push(path.display().to_string());
        }
    }
    if !failed.is_empty() {
        // 再杀一次 + 重试（有的机器上引擎会晚几百毫秒退出）
        log(&format!("{} 个文件写入失败，重试：{:?}", failed.len(), failed));
        kill_running();
        std::thread::sleep(std::time::Duration::from_millis(600));
        let mut still: Vec<String> = Vec::new();
        for (name, data) in &files {
            let rel = name.replace('\\', "/");
            let path = dir.join(rel.replace('/', std::path::MAIN_SEPARATOR_STR));
            if failed.iter().any(|f| f == &path.display().to_string()) {
                if fs::write(&path, data).is_ok() {
                    written += 1;
                } else {
                    still.push(path.display().to_string());
                }
            }
        }
        if !still.is_empty() {
            msgbox(
                &format!(
                    "有 {} 个文件写不进去（被占用）：\n{}\n\n请先在任务管理器里结束 CP16+ 相关进程，再重装。",
                    still.len(),
                    still.join("\n")
                ),
                MB_OK | MB_ICONERR,
            );
            return;
        }
    }
    if written == 0 {
        msgbox("一个文件都没写出去（目录不可写？）", MB_OK | MB_ICONERR);
        return;
    }

/// 结束正在运行的 CP16+ 进程（引擎 + 启动器）。
/// 不这样做，旧 exe 会被占用，安装器只能跳过它——表现就是"装了新版还是旧界面"。
fn kill_running() {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    for img in ["cp16plus.exe", "cp16plus-app.exe"] {
        let _ = std::process::Command::new("taskkill")
            .args(["/F", "/IM", img])
            .creation_flags(CREATE_NO_WINDOW)
            .output();
    }
    // 启动器的文件名带版本号，无法按固定名字杀；引擎一停，它自己也会退出
    // （启动器只是等引擎起来然后开窗，之后就不管了）。
}

    // 干净的卸载器（不带载荷，150 KB 左右）
    let uninstaller = dir.join(UNINSTALLER);
    let _ = fs::write(&uninstaller, self_without_payload());

    let mut desktop_ok = false;
    let mut menu_ok = false;
    if !has_flag("--no-shortcut") {
        let (d, m) = install_shortcuts(&dir);
        desktop_ok = d;
        menu_ok = m;
    }

    // 卸载项（HKCU：免管理员，卸载入口出现在「设置 → 应用 → 已安装的应用」）
    let launcher = dir.join(LAUNCHER);
    let reg_ok = reg_add("DisplayName", APP_NAME)
        && reg_add("DisplayVersion", env!("CARGO_PKG_VERSION"))
        && reg_add("Publisher", "CP16+ 项目")
        && reg_add("InstallLocation", &dir.display().to_string())
        && reg_add("DisplayIcon", &launcher.display().to_string())
        && reg_add(
            "UninstallString",
            &format!("\"{}\" --uninstall", uninstaller.display()),
        )
        && reg_add_dword("NoModify", "1")
        && reg_add_dword("NoRepair", "1");
    log(&format!("registry uninstall entry: {reg_ok}"));

    if !has_flag("--no-launch") && launcher.is_file() {
        let _ = Command::new(&launcher).current_dir(&dir).spawn();
    }
    if !quiet() {
        let icons = match (desktop_ok, menu_ok) {
            (true, true) => "桌面和开始菜单都有入口。".to_string(),
            (true, false) => "桌面图标已建好；开始菜单那条没建成（见日志）。".to_string(),
            (false, true) => "开始菜单里有入口；桌面图标没建成（可能桌面被重定向，见日志）。".to_string(),
            (false, false) => format!(
                "注意：两个快捷方式都没建成。\n可以直接运行：\n{}\n\
                 （日志：{}）",
                dir.join(LAUNCHER).display(),
                log_path().display()
            ),
        };
        let tail = if reg_ok {
            format!("卸载：设置 → 应用 → {APP_NAME}（需要管理员权限）。")
        } else {
            // 注册表被策略/安全软件拦住时，安装本身是好的，卸载入口换个说法
            format!(
                "注意：卸载入口没能写进注册表（可能被策略或安全软件拦了）。\n\
                 卸载直接运行这个文件即可：\n{}",
                uninstaller.display()
            )
        };
        msgbox(
            &format!("装好了：\n{}\n\n{icons}\n{tail}", dir.display()),
            MB_OK | MB_ICONINFO,
        );
    }
}

// ---------------------------------------------------------------- 卸载

fn do_uninstall() {
    let dir = arg_value("--dir")
        .map(PathBuf::from)
        .or_else(reg_query_install_dir)
        .unwrap_or_else(default_dir);
    if !quiet() {
        let text = format!("要卸载 {} 吗？\n\n安装目录：\n{}", APP_NAME, dir.display());
        if msgbox(&text, MB_YESNO | MB_ICONINFO) != IDYES {
            return;
        }
    }

    // 自己就在安装目录里时，先把副本丢到 TEMP 再跑，否则目录删不掉（exe 被占用）
    let me = exe_path();
    let inside = me.starts_with(&dir);
    if inside && !has_flag("--from-temp") {
        let tmp = std::env::temp_dir().join(format!("cp16-uninstall-{}.exe", std::process::id()));
        if fs::copy(&me, &tmp).is_ok() {
            let spawned = Command::new(&tmp)
                .args([
                    "--uninstall",
                    "--silent",
                    "--from-temp",
                    "--dir",
                    &dir.display().to_string(),
                ])
                .creation_flags(CREATE_NO_WINDOW)
                .spawn();
            if spawned.is_ok() {
                return;
            }
        }
    }

    if let Some(sm) = start_menu_dir() {
        let _ = fs::remove_file(sm.join(format!("{APP_NAME}.lnk")));
    }
    if let Some(dt) = desktop_dir() {
        let _ = fs::remove_file(dt.join(format!("{APP_NAME}.lnk")));
    }
    reg_delete_tree();

    // 删目录：等本进程退出后再删。
    //   · 用 ping 拖时间而不是 timeout：没有控制台时 `timeout` 会直接报
    //     "Input redirection is not supported"，然后 rmdir 立刻执行、目录被占用删不掉；
    //   · 必须用 raw_arg：`cmd /c` 的参数里带引号时，走普通 args 会被 Rust 的
    //     MSVC 式转义（\"）搞坏，cmd 解析出来就是"系统找不到指定的路径"。
    let script = format!("ping -n 3 127.0.0.1 >nul & rmdir /s /q \"{}\"", dir.display());
    let _ = Command::new("cmd")
        .raw_arg(format!("/c {script}"))
        .creation_flags(CREATE_NO_WINDOW)
        .spawn();

    if !quiet() {
        msgbox(&format!("已卸载，目录会在一两秒后消失：\n{}", dir.display()), MB_OK | MB_ICONINFO);
    }
}

fn main() {
    if has_flag("--uninstall") {
        do_uninstall();
    } else if has_flag("--print-dialog") {
        // 自检：把两个对话框的正文原样打出来（不开窗），核对内容与版本号
        println!("=== 提示页 ===");
        println!("{}", notice_body());
        println!("=== 许可页 ===");
        println!("{}", LICENSE);
    } else if has_flag("--test-dialog") {
        // 自检：只弹对话框，看标题/正文是否正常（安装流程用同一段代码）
        let which = arg_value("--test-dialog").unwrap_or_default();
        let ok = if which == "license" {
            show_license_page()
        } else {
            show_notice_page()
        };
        log(&format!("自检对话框 {which}: {ok}"));
        println!("dialog result: {ok}");
    } else if let Some(dir) = arg_value("--test-shortcut") {
        // 自检：在指定目录里建一个快捷方式，验证 WScript.Shell 那条链路通不通
        let dir = PathBuf::from(dir);
        let _ = fs::create_dir_all(&dir);
        let link = dir.join("shortcut-self-test.lnk");
        let me = exe_path();
        let ok = make_shortcut(&link, &me, &dir);
        log(&format!("自检快捷方式：ok={ok} link={}", link.display()));
        if !quiet() {
            msgbox(
                &format!(
                    "快捷方式自检：{}\n{}\n\n日志：{}",
                    if ok { "成功" } else { "失败" },
                    link.display(),
                    log_path().display()
                ),
                MB_OK,
            );
        }
    } else {
        do_install();
    }
}

#[allow(dead_code)]
fn write_all_or_warn(path: &Path, data: &[u8]) {
    if let Ok(mut f) = fs::File::create(path) {
        let _ = f.write_all(data);
    }
}
