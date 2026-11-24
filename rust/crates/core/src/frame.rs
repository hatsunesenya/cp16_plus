//! 控制信道帧：HDR 位布局与打包（与 `ref/cp16plus/frame.py` 一致）。

use crate::crc::crc16;
use crate::params::{FRAME_MAGIC, HDR_BITS, PROTO_VER};

pub const CALL_ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789/ ";
pub const GRID_LETTERS: &[u8] = b"ABCDEFGHIJKLMNOPQR";

pub fn bits_of(value: u64, width: usize) -> Vec<u8> {
    (0..width).map(|i| ((value >> (width - 1 - i)) & 1) as u8).collect()
}

pub fn value_of(bits: &[u8]) -> u64 {
    let mut v = 0u64;
    for &b in bits {
        v = (v << 1) | (b as u64 & 1);
    }
    v
}

#[derive(Clone, Debug, PartialEq)]
pub struct Hdr {
    pub call: String,
    pub grid: String,
    pub length: u16,
    pub msg_type: u8,
    pub sf_id: u16,
    pub utc: u16,
    pub power: u8,
    pub rmap: u8,
    pub plan: u8,
    pub raster: u8,
    pub k: u32,
    pub repeat: u8,
    pub font_id: u8,
}

impl Default for Hdr {
    fn default() -> Self {
        Hdr {
            call: "BG1ABC".into(),
            grid: "OM89".into(),
            length: 0,
            msg_type: 0,
            sf_id: 0,
            utc: 0,
            power: 1,
            rmap: 0,
            plan: 0,
            raster: 16,
            k: 2,
            repeat: 1,
            font_id: 1,
        }
    }
}

fn call_bits(call: &str) -> Vec<u8> {
    let bytes = call.as_bytes();
    let mut out = Vec::with_capacity(36);
    for i in 0..6 {
        let ch = if i < bytes.len() { bytes[i] } else { b' ' };
        let v = CALL_ALPHABET.iter().position(|&c| c == ch).unwrap_or(38) as u64;
        out.extend(bits_of(v, 6));
    }
    out
}

fn bits_call(bits: &[u8]) -> String {
    let mut s = String::new();
    for i in 0..6 {
        let v = value_of(&bits[i * 6..(i + 1) * 6]) as usize;
        s.push(*CALL_ALPHABET.get(v).unwrap_or(&b'?') as char);
    }
    s.trim_end().to_string()
}

fn grid_bits(grid: &str) -> Vec<u8> {
    let g: Vec<u8> = grid.bytes().collect();
    let at = |i: usize| -> u8 { *g.get(i).unwrap_or(&b' ') };
    let mut out = Vec::with_capacity(30);
    for i in 0..2 {
        let v = GRID_LETTERS.iter().position(|&c| c == at(i)).unwrap_or(0) as u64;
        out.extend(bits_of(v, 5));
    }
    for i in 2..4 {
        let v = (at(i) as char).to_digit(10).unwrap_or(0) as u64;
        out.extend(bits_of(v, 4));
    }
    for i in 4..6 {
        let c = at(i);
        let v = if (b'A'..=b'X').contains(&c) { (c - b'A') as u64 } else { 0 };
        out.extend(bits_of(v, 5));
    }
    out.extend(bits_of(0, 2));
    out
}

fn bits_grid(bits: &[u8]) -> String {
    let a = GRID_LETTERS[(value_of(&bits[0..5]) % 18) as usize] as char;
    let b = GRID_LETTERS[(value_of(&bits[5..10]) % 18) as usize] as char;
    let c = char::from(b'0' + (value_of(&bits[10..14]) % 10) as u8);
    let d = char::from(b'0' + (value_of(&bits[14..18]) % 10) as u8);
    let e = char::from(b'A' + (value_of(&bits[18..23]) % 24) as u8);
    let f = char::from(b'A' + (value_of(&bits[23..28]) % 24) as u8);
    format!("{a}{b}{c}{d}{e}{f}")
}

