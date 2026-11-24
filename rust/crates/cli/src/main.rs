//! CP16+ 命令行：收信（WAV → 参数/字符块/预览）、golden 对拍、参数自检。

use std::fs;
use std::path::{Path, PathBuf};

use cp16plus_cli::server;

use cp16plus_core::params::{self, CC_BAUD, CC_CENTER, FS};
use cp16plus_core::{fec, modem, png, rx, wav};
use cp16plus_core::frame::Hdr;

const CMAP: [[u8; 3]; 8] = [
    [4, 4, 10], [22, 14, 60], [92, 26, 128], [150, 40, 122],
    [210, 78, 92], [245, 132, 70], [255, 190, 110], [255, 255, 255],
];

fn read_f32(path: &Path) -> Vec<f32> {
    let bytes = fs::read(path).unwrap_or_else(|e| panic!("read {path:?}: {e}"));
    bytes
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

fn read_u8(path: &Path) -> Vec<u8> {
    fs::read(path).unwrap_or_else(|e| panic!("read {path:?}: {e}"))
}

struct Report {
    fails: usize,
}

impl Report {
    fn check(&mut self, name: &str, ok: bool, detail: String) {
        println!("  [{}] {name}  {detail}", if ok { "ok" } else { "FAIL" });
        if !ok {
            self.fails += 1;
        }
    }
}

fn golden(dir: &Path) -> i32 {
    println!("CP16+ Rust golden 对拍  ({})", dir.display());
    let mut r = Report { fails: 0 };

    let m = params::by_name("M2").expect("M2");
    r.check("档位 M2", m.n == 16 && (m.df - 25.0).abs() < 1e-12 && m.k == 2,
            format!("n={} df={} k={} rows/字={}", m.n, m.df, m.k, m.rows_per_char()));
    r.check("行=5k 符号", m.row_symbols() == 10, format!("{}", m.row_symbols()));

    let hdr = Hdr {
        call: "BG1ABC".into(), grid: "OM89AB".into(), length: 42, msg_type: 0,
        sf_id: 0, utc: 733, power: 1, rmap: 0, plan: 0, raster: 16, k: 2,
        repeat: 1, font_id: 1,
    };
    let bits = hdr.pack();
    let want_bits = read_u8(&dir.join("hdr_bits.u8"));
    r.check("HDR 位流一致", bits == want_bits, format!("len={}", bits.len()));
    r.check("HDR 自解", Hdr::unpack(&bits).as_ref() == Some(&hdr), String::new());

    let coded = fec::interleave(&fec::conv_encode(&bits));
    let want_coded = read_u8(&dir.join("hdr_coded.u8"));
    let same = coded.len() == want_coded.len() && coded.iter().zip(&want_coded).all(|(a, b)| a == b);
    r.check("FEC+交织 一致", same, format!("len={}", coded.len()));

    // 同步帧（v1.1）与 Python 参考逐位对齐
    let sf = cp16plus_core::frame::SyncFrame {
        slot: 42, cell: 1234,
        call_crc: cp16plus_core::frame::call_crc("BG1ABC"),
        grid: "OM89AB".into(), raster: 16, k: 2, font_id: 1,
    };
    let got = sf.pack();
    let want_sync = read_u8(&dir.join("sync_bits.u8"));
    r.check("同步帧位流一致", got == want_sync, format!("len={}", got.len()));

    let sig = modem::modulate(&coded, FS, CC_CENTER, CC_BAUD, 0.35, modem::preamble_hdr());
    let want = read_f32(&dir.join("cc_audio.f32"));
    let n = sig.len().min(want.len());
    let maxerr = (0..n).map(|i| (sig[i] - want[i]).abs()).fold(0.0f32, f32::max);
    let rel = maxerr / want.iter().fold(0.0f32, |a, b| a.max(b.abs()));
    r.check("CC 波形一致", sig.len() == want.len() && rel < 1e-5,
            format!("len={} maxerr={:.2e} rel={:.2e}", sig.len(), maxerr, rel));

    let soft = modem::demodulate_at(&sig, FS, CC_CENTER, CC_BAUD, 0, coded.len(),
                                    modem::preamble_hdr());
    let decoded = fec::viterbi(&fec::deinterleave(&soft), params::HDR_BITS);
    r.check("软值→译码→HDR", Hdr::unpack(&decoded).as_ref() == Some(&hdr), String::new());

    println!("\n{}", if r.fails == 0 { "全部通过" } else { "有失败项" });
    r.fails as i32
}

/// 把字符块渲染成预览图（与 Python `display.py` 同参数：6–18 dB、gamma 0.9）。
fn render(res: &rx::RxResult, px_tone: usize) -> (usize, usize, Vec<u8>) {
    if res.blocks.is_empty() {
        return (1, 1, vec![0, 0, 0]);
    }
    let mut all: Vec<f64> = res.blocks.iter().flat_map(|b| b.iter().flatten().copied()).collect();
    all.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mut floor = all[(all.len() as f64 * 0.2) as usize % all.len()];
    let pmax = *all.last().unwrap();
    if floor < pmax * 1e-8 {
        floor = all[(all.len() as f64 * 0.4) as usize % all.len()];
    }
    floor = floor.max(pmax * 1e-6).max(1e-30);

    let rep = res.mode.repeat.max(1);
    let px_row = (px_tone / rep).max(1);
    let n_tones = res.blocks[0][0].len();
    let gap = 2usize;

    let width = n_tones * px_tone;
    let mut rows_img: Vec<Vec<u8>> = Vec::new();
    for (bi, blk) in res.blocks.iter().enumerate() {
        if bi > 0 {
            rows_img.push(vec![0u8; width * 3]);
        }
        for row in blk {
            let mut px = Vec::with_capacity(width * 3);
            for &p in row {
                let db = 10.0 * (p / floor).max(1e-12).log10();
                let v = ((db - 6.0) / 12.0).clamp(0.0, 1.0).powf(0.9);
                let idx = (v * (CMAP.len() - 1) as f64) as usize;
                let c = CMAP[idx.min(CMAP.len() - 1)];
                for _ in 0..px_tone {
                    px.extend_from_slice(&c);
                }
            }
            for _ in 0..px_row {
                rows_img.push(px.clone());
            }
        }
    }
    let height = rows_img.len();
    let mut img = Vec::with_capacity(height * width * 3);
    for r in rows_img {
        img.extend_from_slice(&r);
    }
    let _ = gap;
    (width, height, img)
}

fn write_blocks(path: &Path, res: &rx::RxResult) -> std::io::Result<()> {
    let mut out = Vec::new();
    out.extend_from_slice(b"CB16");
    let rows = res.blocks.first().map(|b| b.len()).unwrap_or(0) as u32;
    let tones = res.blocks.first().map(|b| b[0].len()).unwrap_or(0) as u32;
    out.extend_from_slice(&(res.blocks.len() as u32).to_le_bytes());
    out.extend_from_slice(&rows.to_le_bytes());
    out.extend_from_slice(&tones.to_le_bytes());
    for blk in &res.blocks {
        for row in blk {
            for &p in row {
                out.extend_from_slice(&(p as f32).to_le_bytes());
            }
        }
    }
    fs::write(path, out)
}

fn cmd_rx(path: &Path, png_out: Option<&Path>, json_out: Option<&Path>,
          blocks_out: Option<&Path>, mode_name: Option<&str>) -> i32 {
    let w = match wav::read(path) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    };
    let x = wav::resample(&w.samples, w.fs, FS as u32);
    let force = mode_name.and_then(params::by_name);
    if mode_name.is_some() && force.is_none() {
        eprintln!("未知档位 {mode_name:?}");
        return 2;
    }
    if std::env::var("CP16P_DEBUG").is_ok() {
        let est2 = cp16plus_core::pilot::estimate(&x, FS, params::PILOT1, params::PILOT2, 60.0);
        let center = est2.a * CC_CENTER + est2.b;
        let baud = CC_BAUD * est2.a;
        for b in cp16plus_core::modem::find_bursts(&x, FS, center, baud, 0.65, 40) {
            let nc = 2 * (params::HDR_BITS + cp16plus_core::fec::TAIL);
            let soft = cp16plus_core::modem::demodulate_at(&x, FS, center, baud, b.start, nc,
                                                           cp16plus_core::modem::preamble_hdr());
            let bits = cp16plus_core::fec::viterbi(&cp16plus_core::fec::deinterleave(&soft),
                                                   params::HDR_BITS);
            eprintln!("burst {:?} start={} score={:.2} magic={:04x} first={:?}",
                      b.kind, b.start, b.score,
                      bits[..16].iter().fold(0u16, |a, &v| (a << 1) | v as u16),
                      &bits[..8]);
        }
    }
    let t0 = std::time::Instant::now();
    let res = rx::decode(&x, FS, force);
    let dt = t0.elapsed().as_secs_f64();

    let hdr_json = match &res.hdr {
        Some(h) => format!(
            "{{\"call\":\"{}\",\"grid\":\"{}\",\"length\":{},\"k\":{},\"repeat\":{},\"raster\":{},\"plan\":{},\"utc\":{}}}",
            h.call, h.grid, h.length, h.k, h.repeat, h.raster, h.plan, h.utc),
        None => "null".to_string(),
    };
    let bursts: Vec<String> = res.bursts.iter()
        .map(|b| format!("{{\"kind\":\"{}\",\"t\":{:.3},\"score\":{:.2}}}",
                         b.kind, b.start as f64 / FS as f64, b.score))
        .collect();
    let anchors: Vec<String> = res.anchors.iter().map(|a| format!("\"{a}\"")).collect();
    let json = format!(
        "{{\n  \"fs\": {},\n  \"seconds\": {:.2},\n  \"a\": {:.9},\n  \"b\": {:.4},\n  \"pilot_conf\": {:.3},\n  \
         \"mode\": \"{}\",\n  \"hdr\": {},\n  \"bursts\": [{}],\n  \"anchors\": [{}],\n  \"cells\": {},\n  \"sync\": {{\"frames\": {}, \"slot\": {}, \"cell\": {}}},\n  \
         \"metrics\": {{\"eta\": {:.3}, \"kappa\": {:.3}, \"nu\": {:.3}, \"J\": {:.3}}},\n  \"decode_seconds\": {:.3}\n}}",
        FS, x.len() as f64 / FS as f64, res.est.a, res.est.b, res.est.conf,
        res.mode.name, hdr_json, bursts.join(","), anchors.join(","), res.cells,
        res.sync_frames, res.sync_slot, res.sync_cell,
        res.metrics.eta, res.metrics.kappa, res.metrics.nu, res.metrics.j, dt);

    match json_out {
        Some(p) => {
            if let Err(e) = fs::write(p, &json) {
                eprintln!("write {p:?}: {e}");
            }
        }
        None => println!("{json}"),
    }

    if let Some(p) = blocks_out {
        if let Err(e) = write_blocks(p, &res) {
            eprintln!("{e}");
        }
    }

    if let Some(p) = png_out {
        let (w_, h_, img) = render(&res, 10);
        if let Err(e) = png::write_rgb(p, w_, h_, &img) {
            eprintln!("{e}");
            return 2;
        }
    }

    if json_out.is_some() {
        println!("cells={} hdr={} anchors={:?} J={:.2}", res.cells,
                 res.hdr.as_ref().map(|h| h.call.clone()).unwrap_or_default(),
                 res.anchors, res.metrics.j);
    }
    if res.cells == 0 { 1 } else { 0 }
}

