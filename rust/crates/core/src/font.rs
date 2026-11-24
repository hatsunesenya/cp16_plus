//! CPF 字模容器读取（按 Unicode 码点索引），供 Rust 端排版使用。
//!
//! 格式见 `ref/cp16plus/font.py::save_cpf`：
//! `CPF1 | ver u8 | n u8 | count u32 | flags u16 | count×(code u16, offset u32) | 点阵`

use std::collections::HashMap;
use std::fs;
use std::path::Path;

pub struct CpfFont {
    pub n: usize,
    stride: usize,
    map: HashMap<u16, usize>,
    data: Vec<u8>,
}

impl CpfFont {
    pub fn load(path: &Path) -> Result<Self, String> {
        let raw = fs::read(path).map_err(|e| format!("read {path:?}: {e}"))?;
        Self::load_bytes(raw)
    }

    /// 从内存加载（字库可以编进 exe，发行时不用带外部文件）。
    pub fn load_bytes(raw: Vec<u8>) -> Result<Self, String> {
        if raw.len() < 12 || &raw[0..4] != b"CPF1" {
            return Err("不是 CPF 字库".into());
        }
        let n = raw[5] as usize;
        let count = u32::from_le_bytes([raw[6], raw[7], raw[8], raw[9]]) as usize;
        let mut map = HashMap::with_capacity(count);
        for i in 0..count {
            let at = 12 + i * 6;
            if at + 6 > raw.len() {
                break;
            }
            let code = u16::from_le_bytes([raw[at], raw[at + 1]]);
            map.insert(code, i);
        }
        let stride = (n * n + 7) / 8;
        Ok(CpfFont { n, stride, map, data: raw })
    }

    /// 取一个字的点阵（n×n，每字节 0/1）。
    pub fn raster(&self, ch: char) -> Option<Vec<u8>> {
        let code = ch as u32;
        if code > 0xFFFF {
            return None;
        }
        let idx = *self.map.get(&(code as u16))?;
        let at = 12 + idx * 6;
        let off = u32::from_le_bytes([self.data[at + 2], self.data[at + 3],
                                      self.data[at + 4], self.data[at + 5]]) as usize;
        if off + self.stride > self.data.len() {
            return None;
        }
        let mut out = Vec::with_capacity(self.n * self.n);
        for k in 0..self.n * self.n {
            let byte = self.data[off + k / 8];
            out.push((byte >> (7 - (k % 8))) & 1);
        }
        Some(out)
    }

    /// 把一段文本排成点阵列表，跳过字库外的字符。
    pub fn text(&self, s: &str) -> (Vec<Vec<u8>>, String) {
        let mut glyphs = Vec::new();
        let mut kept = String::new();
        for ch in s.chars() {
            if let Some(g) = self.raster(ch) {
                glyphs.push(g);
                kept.push(ch);
            }
        }
        (glyphs, kept)
    }
}
