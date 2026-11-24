//! 协议常量与档位表（与 `ref/cp16plus/params.py` 逐项一致）。

pub const FS: usize = 8000;
pub const FS_PROD: usize = 48000;

pub const PILOT1: f64 = 450.0;
pub const PILOT2: f64 = 1250.0;

pub const CC_LO: f64 = 500.0;
pub const CC_HI: f64 = 740.0;
pub const CC_CENTER: f64 = (CC_LO + CC_HI) / 2.0; // 620
pub const CC_BAUD: f64 = 125.0;
pub const CC_RRC_ALPHA: f64 = 0.35;
pub const CC_SYM: f64 = 1.0 / CC_BAUD;

pub const SEGMENT_GAP_SYM: i64 = 20;
pub const HDR_SYMBOLS: i64 = 203; // 32 前导 + 171 数据
pub const BCON_SYMBOLS: i64 = 100;
pub const EOT_SYMBOLS: i64 = 54;
pub const CELLS_PER_SEGMENT: usize = 8;

pub const HDR_BITS: usize = 165;
pub const BCON_BITS: usize = 62;
pub const EOT_BITS: usize = 16;
pub const FRAME_MAGIC: u16 = 0x4350; // "CP"
pub const PROTO_VER: u16 = 1;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Mode {
    pub name: &'static str,
    pub n: usize,
    pub df: f64,
    pub k: u32,
    pub repeat: usize,
    pub f_lo: f64,
}

impl Mode {
    pub const fn new(name: &'static str, n: usize, df: f64, k: u32,
                     repeat: usize, f_lo: f64) -> Self {
        Mode { name, n, df, k, repeat, f_lo }
    }

    pub fn t_row(&self) -> f64 {
        self.k as f64 / self.df
    }

    pub fn rows_per_char(&self) -> usize {
        (1 + self.n) * self.repeat
    }

    /// 一行 = 5k 个 CC 符号（k/Δf ÷ 1/125 = 5k）
    pub fn row_symbols(&self) -> i64 {
        5 * self.k as i64
    }

    pub fn band(&self) -> f64 {
        self.n as f64 * self.df
    }

    pub fn f_hi(&self) -> f64 {
        self.f_lo + (self.n - 1) as f64 * self.df
    }

    pub fn f_margin(&self) -> f64 {
        self.f_lo - 2.0 * self.df
    }

    pub fn tones(&self) -> Vec<f64> {
        (0..self.n).map(|i| self.f_lo + i as f64 * self.df).collect()
    }
}

pub const M1: Mode = Mode::new("M1", 16, 25.0, 1, 1, 800.0);
pub const M2: Mode = Mode::new("M2", 16, 25.0, 2, 1, 800.0);
pub const M4: Mode = Mode::new("M4", 16, 25.0, 4, 1, 800.0);
pub const M8: Mode = Mode::new("M8", 16, 25.0, 8, 1, 800.0);
pub const M16N4: Mode = Mode::new("16N-M4", 16, 18.75, 4, 1, 640.0);
pub const M24_4: Mode = Mode::new("24-M4", 24, 25.0, 4, 1, 800.0);

pub const MODES: [Mode; 6] = [M1, M2, M4, M8, M16N4, M24_4];
pub const DEFAULT_MODE: &str = "M2";

pub fn by_name(name: &str) -> Option<Mode> {
    MODES.iter().copied().find(|m| m.name == name)
}