// --------------------------------------------------------------------------
// check：读 index.csv，逐条解码并与真值点阵比对
// --------------------------------------------------------------------------

struct Truth {
    n: usize,
    rows: usize,
    cols: usize,
    bits: Vec<u8>,
}

fn read_truth(path: &Path) -> Option<Truth> {
    let raw = fs::read(path).ok()?;
    if raw.len() < 16 || &raw[0..4] != b"GT16" {
        return None;
    }
    let n = u32::from_le_bytes([raw[4], raw[5], raw[6], raw[7]]) as usize;
    let rows = u32::from_le_bytes([raw[8], raw[9], raw[10], raw[11]]) as usize;
    let cols = u32::from_le_bytes([raw[12], raw[13], raw[14], raw[15]]) as usize;
    let need = n * rows * cols;
    if raw.len() < 16 + need {
        return None;
    }
    Some(Truth { n, rows, cols, bits: raw[16..16 + need].to_vec() })
}

fn split_csv(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_q = false;
    for ch in line.chars() {
        match ch {
            '"' => in_q = !in_q,
            ',' if !in_q => {
                out.push(std::mem::take(&mut cur));
            }
            _ => cur.push(ch),
        }
    }
    out.push(cur);
    out
}

fn acc_blocks(blocks: &[Vec<Vec<f64>>], t: &Truth, repeat: usize) -> f64 {
    let mut accs = Vec::new();
    for (i, blk) in blocks.iter().enumerate() {
        if i >= t.n {
            break;
        }
        let mut vals: Vec<f64> = blk.iter().flatten().copied().collect();
        vals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let (lo, hi) = (vals[0], *vals.last().unwrap());
        let med = vals[vals.len() / 2];
        let thr = 0.5 * (hi + med).max(lo + 1e-30);
        let mut hit = 0usize;
        let mut tot = 0usize;
        for r in 0..t.rows {
            let row = &blk[(r * repeat).min(blk.len() - 1)];
            for c in 0..t.cols.min(row.len()) {
                let rec = if row[c] > thr { 1u8 } else { 0u8 };
                let want = t.bits[i * t.rows * t.cols + r * t.cols + c];
                hit += (rec == want) as usize;
                tot += 1;
            }
        }
        if tot > 0 {
            accs.push(hit as f64 / tot as f64);
        }
    }
    if accs.is_empty() {
        0.0
    } else {
        accs.iter().sum::<f64>() / accs.len() as f64
    }
}

