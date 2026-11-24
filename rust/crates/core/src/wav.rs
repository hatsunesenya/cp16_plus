//! WAV 读写：16/24/32 bit PCM 与 float32/64，单声道下混，任意采样率线性重采样。
//!
//! 与 Python `wavio.py` 行为一致（参考实现用线性插值；产品端换多相 FIR）。

use std::fs;
use std::path::Path;

pub struct Wav {
    pub fs: u32,
    pub samples: Vec<f32>,
}

fn le16(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

fn le32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

pub fn read(path: &Path) -> Result<Wav, String> {
    let data = fs::read(path).map_err(|e| format!("read {path:?}: {e}"))?;
    parse(&data)
}

/// 从内存解析（供 HTTP 上传用）。
pub fn parse(data: &[u8]) -> Result<Wav, String> {
    if data.len() < 12 || &data[0..4] != b"RIFF" || &data[8..12] != b"WAVE" {
        return Err("not a RIFF/WAVE file".into());
    }
    let mut pos = 12usize;
    let mut tag = 0u16;
    let mut ch = 1u16;
    let mut fs_hz = 8000u32;
    let mut bits = 16u16;
    let mut payload: Option<&[u8]> = None;

    while pos + 8 <= data.len() {
        let cid = &data[pos..pos + 4];
        let size = le32(&data, pos + 4) as usize;
        let body_end = (pos + 8 + size).min(data.len());
        let body = &data[pos + 8..body_end];
        if cid == b"fmt " && body.len() >= 16 {
            tag = le16(body, 0);
            ch = le16(body, 2);
            fs_hz = le32(body, 4);
            bits = le16(body, 14);
            if tag == 0xFFFE && body.len() >= 26 {
                tag = le16(body, 24);
            }
        } else if cid == b"data" {
            payload = Some(body);
        }
        pos += 8 + size + (size & 1);
    }
    let payload = payload.ok_or("missing data chunk")?;

    let mut mono: Vec<f32> = match (tag, bits) {
        (3, 32) => payload
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect(),
        (1, 16) => payload
            .chunks_exact(2)
            .map(|c| i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0)
            .collect(),
        (1, 24) => payload
            .chunks_exact(3)
            .map(|c| {
                let v = (c[0] as i32) | ((c[1] as i32) << 8) | ((c[2] as i32) << 16);
                let v = if v & 0x80_0000 != 0 { v - 0x100_0000 } else { v };
                v as f32 / 8_388_608.0
            })
            .collect(),
        (1, 32) => payload
            .chunks_exact(4)
            .map(|c| i32::from_le_bytes([c[0], c[1], c[2], c[3]]) as f32 / 2_147_483_648.0)
            .collect(),
        _ => return Err(format!("unsupported wav: tag={tag} bits={bits}")),
    };

    if ch > 1 {
        let n = ch as usize;
        let frames = mono.len() / n;
        let mut out = Vec::with_capacity(frames);
        for i in 0..frames {
            let mut acc = 0.0f32;
            for c in 0..n {
                acc += mono[i * n + c];
            }
            out.push(acc / n as f32);
        }
        mono = out;
    }
    Ok(Wav { fs: fs_hz, samples: mono })
}

/// 线性插值重采样（与 Python 参考实现一致）。
pub fn resample(x: &[f32], fs_in: u32, fs_out: u32) -> Vec<f32> {
    if fs_in == fs_out || x.is_empty() {
        return x.to_vec();
    }
    let ratio = fs_in as f64 / fs_out as f64;
    let n_out = ((x.len() as f64) * fs_out as f64 / fs_in as f64).round() as usize;
    let mut out = Vec::with_capacity(n_out);
    for i in 0..n_out {
        let t = i as f64 * ratio;
        let i0 = t.floor() as usize;
        let frac = (t - i0 as f64) as f32;
        let a = x[i0.min(x.len() - 1)];
        let b = x[(i0 + 1).min(x.len() - 1)];
        out.push(a + (b - a) * frac);
    }
    out
}

pub fn to_bytes(x: &[f32], fs: u32) -> Vec<u8> {
    let mut pcm = Vec::with_capacity(x.len() * 2);
    for v in x {
        let s = (v.clamp(-1.0, 1.0) * 32767.0) as i16;
        pcm.extend_from_slice(&s.to_le_bytes());
    }
    let mut out = Vec::with_capacity(44 + pcm.len());
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + pcm.len() as u32).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&fs.to_le_bytes());
    out.extend_from_slice(&(fs * 2).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&(pcm.len() as u32).to_le_bytes());
    out.extend_from_slice(&pcm);
    out
}

pub fn write_f32(path: &Path, x: &[f32], fs: u32) -> Result<(), String> {
    fs::write(path, to_bytes(x, fs)).map_err(|e| format!("write {path:?}: {e}"))
}
