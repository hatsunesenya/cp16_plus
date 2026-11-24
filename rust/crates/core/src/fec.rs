//! FEC：K=7 卷积码（r=1/2，0o171/0o133）、确定性块交织、软判决 Viterbi。

use std::sync::OnceLock;

pub const K: usize = 7;
pub const NSTATE: usize = 1 << (K - 1);
pub const TAIL: usize = K - 1;
pub const POLYS: [u32; 2] = [0o171, 0o133];

type OutTab = [[[u8; 2]; 2]; NSTATE];
type NextTab = [[u8; 2]; NSTATE];

fn parity(mut v: u32) -> u8 {
    v ^= v >> 4;
    v ^= v >> 2;
    v ^= v >> 1;
    (v & 1) as u8
}

fn tables() -> &'static (OutTab, NextTab) {
    static TAB: OnceLock<(OutTab, NextTab)> = OnceLock::new();
    TAB.get_or_init(|| {
        let mut out = [[[0u8; 2]; 2]; NSTATE];
        let mut next = [[0u8; 2]; NSTATE];
        for s in 0..NSTATE {
            for b in 0..2usize {
                let reg = ((s << 1) | b) as u32;
                out[s][b][0] = parity(reg & POLYS[0]);
                out[s][b][1] = parity(reg & POLYS[1]);
                next[s][b] = (((s << 1) | b) & (NSTATE - 1)) as u8;
            }
        }
        (out, next)
    })
}

pub fn conv_encode(bits: &[u8]) -> Vec<u8> {
    let (out, next) = tables();
    let mut padded = Vec::with_capacity(bits.len() + TAIL);
    padded.extend_from_slice(bits);
    padded.extend(std::iter::repeat(0u8).take(TAIL));
    let mut coded = Vec::with_capacity(padded.len() * 2);
    let mut s = 0usize;
    for &b in &padded {
        let bi = (b & 1) as usize;
        coded.push(out[s][bi][0]);
        coded.push(out[s][bi][1]);
        s = next[s][bi] as usize;
    }
    coded
}

/// 确定性块交织置换（与 Python `fec._perm` 完全一致）。
pub fn perm(n: usize) -> Vec<usize> {
    let rows = ((n as f64).sqrt().floor() as usize).max(1);
    let cols = (n + rows - 1) / rows;
    let mut seen = vec![false; n];
    let mut out = Vec::with_capacity(n);
    for i in 0..(n * 2) {
        let v = (i % rows) * cols + (i / rows);
        if v < n && !seen[v] {
            seen[v] = true;
            out.push(v);
        }
    }
    for v in 0..n {
        if !seen[v] {
            out.push(v);
        }
    }
    out
}

pub fn interleave(bits: &[u8]) -> Vec<u8> {
    let p = perm(bits.len());
    let mut out = vec![0u8; bits.len()];
    for (i, &pi) in p.iter().enumerate() {
        out[pi] = bits[i];
    }
    out
}

pub fn deinterleave(soft: &[f32]) -> Vec<f32> {
    let p = perm(soft.len());
    p.iter().map(|&i| soft[i]).collect()
}

/// soft：正 = 比特 0；长度须为偶数。
pub fn viterbi(soft: &[f32], n_info: usize) -> Vec<u8> {
    let (out, next) = tables();
    let n_steps = soft.len() / 2;
    const NEG: f32 = -1e18;
    let mut metric = vec![NEG; NSTATE];
    metric[0] = 0.0;
    let mut pred = vec![0u8; n_steps * NSTATE];

    for t in 0..n_steps {
        let mut new = vec![NEG; NSTATE];
        let r0 = soft[2 * t];
        let r1 = soft[2 * t + 1];
        for s in 0..NSTATE {
            let ms = metric[s];
            if ms <= NEG {
                continue;
            }
            for b in 0..2usize {
                let ns = next[s][b] as usize;
                let m = ms
                    + if out[s][b][0] == 0 { r0 } else { -r0 }
                    + if out[s][b][1] == 0 { r1 } else { -r1 };
                if m > new[ns] {
                    new[ns] = m;
                    pred[t * NSTATE + ns] = (s * 2 + b) as u8;
                }
            }
        }
        metric = new;
    }

    let mut state = 0usize;
    let mut bits = vec![0u8; n_steps];
    for t in (0..n_steps).rev() {
        let code = pred[t * NSTATE + state] as usize;
        bits[t] = (code & 1) as u8;
        state = code >> 1;
    }
    bits.truncate(n_info);
    bits
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_noiseless() {
        let bits: Vec<u8> = (0..40).map(|i| ((i * 7 + 3) % 2) as u8).collect();
        let coded = conv_encode(&bits);
        assert_eq!(coded.len(), 2 * (bits.len() + TAIL));
        let soft: Vec<f32> = coded.iter().map(|&b| if b == 0 { 1.0 } else { -1.0 }).collect();
        assert_eq!(viterbi(&soft, bits.len()), bits);
    }

    #[test]
    fn perm_is_bijection() {
        for n in [16usize, 44, 136, 342] {
            let p = perm(n);
            let mut seen = vec![false; n];
            for &v in &p {
                assert!(v < n && !seen[v], "n={n} v={v}");
                seen[v] = true;
            }
            assert_eq!(p.len(), n);
        }
    }
}