fn cmd_check(dir: &Path) -> i32 {
    let index = dir.join("index.csv");
    let text = match fs::read_to_string(&index) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("读不到 {:?}: {e}", index);
            eprintln!("先在样张目录里跑：cp16plus samples（Python 端生成）");
            return 2;
        }
    };
    let mut lines = text.lines();
    // Excel 友好起见 CSV 带 UTF-8 BOM，读的时候要去掉
    let header = split_csv(lines.next().unwrap_or("").trim_start_matches('\u{feff}'));
    let col = |name: &str| header.iter().position(|h| h.trim().trim_start_matches('\u{feff}') == name);
    let (Some(c_file), Some(c_snr), Some(c_chars)) =
        (col("file"), col("snr_db"), col("chars"))
    else {
        eprintln!("index.csv 缺列（需要有 file/snr_db/chars）");
        return 2;
    };
    let c_truth = col("truth");
    let c_mode = col("mode");

    println!("{:<22} {:>5} {:>7} {:>6} {:>5} {:>6} {:>7}",
             "file", "snr", "字(得/应)", "HDR", "锚定", "位准", "耗时");
    let mut csv = String::from("file,snr_db,chars_got,chars_want,hdr,anchor,acc,seconds\n");
    let (mut ok_chars, mut ok_hdr, mut acc_sum, mut n_row) = (0usize, 0usize, 0.0f64, 0usize);
    // 按信噪比分档统计，避免"地板档"把整体数字拉花
    let mut bucket = [(0usize, 0usize); 3]; // (正确, 总数): >=-6 / -9 / <=-12

    for line in lines.filter(|l| !l.trim().is_empty()) {
        let f = split_csv(line);
        let get = |i: usize| f.get(i).map(|s| s.as_str()).unwrap_or("");
        let file = get(c_file).to_string();
        let snr: f64 = get(c_snr).parse().unwrap_or(f64::NAN);
        let want: usize = get(c_chars).parse().unwrap_or(0);

        let wav_path = dir.join(&file);
        let w = match wav::read(&wav_path) {
            Ok(w) => w,
            Err(e) => {
                println!("{file:<22} 读取失败: {e}");
                continue;
            }
        };
        let x = wav::resample(&w.samples, w.fs, FS as u32);
        // 样张的档位是已知的：强制档位，避免弱信号下退回默认档带来的额外误差
        // 不再强制档位：让 HDR 自己决定，才能测到真实的档位识别与 HDR 成功率
        let forced: Option<params::Mode> = None;
        let _ = c_mode;
        let t0 = std::time::Instant::now();
        let res = rx::decode(&x, FS, forced);
        let dt = t0.elapsed().as_secs_f64();

        let truth = c_truth
            .and_then(|i| get(i).to_string().into())
            .filter(|s: &String| !s.is_empty())
            .and_then(|name| read_truth(&dir.join(name)));
        let acc = truth
            .as_ref()
            .map(|t| acc_blocks(&res.blocks, t, res.mode.repeat))
            .unwrap_or(f64::NAN);

        let hdr_ok = res.hdr.is_some();
        let anchor = res.anchors.first().copied().unwrap_or("none");
        let chars_ok = res.cells == want;
        ok_chars += chars_ok as usize;
        ok_hdr += hdr_ok as usize;
        if acc.is_finite() {
            acc_sum += acc;
        }
        n_row += 1;
        let bi = if snr >= -6.0 { 0 } else if snr >= -9.5 { 1 } else { 2 };
        bucket[bi].1 += 1;
        bucket[bi].0 += chars_ok as usize;

        println!("{:<22} {:>5.0} {:>7} {:>6} {:>5} {:>6.3} {:>6.2}s",
                 file, snr, format!("{}/{}", res.cells, want),
                 if hdr_ok { "OK" } else { "--" }, anchor, acc, dt);
        csv.push_str(&format!("{},{},{},{},{},{},{:.3},{:.3}\n",
                              file, snr, res.cells, want,
                              if hdr_ok { "ok" } else { "--" }, anchor, acc, dt));
    }

    let n = n_row.max(1);
    println!("\n字数正确 {}/{}（≥-6 dB {}/{} · -9 dB {}/{} · ≤-12 dB {}/{}）· HDR {}/{} · 平均位准 {:.3}",
             ok_chars, n_row, bucket[0].0, bucket[0].1, bucket[1].0, bucket[1].1,
             bucket[2].0, bucket[2].1, ok_hdr, n_row, acc_sum / n as f64);
    let out = dir.join("check.csv");
    if fs::write(&out, csv).is_ok() {
        println!("明细 -> {}", out.display());
    }
    if ok_chars == n_row { 0 } else { 1 }
}