pub fn mode_bits(raster: u8, k: u32, repeat: u8, font_id: u8) -> Vec<u8> {
    let k_idx = match k {
        1 => 0,
        2 => 1,
        4 => 2,
        _ => 3,
    };
    let mut v = Vec::with_capacity(10);
    v.extend(bits_of(if raster == 16 { 0 } else { 1 }, 2));
    v.extend(bits_of(k_idx as u64, 3));
    v.extend(bits_of((repeat.saturating_sub(1)) as u64, 2));
    v.extend(bits_of(font_id as u64, 3));
    v
}

pub fn bits_mode(bits: &[u8]) -> (u8, u32, u8, u8) {
    let raster = if value_of(&bits[0..2]) == 0 { 16 } else { 24 };
    let k = 1u32 << value_of(&bits[2..5]);
    let repeat = (value_of(&bits[5..7]) + 1) as u8;
    let font = value_of(&bits[7..10]) as u8;
    (raster, k, repeat, font)
}

impl Hdr {
    pub fn pack(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(HDR_BITS);
        b.extend(bits_of(FRAME_MAGIC as u64, 16));
        b.extend(bits_of(PROTO_VER as u64, 4));
        b.extend(mode_bits(self.raster, self.k, self.repeat, self.font_id));
        b.extend(call_bits(&self.call));
        b.extend(grid_bits(&self.grid));
        b.extend(bits_of((self.length & 0x3FF) as u64, 10));
        b.extend(bits_of((self.msg_type & 0x7) as u64, 3));
        b.extend(bits_of((self.sf_id & 0xFFF) as u64, 12));
        b.extend(bits_of((self.utc & 0x7FF) as u64, 11));
        b.extend(bits_of((self.power & 0x7) as u64, 3));
        b.extend(bits_of((self.rmap & 0xFF) as u64, 8));
        b.extend(bits_of((self.plan & 0x3) as u64, 2));
        b.extend(bits_of(0, 4));
        let crc = crc16(&b);
        b.extend(bits_of(crc as u64, 16));
        debug_assert_eq!(b.len(), HDR_BITS);
        b
    }

    pub fn unpack(bits: &[u8]) -> Option<Hdr> {
        if bits.len() < HDR_BITS {
            return None;
        }
        if value_of(&bits[0..16]) != FRAME_MAGIC as u64
            || value_of(&bits[16..20]) != PROTO_VER as u64
        {
            return None;
        }
        if crc16(&bits[0..149]) != value_of(&bits[149..165]) as u16 {
            return None;
        }
        let (raster, k, repeat, font_id) = bits_mode(&bits[20..30]);
        Some(Hdr {
            call: bits_call(&bits[30..66]),
            grid: bits_grid(&bits[66..96]),
            length: value_of(&bits[96..106]) as u16,
            msg_type: value_of(&bits[106..109]) as u8,
            sf_id: value_of(&bits[109..121]) as u16,
            utc: value_of(&bits[121..132]) as u16,
            power: value_of(&bits[132..135]) as u8,
            rmap: value_of(&bits[135..143]) as u8,
            plan: value_of(&bits[143..145]) as u8,
            raster, k, repeat, font_id,
        })
    }
}

/// BCON：`SF_ID 12 · MODE 10 · CALL_CRC8 8 · CELL 8 · FLAGS 8 · CRC16 16`
pub fn pack_bcon(sf_id: u16, cell: u8, flags: u8, mode: &[u8], call_crc: u8) -> Vec<u8> {
    let mut b = Vec::with_capacity(62);
    b.extend(bits_of((sf_id & 0xFFF) as u64, 12));
    b.extend_from_slice(mode);
    b.extend(bits_of(call_crc as u64, 8));
    b.extend(bits_of(cell as u64, 8));
    b.extend(bits_of(flags as u64, 8));
    let crc = crc16(&b);
    b.extend(bits_of(crc as u64, 16));
    debug_assert_eq!(b.len(), 62);
    b
}

