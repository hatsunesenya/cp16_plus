//! 双击启动器：没有控制台、没有安装向导，直接起引擎 + 开应用窗口。
//!
//! 流程（全部在毫秒级）：
//!   1. 先找是否已有引擎在跑（同一时间只留一个引擎，第二次双击只是再开一个窗口）；
//!   2. 没有就挑一个空闲端口，静默启动 `cp16plus serve --port P --no-open`；
//!   3. 等端口能连上，再用浏览器 `--app` 模式开窗（没有地址栏的应用窗口）；
//!   4. 窗口关掉即收工：顺手把引擎也关掉，不留后台进程。
//!
//! 这样发行物只有一个目录：`CP16+ 测试版.exe`（本程序）+ `cp16plus.exe`（引擎）。

#![windows_subsystem = "windows"]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const PING: &str = "GET /api/ping HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n";

fn exe_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
}

fn engine_path() -> Option<PathBuf> {
    let dir = exe_dir();
    for name in ["cp16plus.exe", "cp16plus"] {
        let p = dir.join(name);
        if p.is_file() {
            return Some(p);
        }
        // 开发期直接从 target/release 跑也能用
        let p = dir.join("..").join(name);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

/// 端口上是不是我们的引擎（避免误连别的程序）。
///
/// 注意要读到 EOF 再看内容：HTTP 头和正文经常落在两个 TCP 段里，
/// 只 read 一次往往只看得到头，判断就会永远失败（表现为"引擎起不来"）。
fn engine_version(port: u16) -> Option<String> {
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let mut s = TcpStream::connect_timeout(&addr, Duration::from_millis(150)).ok()?;
    let _ = s.set_read_timeout(Some(Duration::from_millis(600)));
    s.write_all(PING.as_bytes()).ok()?;
    let mut out: Vec<u8> = Vec::new();
    let mut buf = [0u8; 512];
    loop {
        match s.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                out.extend_from_slice(&buf[..n]);
                if out.len() > 4096 || String::from_utf8_lossy(&out).contains('}') {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    let text = String::from_utf8_lossy(&out).to_string();
    if !text.contains("cp16plus-app") {
        return None;
    }
    let key = "\"version\":\"";
    let at = text.find(key)? + key.len();
    let rest = &text[at..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

fn is_our_engine(port: u16) -> bool {
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let Ok(mut s) = TcpStream::connect_timeout(&addr, Duration::from_millis(150)) else {
        return false;
    };
    let _ = s.set_read_timeout(Some(Duration::from_millis(600)));
    if s.write_all(PING.as_bytes()).is_err() {
        return false;
    }
    let mut out: Vec<u8> = Vec::new();
    let mut buf = [0u8; 512];
    loop {
        match s.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                out.extend_from_slice(&buf[..n]);
                if out.len() > 8192 {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    String::from_utf8_lossy(&out).contains("cp16plus-app")
}

fn free_port() -> Option<u16> {
    for p in cp16plus_core::PORT_BASE..cp16plus_core::PORT_BASE + 30 {
        if std::net::TcpListener::bind(("127.0.0.1", p)).is_ok() {
            return Some(p);
        }
    }
    None
}

fn wait_ready(port: u16, ms: u64) -> bool {
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_millis(ms) {
        if is_our_engine(port) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(60));
    }
    false
}

fn spawn_engine(exe: &PathBuf, port: u16) -> Option<Child> {
    let mut cmd = Command::new(exe);
    cmd.args(["serve", "--port", &port.to_string(), "--no-open"])
        // 关键：启动器是 GUI 子系统、没有控制台。若让子进程继承"无效的"标准句柄，
        // 引擎第一句 println! 就会写失败并 panic 退出（表现是双击后什么都不发生）。
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd.spawn().ok()
}

fn log_line(msg: &str) {
    // 优先写到 %LOCALAPPDATA%\CP16Plus\launcher.log；写不进去就退到 exe 旁边
    // （绿色/便携部署时 LOCALAPPDATA 可能不可写，静默失败会让排障变瞎）。
    let mut targets: Vec<PathBuf> = Vec::new();
    if let Ok(base) = std::env::var("LOCALAPPDATA") {
        targets.push(PathBuf::from(base).join("CP16Plus").join("launcher.log"));
    }
    targets.push(exe_dir().join("launcher.log"));
    for path in targets {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
            let _ = writeln!(f, "[{}] {}", now_str(), msg);
            return;
        }
    }
}

fn now_str() -> String {
    // 不引依赖：用系统时间戳足够定位问题
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{t}")
}

fn main() {
    log_line("launcher start");
    // 已有引擎：版本一致才复用。
    // 之前不看版本，装完新版后如果旧引擎还在跑，点快捷方式会连到旧引擎，
    // 界面就一直显示旧版本号（beta0.1.3 装不上 beta0.1.4 的表象）。
    let my_ver = env!("CARGO_PKG_VERSION");
    for p in cp16plus_core::PORT_BASE..cp16plus_core::PORT_BASE + 30 {
        if let Some(v) = engine_version(p) {
            if v != my_ver {
                log_line(&format!("port {p}: 旧引擎 v{v}（本机 {my_ver}），不复用"));
                continue;
            }
            let url = format!("http://127.0.0.1:{p}/");
            log_line(&format!("reuse engine on {p}"));
            match cp16plus_cli::browser::open_ui(&url) {
                cp16plus_cli::browser::Launch::Window(mut c) => { let _ = c.wait(); }
                cp16plus_cli::browser::Launch::HandedOff => log_line("handed off to running browser"),
                cp16plus_cli::browser::Launch::NoBrowser => log_line("no chromium for reuse"),
            }
            return;
        }
    }

    let Some(exe) = engine_path() else {
        log_line("engine exe not found");
        show_error("找不到 cp16plus.exe。请把启动器和引擎放在同一个目录里。");
        return;
    };
    let Some(port) = free_port() else {
        log_line("no free port");
        show_error("8770–8799 端口都被占用了，起不来。请关掉占用的程序再试。");
        return;
    };
    let Some(mut engine) = spawn_engine(&exe, port) else {
        log_line("spawn failed");
        show_error("引擎启动失败。");
        return;
    };
    log_line(&format!("spawned engine pid={} port={}", engine.id(), port));
    if !wait_ready(port, 8000) {
        log_line("engine not ready in 8s");
        let _ = engine.kill();
        show_error("引擎没有在 8 秒内就绪，请用命令行 `cp16plus serve` 看报错。");
        return;
    }
    let url = format!("http://127.0.0.1:{port}/");
    match cp16plus_cli::browser::open_ui(&url) {
        cp16plus_cli::browser::Launch::Window(mut child) => {
            log_line(&format!("app window pid={}", child.id()));
            let _ = child.wait();
            log_line("window closed, killing engine");
            let _ = engine.kill();
        }
        cp16plus_cli::browser::Launch::HandedOff => {
            // 窗口交给了已经在跑的浏览器实例：没有子进程可等，引擎留给界面里的「退出」收尾
            log_line("handed off to running browser instance; engine stays");
        }
        cp16plus_cli::browser::Launch::NoBrowser => {
            log_line("no app window: fell back to default browser, engine stays");
            std::thread::sleep(Duration::from_millis(300));
        }
    }
}

fn show_error(msg: &str) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let _ = Command::new("mshta")
            .arg(format!(
                "javascript:alert('{}');close()",
                msg.replace('\'', "")
            ))
            .creation_flags(CREATE_NO_WINDOW)
            .spawn();
    }
    #[cfg(not(windows))]
    eprintln!("{msg}");
}