fn cmd_tx(text: &str, out: &Path, mode_name: &str, font_path: &Path,
          call: &str, grid: &str, sync: bool) -> i32 {
    // 找不到外部字库就用编进 exe 的那份：发行版不带任何附带文件
    let loaded = if font_path.exists() {
        cp16plus_core::font::CpfFont::load(font_path)
    } else {
        cp16plus_core::font::CpfFont::load_bytes(server::FONT_BYTES.to_vec())
    };
    let font = match loaded {
        Ok(f) => f,
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    };
    let (glyphs, kept) = font.text(text);
    if glyphs.is_empty() {
        eprintln!("没有可发送的字形（字库里没有这些字）");
        return 2;
    }
    let Some(mode) = params::by_name(mode_name) else {
        eprintln!("未知档位 {mode_name}");
        return 2;
    };
    let opts = cp16plus_core::tx::TxOpts {
        call: call.to_string(),
        grid: grid.to_string(),
        sync_period: if sync { 10.0 } else { 0.0 },
        ..Default::default()
    };
    let t0 = std::time::Instant::now();
    let (x, meta) = cp16plus_core::tx::build(&glyphs, mode, &opts);
    if let Err(e) = wav::write_f32(out, &x, FS as u32) {
        eprintln!("{e}");
        return 2;
    }
    println!("{} 字 / {} 段 / {:.2} s / 用时 {:.2}s -> {}",
             kept.chars().count(), meta.segments.len(),
             x.len() as f64 / FS as f64, t0.elapsed().as_secs_f64(), out.display());
    println!("  HDR: call={} grid={} k={} repeat={} raster={}",
             meta.hdr.call, meta.hdr.grid, meta.hdr.k, meta.hdr.repeat, meta.hdr.raster);
    0
}