/// EOT：`0x5A 8 · LEN mod 256 8`
pub fn pack_eot(length: usize) -> Vec<u8> {
    let mut b = bits_of(0x5A, 8);
    b.extend(bits_of((length & 0xFF) as u64, 8));
    b
}

pub fn call_crc(call: &str) -> u8 {
    (crc16(&call_bits(call)) & 0xFF) as u8
}

#[cfg(test)]
mod tests {    use super::*;

    #[test]
    fn roundtrip() {
        let h = Hdr { grid: "OM89AB".into(), length: 42, k: 4, repeat: 2, plan: 1, ..Default::default() };
        let bits = h.pack();
        assert_eq!(bits.len(), HDR_BITS);
        assert_eq!(Hdr::unpack(&bits), Some(h));
    }

    #[test]
    fn crc_rejects() {
        let h = Hdr::default();
        let mut bits = h.pack();
        bits[40] ^= 1;
        assert!(Hdr::unpack(&bits).is_none());
    }
}

// --------------------------------------------------------------------------
// 同步帧（v1.1）：104 比特
//   MAGIC8 | SLOT6 | CELL16 | CALL_CRC8 | GRID30 | MODE10 | FONT4 | RSV6 | CRC16
// --------------------------------------------------------------------------
pub const SYNC_MAGIC: u8 = 0xC5;
pub const SYNC_BITS: usize = 104;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyncFrame {
    pub slot: u8,
    pub cell: u16,
    pub call_crc: u8,
    pub grid: String,
    pub raster: u8,
    pub k: u8,
    pub font_id: u8,
}

impl SyncFrame {
    pub fn pack(&self) -> Vec<u8> {
        let mut b = bits_of(SYNC_MAGIC as u64, 8);
        b.extend(bits_of((self.slot % 60) as u64, 6));
        b.extend(bits_of(self.cell as u64, 16));
        b.extend(bits_of(self.call_crc as u64, 8));
        b.extend(grid_bits(&self.grid));
        b.extend(mode_bits(self.raster, self.k as u32, 1, self.font_id));
        b.extend(bits_of((self.font_id & 0xF) as u64, 4));
        b.extend(bits_of(0, 6));
        let crc = crc16(&b);
        b.extend(bits_of(crc as u64, 16));
        debug_assert_eq!(b.len(), SYNC_BITS);
        b
    }

    pub fn unpack(bits: &[u8]) -> Option<Self> {
        if bits.len() < SYNC_BITS {
            return None;
        }
        let b = &bits[..SYNC_BITS];
        if value_of(&b[0..8]) != SYNC_MAGIC as u64 {
            return None;
        }
        if value_of(&b[88..104]) as u16 != crc16(&b[..88]) {
            return None;
        }
        let (raster, k, _rep, _font) = bits_mode(&b[68..78]);
        let k = k as u8;
        Some(SyncFrame {
            slot: value_of(&b[8..14]) as u8,
            cell: value_of(&b[14..30]) as u16,
            call_crc: value_of(&b[30..38]) as u8,
            grid: bits_grid(&b[38..68]),
            raster,
            k,
            font_id: value_of(&b[78..82]) as u8,
        })
    }
}

#[cfg(test)]
mod sync_tests {
    use super::*;

    #[test]
    fn sync_roundtrip() {
        for &(slot, cell, raster, k, font) in &[
            (42u8, 1234u16, 16u8, 2u8, 1u8),
            (0, 0, 24, 4, 1),
            (59, 65535, 16, 8, 3),
        ] {
            let f = SyncFrame { slot, cell, call_crc: 0xA5, grid: "OM89AB".into(),
                                raster, k, font_id: font };
            let g = SyncFrame::unpack(&f.pack()).expect("unpack");
            assert_eq!(g.slot, slot);
            assert_eq!(g.cell, cell);
            assert_eq!(g.raster, raster);
            assert_eq!(g.k, k);
            assert_eq!(g.font_id, font);
            assert_eq!(g.grid, "OM89AB");
        }
    }
}