//! chronos_payload_archive — the producer's REAL `chain.log` at private-payment scale.
//!
//! Every SIGIL payment is now private and carries a STARK proof (v7: 90,978 B for one
//! input, 109,545 B for two, measured 2026-10-12/13). Today's g2 blocks are ~1.7 KB; a
//! block with eight private payments is ~0.8 MB. This harness writes blocks of that shape
//! through the node's own `ChainLog::append_bytes` + `chain_log::encode_record`, and then
//! times the operations a node performs on that log — restart (`ChainLog::open`), random
//! reads (`get`, `get_by_height`), backfill serving (`get_range`, `get_range_by_height`)
//! and recovery replay (`replay_range`) — verifying every block read back against an
//! independently regenerated copy (each block is a pure function of its height).
//!
//! What is real: ChainLog, the record codec (msgpack + dictionary zstd level 3), the
//! StateMutation::ShieldedSpend shape, a real r1 block as the header/event template, the
//! real note-ciphertext layout. What is modelled: proof bytes are high-entropy filler of
//! the measured lengths (real proofs compress a little; `template` mode measures how much),
//! header roots/signatures are filler (the log never verifies them).
//!
//! Modes (ARCH_MODE):
//!   template — read the template chain.log copy, print the real spend shapes + compressibility
//!   write    — append blocks until ARCH_TARGET_BYTES; pause at each ARCH_CHECKPOINTS mark
//!              (writes `checkpoint-<n>.json`, waits for `probe-done-<n>`)
//!   probe    — open + read + verify; one JSON line to ARCH_RESULTS

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::Instant;

use sigil_node::block::Block;
use sigil_node::chain_log::{self, ChainLog};
use sigil_state::{StateMutation, StateTransition};

fn env_u64(k: &str, d: u64) -> u64 {
    std::env::var(k).ok().and_then(|v| v.replace('_', "").parse::<f64>().ok()).map(|f| f as u64).unwrap_or(d)
}
fn env_s(k: &str, d: &str) -> String {
    std::env::var(k).unwrap_or_else(|_| d.to_string())
}

fn fill(tag: &[u8], h: u64, i: u64, out: &mut [u8]) {
    let mut x = blake3::Hasher::new_keyed(b"sigil-chronos-3tb-1018-payload!!");
    x.update(tag);
    x.update(&h.to_le_bytes());
    x.update(&i.to_le_bytes());
    x.finalize_xof().fill(out);
}
fn e32(tag: &[u8], h: u64, i: u64) -> [u8; 32] {
    let mut b = [0u8; 32];
    fill(tag, h, i, &mut b);
    b
}
fn ent(tag: &[u8], h: u64, i: u64, n: usize) -> Vec<u8> {
    let mut v = vec![0u8; n];
    fill(tag, h, i, &mut v);
    v
}

/// Same layout as a real ciphertext string: every hex digit replaced by a fresh one.
fn reshape_hex(template: &str, h: u64, i: u64) -> String {
    let n = template.len();
    let r = ent(b"ct", h, i, n);
    template
        .bytes()
        .zip(r)
        .map(|(c, x)| if c.is_ascii_hexdigit() && !c.is_ascii_uppercase() { b"0123456789abcdef"[(x & 15) as usize] as char } else { c as char })
        .collect()
}

#[derive(Clone)]
struct Template {
    block: Block,
    base: Vec<StateMutation>,
    ct: String,
    source: String,
}

fn rss_mb() -> f64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| s.lines().find(|l| l.starts_with("VmRSS:")).and_then(|l| l.split_whitespace().nth(1)?.parse::<f64>().ok()))
        .map(|kb| kb / 1024.0)
        .unwrap_or(0.0)
}
fn io_bytes() -> (u64, u64) {
    let s = std::fs::read_to_string("/proc/self/io").unwrap_or_default();
    let g = |k: &str| s.lines().find(|l| l.starts_with(k)).and_then(|l| l.split_whitespace().nth(1)?.parse().ok()).unwrap_or(0);
    (g("read_bytes:"), g("write_bytes:"))
}