fn cmd_stream(path: &Path, block_ms: f64, realtime: bool, verify: bool) -> i32 {
    let w = match wav::read(path) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    };
    let mut st = cp16plus_core::rt::Stream::new(w.fs as usize, None);
    let block = ((w.fs as f64 * block_ms / 1000.0) as usize).max(64);
    let t0 = std::time::Instant::now();
    let mut shown = 0usize;
    let audio_s = w.samples.len() as f64 / w.fs as f64;
    println!("流式解码 {:?}  fs={}  {:.1}s  块 {:.0} ms{}", path, w.fs, audio_s, block_ms,
             if realtime { "（实时节奏）" } else { "" });

    for chunk in w.samples.chunks(block) {
        if let Some(ev) = st.feed(chunk) {
            if ev.new_cells > 0 {
                shown = ev.cells;
                println!("  [{:6.2}s] 字数 {:>2} (+{})  HDR {}  锚定 {:?}",
                         ev.audio_s, ev.cells, ev.new_cells,
                         ev.hdr.unwrap_or_else(|| "-".into()), ev.anchors);
            }
        }
        if realtime {
            let want = st.audio_seconds() / 1.0;
            let have = t0.elapsed().as_secs_f64();
            if want > have {
                std::thread::sleep(std::time::Duration::from_secs_f64(want - have));
            }
        }
    }
    if let Some(ev) = st.finish() {
        shown = ev.cells;
        println!("  [收尾] 字数 {:>2} (+{})", ev.cells, ev.new_cells);
    }
    if std::env::var("CP16P_DEBUG").is_ok() {
        eprintln!("[stream] {}", st.debug_state());
    }
    let dt = t0.elapsed().as_secs_f64();
    println!("\n共 {} 字，音频 {:.1}s，处理 {:.2}s（{:.0}× 实时）",
             shown, audio_s, dt, audio_s / dt.max(1e-9));
    if verify {
        let blocks = st.take_blocks();
        let off = rx::decode(st.audio(), FS, None);
        // 判定必须完全一致；数值在重采样路径下允许亚采样级的定时差异
        fn bits(blk: &[Vec<f64>]) -> Vec<Vec<u8>> {
            let flat: Vec<f64> = blk.iter().flat_map(|r| r.iter().copied()).collect();
            let mut s = flat.clone();
            s.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            let med = s[s.len() / 2];
            let hi = *s.last().unwrap();
            let thr = 0.5 * (hi + med);
            blk.iter().map(|r| r.iter().map(|v| (*v > thr) as u8).collect()).collect()
        }
        let mut same = blocks.len() == off.blocks.len();
        for (a, b) in blocks.iter().zip(&off.blocks) {
            if a.len() != b.len() || a[0].len() != b[0].len() || bits(a) != bits(b) {
                same = false;
            }
        }
        let mut maxd = 0.0f64;
        for (ci, (a, b)) in blocks.iter().zip(&off.blocks).enumerate() {
            for (ri, (x, y)) in a.iter().zip(b).enumerate() {
                for (ti, (p, q)) in x.iter().zip(y).enumerate() {
                    let d = (p - q).abs();
                    if d > maxd {
                        maxd = d;
                        if d > 1e-9 { eprintln!("  diff cell{ci} row{ri} tone{ti}: {p} vs {q}"); }
                    }
                }
            }
        }
        let scale = off.blocks.iter().flat_map(|b| b.iter()).flat_map(|r| r.iter())
            .fold(0.0f64, |m, v| m.max(v.abs()));
        println!("  最大数值差 {maxd:.3e}（块峰值 {scale:.3e}）");
        println!("与离线 rx 对拍：流式 {} 块 / 离线 {} 块 -> {}",
                 blocks.len(), off.blocks.len(), if same { "逐值一致" } else { "不一致" });
        if !same {
            return 1;
        }
    }
    if shown == 0 { 1 } else { 0 }
}

