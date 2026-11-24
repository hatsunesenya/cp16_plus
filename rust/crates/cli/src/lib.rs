//! CP16+ 应用层库：内置 HTTP 服务 + 浏览器应用窗口。
//!
//! 界面走"本地服务 + 浏览器应用窗口"，所以命令行工具和双击启动器共用同一套引擎。

pub mod browser;
pub mod fft;
pub mod server;