fn load_template(dir: &Path) -> Template {
    let mut first: Option<Block> = None;
    let mut with_spend: Option<Block> = None;
    let mut n = 0u64;
    let _ = ChainLog::replay(dir, |b| {
        n += 1;
        if first.is_none() && b.header.height > 0 {
            first = Some(b.clone());
        }
        if with_spend.is_none()
            && b.transition.mutations.iter().any(|m| matches!(m, StateMutation::ShieldedSpend { note_ciphertexts, .. } if note_ciphertexts.iter().any(|c| c.is_some())))
        {
            with_spend = Some(b);
        }
    });
    let (block, source) = match (with_spend, first) {
        (Some(b), _) => {
            let h = b.header.height;
            (b, format!("r1 block {h} (has a real ShieldedSpend with a ciphertext), {n} blocks scanned"))
        }
        (None, Some(b)) => {
            let h = b.header.height;
            (b, format!("r1 block {h} (no spend with ciphertext found in {n} blocks; synthetic ciphertext)"))
        }
        (None, None) => panic!("template chain.log at {} has no blocks", dir.display()),
    };
    let mut ct = String::new();
    let mut base = Vec::new();
    for m in &block.transition.mutations {
        match m {
            StateMutation::ShieldedSpend { note_ciphertexts, .. } => {
                if ct.is_empty() {
                    if let Some(Some(c)) = note_ciphertexts.iter().find(|c| c.is_some()) {
                        ct = c.clone();
                    }
                }
            }
            other => base.push(other.clone()),
        }
    }
    if ct.is_empty() {
        ct = format!("{{\"v\":2,\"epk\":\"{}\",\"ct\":\"{}\"}}", "a".repeat(64), "b".repeat(1100));
    }
    Template { block, base, ct, source }
}

struct Shape {
    n1: usize,
    n2: usize,
    p1: usize,
    p2: usize,
}

/// Block `h`, a pure function of (template, shape, h).
fn gen_block(t: &Template, s: &Shape, h: u64) -> Block {
    let mut b = t.block.clone();
    b.header.height = h;
    b.header.parent_hash = e32(b"parent", h, 0);
    b.header.timestamp_ms = 1_792_000_000_000 + h * 125;
    b.header.wallet_state_root = e32(b"wsr", h, 0);
    b.header.event_log_root = e32(b"elr", h, 0);
    b.header.txs_merkle_root = e32(b"txr", h, 0);
    b.header.vdf_input = e32(b"vdf", h, 0);
    let mut muts = t.base.clone();
    let spend = |i: u64, two: bool, plen: usize| StateMutation::ShieldedSpend {
        anchor: e32(b"anchor", h, i),
        nullifier: e32(b"nf", h, i),
        extra_nullifiers: if two { vec![e32(b"nf2", h, i)] } else { vec![] },
        cm_outs: vec![e32(b"cm0", h, i), e32(b"cm1", h, i)],
        fee: 100_000,
        proof: ent(b"proof", h, i, plen),
        note_ciphertexts: vec![Some(reshape_hex(&t.ct, h, i)), None],
    };
    for i in 0..s.n1 {
        muts.push(spend(i as u64, false, s.p1));
    }
    for i in 0..s.n2 {
        muts.push(spend((s.n1 + i) as u64, true, s.p2));
    }
    b.header.tx_count = (s.n1 + s.n2) as u32 + 1;
    b.transition = StateTransition { at_height: h, mutations: muts };
    b
}