#[cfg(feature = "audio")]
fn cmd_audio(args: &[String]) -> i32 {
    let mut device: Option<String> = None;
    let mut rate: Option<u32> = None;
    let mut seconds = 0.0f64;
    let mut list = false;
    let mut loopback = false;
    let mut save: Option<PathBuf> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--list" => { list = true; i += 1; }
            "--save" => { save = args.get(i + 1).map(PathBuf::from); i += 2; }
            "--loopback" => { loopback = true; i += 1; }
            "--device" => { device = args.get(i + 1).cloned(); i += 2; }
            "--rate" => { rate = args.get(i + 1).and_then(|s| s.parse().ok()); i += 2; }
            "--seconds" => { seconds = args.get(i + 1).and_then(|s| s.parse().ok()).unwrap_or(0.0); i += 2; }
            other => { eprintln!("未知参数 {other}"); return 2; }
        }
    }
    if list {
        let devs = cp16plus_audio::list_devices();
        if devs.is_empty() {
            println!("没有找到输入设备");
            return 1;
        }
        let def = cp16plus_audio::default_device_name().unwrap_or_default();
        for d in devs {
            println!("{}{}", d, if d == def { "   (默认)" } else { "" });
        }
        return 0;
    }

    let cfg = cp16plus_audio::SourceConfig {
        device,
        sample_rate: rate,
        loopback,
        channel: cp16plus_audio::ChannelMode::Auto,
    };
    let cap = match cp16plus_audio::open(&cfg) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            eprintln!("用 `cp16plus audio --list` 看看有哪些输入设备");
            return 2;
        }
    };
    println!("采集：{}  {} Hz  {} 声道", cap.name, cap.fs, cap.channels);
    let mut st = cp16plus_core::rt::Stream::new(cap.fs as usize, None);
    let mut raw: Vec<f32> = Vec::new();
    let t0 = std::time::Instant::now();
    loop {
        match cap.recv() {
            Some(block) => {
                if save.is_some() {
                    raw.extend_from_slice(&block);
                }
                if let Some(ev) = st.feed(&block) {
                    println!("  [{:6.2}s] 字数 {:>2} (+{})  HDR {}", ev.audio_s, ev.cells,
                             ev.new_cells, ev.hdr.unwrap_or_else(|| "-".into()));
                }
            }
            None => continue,
        }
        if seconds > 0.0 && t0.elapsed().as_secs_f64() >= seconds {
            break;
        }
    }
    if let Some(ev) = st.finish() {
        println!("  [收尾] 字数 {}", ev.cells);
    }
    if let Some(p) = save.as_deref() {
        match cp16plus_core::wav::write_f32(p, &raw, cap.fs) {
            Ok(_) => println!("已保存 {:?}（{:.1}s）", p, raw.len() as f64 / cap.fs as f64),
            Err(e) => eprintln!("{e}"),
        }
    }
    0
}

