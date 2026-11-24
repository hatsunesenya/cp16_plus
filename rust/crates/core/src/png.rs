//! 极简 PNG 写出（zlib stored 块，不做压缩）。仅用于预览图，避免引入第三方依赖。

use std::fs;
use std::path::Path;

fn crc32(data: &[u8]) -> u32 {
    let mut table = [0u32; 256];
    for (i, slot) in table.iter_mut().enumerate() {
        let mut c = i as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
        }
        *slot = c;
    }
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc = table[((crc ^ b as u32) & 0xFF) as usize] ^ (crc >> 8);
    }
    crc ^ 0xFFFF_FFFF
}

fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for &x in data {
        a = (a + x as u32) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], body: &[u8]) {
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    let mut buf = Vec::with_capacity(4 + body.len());
    buf.extend_from_slice(kind);
    buf.extend_from_slice(body);
    out.extend_from_slice(&buf);
    out.extend_from_slice(&crc32(&buf).to_be_bytes());
}

/// rgB 8 bit，无滤波（每行前缀 0）。
pub fn write_rgb(path: &Path, width: usize, height: usize, rgb: &[u8]) -> Result<(), String> {
    let mut raw = Vec::with_capacity(height * (1 + width * 3));
    for y in 0..height {
        raw.push(0u8);
        let row = &rgb[y * width * 3..(y + 1) * width * 3];
        raw.extend_from_slice(row);
    }

    let mut z = vec![0x78u8, 0x01];
    let mut pos = 0usize;
    while pos < raw.len() {
        let n = (raw.len() - pos).min(65535);
        let last = pos + n == raw.len();
        z.push(if last { 1 } else { 0 });
        z.extend_from_slice(&(n as u16).to_le_bytes());
        z.extend_from_slice(&(!(n as u16)).to_le_bytes());
        z.extend_from_slice(&raw[pos..pos + n]);
        pos += n;
    }
    z.extend_from_slice(&adler32(&raw).to_be_bytes());

    let mut out = Vec::new();
    out.extend_from_slice(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&(width as u32).to_be_bytes());
    ihdr.extend_from_slice(&(height as u32).to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]); // 8bit, truecolor, deflate, filter0, no interlace
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &z);
    chunk(&mut out, b"IEND", &[]);
    fs::write(path, out).map_err(|e| format!("write {path:?}: {e}"))
}