fn pct(v: &mut [f64], p: f64) -> f64 {
    if v.is_empty() {
        return f64::NAN;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[((v.len() as f64 - 1.0) * p).round() as usize]
}

fn shape() -> Shape {
    Shape {
        n1: env_u64("ARCH_SPENDS_1IN", 6) as usize,
        n2: env_u64("ARCH_SPENDS_2IN", 2) as usize,
        p1: env_u64("ARCH_PROOF_1IN", 90_978) as usize,
        p2: env_u64("ARCH_PROOF_2IN", 109_545) as usize,
    }
}

fn mode_template(t: &Template, tdir: &Path) {
    println!("template: {}", t.source);
    println!("  base (non-spend) mutations: {}, events: {}, ciphertext chars: {}", t.base.len(), t.block.events.len(), t.ct.len());
    // Compressibility of the REAL proofs in the template log, through the real codec.
    let mut real = Vec::new();
    let _ = ChainLog::replay(tdir, |b| {
        for m in &b.transition.mutations {
            if let StateMutation::ShieldedSpend { proof, extra_nullifiers, .. } = m {
                real.push((proof.clone(), extra_nullifiers.len() + 1, b.header.height));
            }
        }
    });
    for (p, inputs, h) in real.iter().take(6) {
        let z = zstd::bulk::compress(p, 3).map(|v| v.len()).unwrap_or(0);
        println!("  real proof @h={h}: {inputs} input(s), {} B, zstd-3 → {} B ({:.1} %)", p.len(), z, 100.0 * z as f64 / p.len() as f64);
    }
    let s = shape();
    let b = gen_block(t, &s, 1000);
    let raw = rmp_serde::to_vec_named(&b).unwrap();
    let rec = chain_log::encode_record(&b).unwrap();
    println!("  generated block: msgpack {} B, record {} B ({:.1} %), {} 1-in + {} 2-in spends", raw.len(), rec.len(), 100.0 * rec.len() as f64 / raw.len() as f64, s.n1, s.n2);
    let back = chain_log::decode_record(&rec).expect("decode");
    assert!(back == b, "generated block must round-trip through the real codec");
    let gossip_cap = 1024 * 1024;
    let per = (rec.len() as f64 - 3000.0) / (s.n1 + s.n2) as f64;
    println!("  ≈ {:.0} B per private payment on the wire → {:.1} payments fit a 1 MiB frame", per, (gossip_cap as f64 - 3000.0) / per);
}

/// Where one big block's encode time goes, single-threaded: generate, MessagePack, zstd
/// (the real dictionary compressor), and the whole real `encode_record` for comparison.
fn mode_bench(t: &Template) {
    let s = shape();
    let n = env_u64("ARCH_BENCH_BLOCKS", 200);
    let (mut g, mut m, mut z, mut e, mut d) = (0f64, 0f64, 0f64, 0f64, 0f64);
    let mut bytes = 0usize;
    for h in 0..n {
        let t0 = Instant::now();
        let b = gen_block(t, &s, 10_000 + h);
        g += t0.elapsed().as_secs_f64();
        let t1 = Instant::now();
        let packed = rmp_serde::to_vec_named(&b).unwrap();
        m += t1.elapsed().as_secs_f64();
        let t2 = Instant::now();
        let mut c = zstd::bulk::Compressor::with_dictionary(3, &[]).unwrap();
        let _ = c.compress(&packed).unwrap();
        z += t2.elapsed().as_secs_f64();
        let t3 = Instant::now();
        let rec = chain_log::encode_record(&b).unwrap();
        e += t3.elapsed().as_secs_f64();
        bytes += rec.len();
        let t4 = Instant::now();
        let back = chain_log::decode_record(&rec).unwrap();
        d += t4.elapsed().as_secs_f64();
        assert!(back.header.height == b.header.height);
    }
    let ms = |x: f64| x * 1e3 / n as f64;
    println!(
        "{{\"ev\":\"bench\",\"blocks\":{n},\"record_b\":{},\"gen_ms\":{:.2},\"msgpack_ms\":{:.2},\"zstd3_nodict_ms\":{:.2},\"encode_record_ms\":{:.2},\"decode_record_ms\":{:.2}}}",
        bytes / n as usize, ms(g), ms(m), ms(z), ms(e), ms(d)
    );
}

fn mode_write(t: &Template, dir: &Path) {
    let s = shape();
    let target = env_u64("ARCH_TARGET_BYTES", 3_100_000_000_000);
    let mut marks: Vec<u64> = env_s("ARCH_CHECKPOINTS", "").split(',').filter_map(|x| x.trim().parse::<f64>().ok()).map(|f| f as u64).collect();
    marks.sort();
    let workers = env_u64("ARCH_WORKERS", 4) as usize;
    let batch = env_u64("ARCH_BATCH", 32);
    let t_open = Instant::now();
    let mut log = ChainLog::open(dir).expect("open chain.log");
    let open_s = t_open.elapsed().as_secs_f64();
    let mut h = log.height();
    let path = dir.join("chain.log");
    let mut bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    marks.retain(|m| *m > bytes);
    println!("{{\"ev\":\"write_start\",\"resume_height\":{h},\"bytes\":{bytes},\"open_s\":{open_s:.3},\"template\":{:?}}}", t.source);
    let t0 = Instant::now();
    let (mut win_t, mut win_b, mut win_h) = (Instant::now(), bytes, h);
    let (mut gen_s, mut app_s) = (0f64, 0f64);
    while bytes < target {
        let tg = Instant::now();
        let hs: Vec<u64> = (h..h + batch).collect();
        let mut recs: Vec<Vec<u8>> = vec![Vec::new(); hs.len()];
        std::thread::scope(|sc| {
            for (w, chunk) in recs.chunks_mut(hs.len().div_ceil(workers)).enumerate() {
                let base = w * hs.len().div_ceil(workers);
                let (t, s, hs) = (&t, &s, &hs);
                sc.spawn(move || {
                    for (k, slot) in chunk.iter_mut().enumerate() {
                        *slot = chain_log::encode_record(&gen_block(t, s, hs[base + k])).expect("encode");
                    }
                });
            }
        });
        gen_s += tg.elapsed().as_secs_f64();
        let ta = Instant::now();
        for r in &recs {
            log.append_bytes(r).expect("append");
            bytes += 4 + r.len() as u64;
        }
        app_s += ta.elapsed().as_secs_f64();
        h += batch;
        if h - win_h >= 2048 {
            let dt = win_t.elapsed().as_secs_f64();
            let (rb, wb) = io_bytes();
            println!(
                "{{\"ev\":\"progress\",\"t\":{:.0},\"height\":{h},\"bytes\":{bytes},\"mb_s\":{:.1},\"blk_s\":{:.1},\"gen_encode_s\":{gen_s:.1},\"append_s\":{app_s:.1},\"rss_mb\":{:.0},\"proc_read\":{rb},\"proc_write\":{wb}}}",
                t0.elapsed().as_secs_f64(),
                (bytes - win_b) as f64 / dt / 1e6,
                (h - win_h) as f64 / dt,
                rss_mb()
            );
            win_t = Instant::now();
            win_b = bytes;
            win_h = h;
        }
        if let Some(&m) = marks.first() {
            if bytes >= m {
                marks.remove(0);
                let cp = dir.join(format!("checkpoint-{m}.json"));
                let done = dir.join(format!("probe-done-{m}"));
                let j = format!("{{\"ev\":\"checkpoint\",\"mark\":{m},\"height\":{h},\"bytes\":{bytes},\"t\":{:.0},\"gen_encode_s\":{gen_s:.1},\"append_s\":{app_s:.1}}}", t0.elapsed().as_secs_f64());
                println!("{j}");
                std::fs::write(&cp, &j).expect("checkpoint marker");
                let tw = Instant::now();
                while !done.exists() {
                    std::thread::sleep(std::time::Duration::from_secs(2));
                }
                println!("{{\"ev\":\"resume\",\"mark\":{m},\"paused_s\":{:.0}}}", tw.elapsed().as_secs_f64());
                win_t = Instant::now();
            }
        }
    }
    println!("{{\"ev\":\"write_done\",\"height\":{h},\"bytes\":{bytes},\"t\":{:.0}}}", t0.elapsed().as_secs_f64());
}

/// CANDIDATE FIX, not production code: the exact `ChainLog::open` scan loop with a larger
/// read buffer, to show what a fix would buy next to the real one.
fn scan_offsets_bigbuf(path: &Path, cap: usize) -> (u64, f64) {
    let t = Instant::now();
    let mut r = std::io::BufReader::with_capacity(cap, std::fs::File::open(path).unwrap());
    let mut n = 0u64;
    loop {
        let mut lb = [0u8; 4];
        if r.read_exact(&mut lb).is_err() {
            break;
        }
        let rec = u32::from_le_bytes(lb) as i64;
        n += 1;
        if r.seek_relative(rec).is_err() {
            n -= 1;
            break;
        }
    }
    (n, t.elapsed().as_secs_f64())
}

fn mode_probe(t: &Template, dir: &Path) {
    let s = shape();
    let path = dir.join("chain.log");
    let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    let samples = env_u64("ARCH_SAMPLES", 64);
    let seed = env_u64("ARCH_SEED", 7);
    let mut out = serde_json::Map::new();
    out.insert("bytes".into(), size.into());
    let skip_open = env_s("ARCH_SKIP_REAL_OPEN", "0") == "1";
    let bigbuf = env_u64("ARCH_BIGBUF", 4 << 20) as usize;
    if bigbuf > 0 {
        let (rb0, _) = io_bytes();
        let (n, secs) = scan_offsets_bigbuf(&path, bigbuf);
        let (rb1, _) = io_bytes();
        out.insert("candidate_bigbuf_open_s".into(), secs.into());
        out.insert("candidate_bigbuf_records".into(), n.into());
        out.insert("candidate_bigbuf_read_bytes".into(), (rb1 - rb0).into());
        eprintln!("candidate scan ({} KiB buffer): {n} records in {secs:.1} s", bigbuf >> 10);
    }
    let (rb0, _) = io_bytes();
    let t_open = Instant::now();
    if skip_open {
        out.insert("real_open_s".into(), serde_json::Value::Null);
        println!("{}", serde_json::Value::Object(out));
        return;
    }
    let log = ChainLog::open(dir).expect("open");
    let open_s = t_open.elapsed().as_secs_f64();
    let (rb1, _) = io_bytes();
    let n = log.height();
    eprintln!("real ChainLog::open: {n} records in {open_s:.1} s ({} MB read)", (rb1 - rb0) / 1_000_000);
    out.insert("real_open_s".into(), open_s.into());
    out.insert("records".into(), n.into());
    out.insert("real_open_read_bytes".into(), (rb1 - rb0).into());
    out.insert("rss_after_open_mb".into(), rss_mb().into());
    // 2026-10-22: how the open got its offsets (chain.off vs scan), and an open-only mode for
    // the first open after an upgrade, which pays one full scan to write chain.off.
    let (from_off, scanned) = log.open_stats();
    out.insert("open_from_chain_off".into(), from_off.into());
    out.insert("open_scanned".into(), scanned.into());
    if env_s("ARCH_OPEN_ONLY", "0") == "1" {
        println!("{}", serde_json::Value::Object(out));
        return;
    }
    let pick = |i: u64| -> u64 {
        let mut b = [0u8; 8];
        fill(b"pick", seed, i, &mut b);
        u64::from_le_bytes(b) % n.max(1)
    };
    let mut bad = 0u64;
    let mut lat = Vec::new();
    for i in 0..samples {
        let h = pick(i);
        let tg = Instant::now();
        let got = log.get(h);
        lat.push(tg.elapsed().as_secs_f64() * 1e3);
        if got.as_ref() != Some(&gen_block(t, &s, h)) {
            bad += 1;
            eprintln!("MISMATCH get({h})");
        }
    }
    out.insert("get_ms_p50".into(), pct(&mut lat.clone(), 0.5).into());
    out.insert("get_ms_p99".into(), pct(&mut lat, 0.99).into());
    let mut lat = Vec::new();
    for i in 0..samples.min(16) {
        let h = pick(1000 + i);
        let tg = Instant::now();
        let got = log.get_by_height(h);
        lat.push(tg.elapsed().as_secs_f64() * 1e3);
        if got.as_ref() != Some(&gen_block(t, &s, h)) {
            bad += 1;
            eprintln!("MISMATCH get_by_height({h})");
        }
    }
    out.insert("get_by_height_ms_p50".into(), pct(&mut lat.clone(), 0.5).into());
    out.insert("get_by_height_ms_max".into(), pct(&mut lat, 1.0).into());
    // Backfill serving: a peer asks for 64 blocks starting at an arbitrary height.
    let mut lat = Vec::new();
    let mut lat_aligned = Vec::new();
    for i in 0..6u64 {
        let from = pick(2000 + i).min(n.saturating_sub(65));
        let tg = Instant::now();
        let got = log.get_range_by_height(from, from + 63);
        lat.push(tg.elapsed().as_secs_f64() * 1e3);
        if got.len() != 64 || got.iter().enumerate().any(|(k, b)| *b != gen_block(t, &s, from + k as u64)) {
            bad += 1;
            eprintln!("MISMATCH get_range_by_height({from}..+63): {} blocks", got.len());
        }
        let al = (from / 512) * 512;
        let tg = Instant::now();
        let got = log.get_range_by_height(al, al + 63);
        lat_aligned.push(tg.elapsed().as_secs_f64() * 1e3);
        if got.len() != 64 {
            bad += 1;
        }
    }
    out.insert("range64_by_height_ms_p50".into(), pct(&mut lat.clone(), 0.5).into());
    out.insert("range64_by_height_ms_max".into(), pct(&mut lat, 1.0).into());
    out.insert("range64_aligned_ms_p50".into(), pct(&mut lat_aligned, 0.5).into());
    let from = pick(3000).min(n.saturating_sub(1025));
    let tg = Instant::now();
    let got = log.get_range(from, from + 1023);
    let secs = tg.elapsed().as_secs_f64();
    let mb: usize = got.iter().map(|b| chain_log::encode_record(b).map(|r| r.len()).unwrap_or(0)).sum();
    if got.len() != 1024 || got.iter().enumerate().step_by(97).any(|(k, b)| *b != gen_block(t, &s, from + k as u64)) {
        bad += 1;
        eprintln!("MISMATCH get_range({from}..+1023): {} blocks", got.len());
    }
    out.insert("range1024_s".into(), secs.into());
    out.insert("range1024_mb".into(), (mb as f64 / 1e6).into());
    let lo = n.saturating_sub(4096);
    let tg = Instant::now();
    let mut cnt = 0u64;
    let mut first_bad = None;
    let replayed = ChainLog::replay_range(dir, lo, n - 1, |b| {
        if cnt % 257 == 0 && b != gen_block(t, &s, b.header.height) && first_bad.is_none() {
            first_bad = Some(b.header.height);
        }
        cnt += 1;
    })
    .unwrap_or(0);
    let secs = tg.elapsed().as_secs_f64();
    if replayed != n - lo || first_bad.is_some() {
        bad += 1;
        eprintln!("MISMATCH replay_range: {replayed} of {} (first bad {first_bad:?})", n - lo);
    }
    out.insert("replay4096_s".into(), secs.into());
    out.insert("replay_blk_s".into(), (replayed as f64 / secs).into());
    out.insert("mismatches".into(), bad.into());
    out.insert("rss_end_mb".into(), rss_mb().into());
    println!("{}", serde_json::Value::Object(out));
}

/// Torn tail: half a record appended, as a crash mid-append leaves it. Reopen must drop it
/// and keep every complete block; then the tail is cut back off.
fn mode_torn(t: &Template, dir: &Path) {
    let path = dir.join("chain.log");
    let before = std::fs::metadata(&path).unwrap().len();
    let s = shape();
    let rec = chain_log::encode_record(&gen_block(t, &s, u64::MAX / 2)).unwrap();
    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(&(rec.len() as u32).to_le_bytes()).unwrap();
        f.write_all(&rec[..rec.len() / 2]).unwrap();
    }
    let log = ChainLog::open(dir).unwrap();
    let n = log.height();
    let last_ok = log.get(n - 1).as_ref() == Some(&gen_block(t, &s, n - 1));
    let f = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
    f.set_len(before).unwrap();
    let mut chk = std::fs::File::open(&path).unwrap();
    let end = chk.seek(SeekFrom::End(0)).unwrap();
    println!("{{\"ev\":\"torn_tail\",\"records_after_torn\":{n},\"last_block_ok\":{last_ok},\"restored_bytes\":{end},\"restored_ok\":{}}}", end == before);
}

fn main() {
    let dir = PathBuf::from(env_s("ARCH_DIR", "/home/storage/sigil-scratch/sigil-chronos-3tb-1018/archive"));
    let tdir = PathBuf::from(env_s("ARCH_TEMPLATE", "/home/storage/sigil-scratch/sigil-chronos-3tb-1018/template"));
    let t = load_template(&tdir);
    match env_s("ARCH_MODE", "template").as_str() {
        "template" => mode_template(&t, &tdir),
        "write" => mode_write(&t, &dir),
        "probe" => mode_probe(&t, &dir),
        "torn" => mode_torn(&t, &dir),
        "bench" => mode_bench(&t),
        m => panic!("unknown ARCH_MODE {m}"),
    }
}