fn usage() {
    eprintln!("cp16plus <命令>");
    eprintln!("  tx <文本> [--out out.wav] [--mode M2] [--font fonts/hzk16.cpf] [--call BG1ABC] [--grid OM89AB]");
    eprintln!("  rx <file.wav> [--png out.png] [--json out.json] [--blocks out.bin] [--mode M2]");
    eprintln!("  stream <file.wav> [--block-ms 20] [--realtime] [--verify]");
    #[cfg(feature = "audio")]
    eprintln!("  audio [--list] [--device 名称] [--loopback] [--seconds N]   （实时采集）");
    eprintln!("  serve [--port 8770] [--open]   （内置网页界面，双击即用）");
    eprintln!("  check <dir>");
    eprintln!("  golden <dir>");
    eprintln!("  version");
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let code = match args.get(1).map(|s| s.as_str()) {
        Some("rx") => {
            let Some(path) = args.get(2) else { usage(); std::process::exit(2) };
            let mut png_out: Option<PathBuf> = None;
            let mut json_out: Option<PathBuf> = None;
            let mut mode: Option<String> = None;
            let mut blocks_out: Option<PathBuf> = None;
            let mut i = 3;
            while i < args.len() {
                match args[i].as_str() {
                    "--png" => { png_out = args.get(i + 1).map(PathBuf::from); i += 2; }
                    "--json" => { json_out = args.get(i + 1).map(PathBuf::from); i += 2; }
                    "--mode" => { mode = args.get(i + 1).cloned(); i += 2; }
                    "--blocks" => { blocks_out = args.get(i + 1).map(PathBuf::from); i += 2; }
                    other => { eprintln!("未知参数 {other}"); std::process::exit(2); }
                }
            }
            cmd_rx(Path::new(path), png_out.as_deref(), json_out.as_deref(),
                   blocks_out.as_deref(), mode.as_deref())
        }
        Some("stream") => {
            let Some(path) = args.get(2) else { usage(); std::process::exit(2) };
            let mut block_ms = 20.0;
            let mut realtime = false;
            let mut verify = false;
            let mut i = 3;
            while i < args.len() {
                match args[i].as_str() {
                    "--block-ms" => {
                        block_ms = args.get(i + 1).and_then(|s| s.parse().ok()).unwrap_or(20.0);
                        i += 2;
                    }
                    "--realtime" => { realtime = true; i += 1; }
                    "--verify" => { verify = true; i += 1; }
                    other => { eprintln!("未知参数 {other}"); std::process::exit(2); }
                }
            }
            cmd_stream(Path::new(path), block_ms, realtime, verify)
        }
        #[cfg(feature = "audio")]
        Some("audio") => cmd_audio(&args[2..]),
        Some("serve") => {
            let mut port = cp16plus_core::PORT_BASE;
            let mut open = false;
            let mut i = 2;
            while i < args.len() {
                match args[i].as_str() {
                    "--port" => {
                        port = args
                            .get(i + 1)
                            .and_then(|s| s.parse().ok())
                            .unwrap_or(cp16plus_core::PORT_BASE);
                        i += 2;
                    }
                    "--open" => { open = true; i += 1; }
                    "--no-open" => { open = false; i += 1; }
                    other => { eprintln!("未知参数 {other}"); return; }
                }
            }
            std::process::exit(server::serve(port, open));
        }
        Some("check") => {
            let dir = args.get(2).map(PathBuf::from).unwrap_or_else(|| PathBuf::from("samples"));
            cmd_check(&dir)
        }
        Some("tx") => {
            let Some(text) = args.get(2) else { usage(); std::process::exit(2) };
            let mut out = PathBuf::from("out.wav");
            let mut mode = params::DEFAULT_MODE.to_string();
            let mut font = PathBuf::from("fonts/hzk16.cpf");
            let mut call = "BG1ABC".to_string();
            let mut grid = "OM89AB".to_string();
            let mut sync = false;
            let mut i = 3;
            while i < args.len() {
                match args[i].as_str() {
                    "--out" => { out = args.get(i + 1).map(PathBuf::from).unwrap_or(out); i += 2; }
                    "--mode" => { mode = args.get(i + 1).cloned().unwrap_or(mode); i += 2; }
                    "--font" => { font = args.get(i + 1).map(PathBuf::from).unwrap_or(font); i += 2; }
                    "--call" => { call = args.get(i + 1).cloned().unwrap_or(call); i += 2; }
                    "--grid" => { grid = args.get(i + 1).cloned().unwrap_or(grid); i += 2; }
                    "--sync" => { sync = true; i += 1; }
                    other => { eprintln!("未知参数 {other}"); std::process::exit(2); }
                }
            }
            cmd_tx(text, &out, &mode, &font, &call, &grid, sync)
        }
        Some("golden") => {
            let dir = args.get(2).map(PathBuf::from).unwrap_or_else(|| PathBuf::from("tests/golden"));
            golden(&dir)
        }
        None => {
            // 双击启动：先给一页帮助，再直接把界面拉起来（测试版最省事的入口）
            usage();
            let mut port = cp16plus_core::PORT_BASE;
            for p in cp16plus_core::PORT_BASE..cp16plus_core::PORT_BASE + 20 {
                if std::net::TcpListener::bind(("127.0.0.1", p)).is_ok() {
                    port = p;
                    break;
                }
            }
            println!();
            std::process::exit(server::serve(port, true));
        }
        Some("version") => {
            println!("cp16plus {} (proto v{})", env!("CARGO_PKG_VERSION"), params::PROTO_VER);
            println!("fs={} pilot={}/{} cc={} Bd", FS, params::PILOT1, params::PILOT2, CC_BAUD);
            0
        }
        Some(other) => {
            eprintln!("unknown command {other}; try: rx | golden | version");
            usage();
            2
        }
    };
    std::process::exit(code);
}
