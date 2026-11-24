//! CP16+ 协议核心（无第三方依赖，便于原生产品与 WASM 双编译）。
//!
//! 与 Python 参考实现一一对应，见 `docs/porting.md`。
//! 数值约定：内部一律 f64，音频出口降 f32；样点索引用 i64。

pub mod complex;
pub mod crc;
pub mod fec;
pub mod font;
pub mod frame;
pub mod modem;
pub mod params;
pub mod pilot;
pub mod png;
pub mod preambles;
pub mod rt;
pub mod rx;
pub mod tx;
pub mod vision;
pub mod wav;

pub use complex::C64;
pub use params::{Mode, MODES};

/// 诊断版标记：构建时设置环境变量 `CP16P_DIAG=1` 即为诊断版。
/// 诊断版用独立端口段、界面标注"诊断版"，避免和已安装的正式版抢同一个端口
/// （之前就是这个问题：诊断版悄悄挪到 8771，用户看的是 8770 的旧版本）。
pub const DIAG_BUILD: bool = option_env!("CP16P_DIAG").is_some();

/// 引擎端口的起始值（诊断版从 8791 起，不与正式版 8770 冲突）
pub const PORT_BASE: u16 = if DIAG_BUILD { 8791 } else { 8770 };
