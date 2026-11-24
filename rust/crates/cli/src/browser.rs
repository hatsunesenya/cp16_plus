//! 把浏览器当应用外壳：优先启用 Edge/Chrome 的 `--app=` 模式，
//! 得到一个没有地址栏、没有标签页、独立任务栏图标的原生观感窗口。
//!
//! 为什么走这条路而不是 Tauri/WebView2 自嵌：
//!   1) 发行物仍是"单目录 + 一个 exe"，几百 KB，不需要 Node/CMake；
//!   2) Edge 是 Windows 10/11 自带组件，不用装运行时，也不吃我们自己包体；
//!   3) `--app` 窗口的观感与内嵌 WebView2 无法区分，但崩溃/升级由系统管。

use std::path::{Path, PathBuf};
use std::process::{Child, Command};

/// 固定的应用窗口尺寸（足够放下 16 音 × 长瀑布 + 右侧参数栏）。
pub const WIN_W: u32 = 1460;
pub const WIN_H: u32 = 940;

fn candidates() -> Vec<PathBuf> {
    let mut v = Vec::new();
    let pf = std::env::var("ProgramFiles").unwrap_or_else(|_| r"C:\Program Files".into());
    let pf86 = std::env::var("ProgramFiles(x86)")
        .unwrap_or_else(|_| r"C:\Program Files (x86)".into());
    let local = std::env::var("LOCALAPPDATA").unwrap_or_default();
    for (dir, exe) in [
        (format!(r"{pf86}\Microsoft\Edge\Application"), "msedge.exe"),
        (format!(r"{pf}\Microsoft\Edge\Application"), "msedge.exe"),
        (format!(r"{pf}\Google\Chrome\Application"), "chrome.exe"),
        (format!(r"{pf86}\Google\Chrome\Application"), "chrome.exe"),
        (format!(r"{local}\Google\Chrome\Application"), "chrome.exe"),
    ] {
        let p = Path::new(&dir).join(exe);
        if p.is_file() {
            v.push(p);
        }
    }
    v
}

/// 找到可用的 Chromium 系浏览器。
pub fn find_browser() -> Option<PathBuf> {
    candidates().into_iter().next()
}

fn writable_dir(p: &Path) -> bool {
    if std::fs::create_dir_all(p).is_err() {
        return false;
    }
    let probe = p.join(".probe");
    match std::fs::write(&probe, b"1") {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

/// 打开界面的结果。必须区分这四种情况，否则会同时开两个窗口：
///   · 窗口起来了      → 关掉它就该退引擎；
///   · 交给已有实例    → 新进程会立刻退出（把 URL 转给老进程），这**不是失败**，
///                       以前的实现把它当失败又去开默认浏览器，于是出现"应用窗口 + 浏览器页面"两个窗口；
///   · 一个浏览器都没有 → 才退回默认浏览器；
///   · 启动失败        → 也退回默认浏览器（至少有东西看）。
pub enum Launch {
    Window(Child),
    HandedOff,
    NoBrowser,
}

/// 应用窗口专用的浏览器配置目录。
/// 必须是**可写**目录，否则 Edge 会报 "Lock file can not be created" 直接退出
/// （受限策略、漫游配置、只读 %LOCALAPPDATA% 都会踩到），所以这里挨个试。
fn profile_dir() -> Option<PathBuf> {
    let mut cands: Vec<PathBuf> = Vec::new();
    // 逃生阀：目录不可写的机器上，用户/排障可以自己指定
    if let Ok(p) = std::env::var("CP16P_PROFILE") {
        cands.push(PathBuf::from(p));
    }
    if let Ok(base) = std::env::var("LOCALAPPDATA") {
        cands.push(Path::new(&base).join("CP16Plus").join("app-window"));
    }
    // 绿色部署：exe 旁边通常可写
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            cands.push(dir.join("app-window"));
        }
    }
    if let Ok(tmp) = std::env::var("TEMP") {
        cands.push(Path::new(&tmp).join("CP16Plus-app-window"));
    }
    cands.push(std::env::temp_dir().join("CP16Plus-app-window"));
    cands.into_iter().find(|p| writable_dir(p))
}

/// 用应用窗口打开 URL。
pub fn open_app_window(url: &str) -> Launch {
    let Some(browser) = find_browser() else { return Launch::NoBrowser };
    let mut cmd = Command::new(&browser);
    cmd.arg(format!("--app={url}"))
        .arg(format!("--window-size={WIN_W},{WIN_H}"))
        .arg("--no-first-run")
        .arg("--no-default-browser-check")
        .arg("--disable-features=Translate,msEdgeSidebarV2,msEdgeIdentityIntegration")
        .arg("--disable-background-mode");
    if let Some(profile) = profile_dir() {
        cmd.arg(format!("--user-data-dir={}", profile.display()));
    }
    let Ok(mut child) = cmd.spawn() else { return Launch::NoBrowser };
    // 等一会儿看它是不是"把活干完就退了"：
    //   · 还活着          → 这是我们的窗口进程；
    //   · 退出码 0 且很快 → 交给已在跑的实例了，窗口已经在屏幕上，不能再开一个；
    //   · 退出码非 0      → 真的失败（配置目录被锁等），交给调用方兜底。
    std::thread::sleep(std::time::Duration::from_millis(1200));
    match child.try_wait() {
        Ok(None) => Launch::Window(child),
        Ok(Some(st)) => {
            if st.success() {
                Launch::HandedOff
            } else {
                Launch::NoBrowser
            }
        }
        Err(_) => Launch::Window(child),
    }
}

/// 兜底：交给系统默认浏览器（有地址栏，但任何机器都能开）。
pub fn open_default_browser(url: &str) {
    let _ = Command::new("cmd").args(["/C", "start", "", url]).spawn();
}

/// 打开界面：优先应用窗口；只有"没有 Chromium"或"启动真失败"才退回默认浏览器。
pub fn open_ui(url: &str) -> Launch {
    let r = open_app_window(url);
    if let Launch::NoBrowser = r {
        open_default_browser(url);
    }
    r
}
