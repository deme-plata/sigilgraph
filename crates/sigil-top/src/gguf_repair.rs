//! Repair a published GGUF so ollama can load it: drop the multi-token-prediction ("MTP", `nextn`) helper
//! layer that some Qwen 3.5 conversions carry as one extra block.
//!
//! Why (measured 2026-11-26 on Epsilon, ollama 0.20.2): `chatgpt1/qwythos-9b-claude-mythos-5-1m-abliterated`
//! (a Qwen 3.5 9B fine-tune, the model sigil-top offers as `mythos`) declares `qwen35.block_count = 33` and
//! `qwen35.nextn_predict_layers = 1`: 32 real layers plus `blk.32`, a draft head used only for speculative
//! decoding. Ollama treats `blk.32` as an ordinary layer and refuses the file ("qwen3next: layer 32 missing
//! attn_qkv/attn_gate projections"). Copying the file without `blk.32`, with block_count 32 and nextn 0, makes
//! it load; every kept tensor is copied byte for byte. This is a port of the lane script
//! `flux-scratch/cockpit-ai-model-1126/gguf_drop_mtp.py` and produces the SAME bytes (same sha256, checked by
//! the ignored test `real_upload_repairs_to_the_known_digest`).
//!
//! Nothing is written to disk here: [`Plan::reader`] produces the repaired file as a stream, so it can be hashed
//! and then sent straight to ollama's blob store (`POST /api/blobs/sha256:<hex>`) without a temporary copy.
//!
//! GGUF v3: magic "GGUF", u32 version, u64 tensor count, u64 key/value count, the key/values, the tensor table
//! (name, dims, type, offset relative to the data section), padding to `general.alignment` (default 32), data.

use std::fs::File;
use std::io::{self, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

const MAGIC: &[u8; 4] = b"GGUF";
/// Architectures this repair has been proven on. Anything else is refused rather than guessed at.
const SUPPORTED_ARCH: &[&str] = &["qwen35"];
/// Hostile-file limits (audit 11-26): a header bigger than this, or an alignment above this, is not a model.
const MAX_HEADER: u64 = 256 << 20;
const MAX_ALIGN: u64 = 1 << 16;

#[derive(Debug, Clone)]
struct Kv {
    key: String,
    ty: u32,
    /// Byte range of the whole entry (key, type, value) in the source header, copied raw when unchanged.
    start: usize,
    end: usize,
    int: Option<u64>,
}

#[derive(Debug, Clone)]
struct Tensor {
    name: String,
    dims: Vec<u64>,
    ty: u32,
    off: u64,
}

/// What a GGUF file says about itself, and (after [`Plan::drop_mtp`]) the repaired layout.
#[derive(Debug)]
pub(crate) struct Plan {
    src: PathBuf,
    version: u32,
    header_src: Vec<u8>,
    kvs: Vec<Kv>,
    tensors: Vec<Tensor>,
    data_start: u64,
    file_len: u64,
    align: u64,
    pub(crate) arch: String,
    pub(crate) blocks: u64,
    pub(crate) nextn: u64,
    // Set by drop_mtp():
    header_out: std::sync::Arc<Vec<u8>>,
    pieces: Vec<(u64, u64)>,
    pub(crate) dropped: Vec<String>,
    pub(crate) out_len: u64,
}

fn bad(msg: impl Into<String>) -> io::Error { io::Error::new(io::ErrorKind::InvalidData, msg.into()) }

/// A reader that counts where it is, so key/value entries can be copied by byte range.
struct Pos<R: Read> { inner: R, pos: u64 }

impl<R: Read> Pos<R> {
    fn take_vec(&mut self, n: u64) -> io::Result<Vec<u8>> {
        if n > 1 << 26 { return Err(bad(format!("a {n}-byte field is not a GGUF header field"))); }
        let mut b = vec![0u8; n as usize];
        self.inner.read_exact(&mut b)?;
        self.pos += n;
        Ok(b)
    }
    fn skip(&mut self, n: u64) -> io::Result<()> {
        let got = io::copy(&mut (&mut self.inner).take(n), &mut io::sink())?;
        if got != n { return Err(io::ErrorKind::UnexpectedEof.into()); }
        self.pos += n;
        Ok(())
    }
    fn u32(&mut self) -> io::Result<u32> { let b = self.take_vec(4)?; Ok(u32::from_le_bytes(b.try_into().unwrap())) }
    fn u64(&mut self) -> io::Result<u64> { let b = self.take_vec(8)?; Ok(u64::from_le_bytes(b.try_into().unwrap())) }
    fn string(&mut self) -> io::Result<String> {
        let n = self.u64()?;
        String::from_utf8(self.take_vec(n)?).map_err(|_| bad("a GGUF key or name is not UTF-8"))
    }
}

/// Width in bytes of a GGUF scalar type, `None` for string (8) and array (9).
fn scalar_width(ty: u32) -> Option<u64> {
    match ty {
        0 | 1 | 7 => Some(1),
        2 | 3 => Some(2),
        4 | 5 | 6 => Some(4),
        10 | 11 | 12 => Some(8),
        _ => None,
    }
}

fn skip_value<R: Read>(r: &mut Pos<R>, ty: u32, depth: u32) -> io::Result<()> {
    if depth > 4 { return Err(bad("GGUF arrays nested too deep")); }
    if let Some(w) = scalar_width(ty) { return r.skip(w); }
    match ty {
        8 => { let n = r.u64()?; r.skip(n) }
        9 => {
            let et = r.u32()?;
            let n = r.u64()?;
            if let Some(w) = scalar_width(et) {
                return r.skip(n.checked_mul(w).ok_or_else(|| bad("GGUF array too large"))?);
            }
            for _ in 0..n { skip_value(r, et, depth + 1)?; }
            Ok(())
        }
        t => Err(bad(format!("unknown GGUF value type {t}"))),
    }
}

/// An unsigned integer value of an integer-typed key (negative or non-integer → `None`).
fn int_value(ty: u32, b: &[u8]) -> Option<u64> {
    match ty {
        0 => Some(b[0] as u64),
        1 => (b[0] as i8 >= 0).then(|| b[0] as u64),
        2 => Some(u16::from_le_bytes([b[0], b[1]]) as u64),
        3 => { let v = i16::from_le_bytes([b[0], b[1]]); (v >= 0).then(|| v as u64) }
        4 => Some(u32::from_le_bytes(b[..4].try_into().ok()?) as u64),
        5 => { let v = i32::from_le_bytes(b[..4].try_into().ok()?); (v >= 0).then(|| v as u64) }
        10 => Some(u64::from_le_bytes(b[..8].try_into().ok()?)),
        11 => { let v = i64::from_le_bytes(b[..8].try_into().ok()?); (v >= 0).then(|| v as u64) }
        _ => None,
    }
}

/// The same integer in the same GGUF type, or `None` when it does not fit that type.
fn encode_int(ty: u32, v: u64) -> Option<Vec<u8>> {
    Some(match ty {
        0 => vec![u8::try_from(v).ok()?],
        1 => vec![i8::try_from(v).ok()? as u8],
        2 => u16::try_from(v).ok()?.to_le_bytes().to_vec(),
        3 => i16::try_from(v).ok()?.to_le_bytes().to_vec(),
        4 => u32::try_from(v).ok()?.to_le_bytes().to_vec(),
        5 => i32::try_from(v).ok()?.to_le_bytes().to_vec(),
        10 => v.to_le_bytes().to_vec(),
        11 => i64::try_from(v).ok()?.to_le_bytes().to_vec(),
        _ => return None,
    })
}

fn put_string(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(&(s.len() as u64).to_le_bytes());
    out.extend_from_slice(s.as_bytes());
}

fn align_up(x: u64, a: u64) -> u64 { x.div_ceil(a) * a }

impl Plan {
    /// Read a GGUF's header. Reads only the header (a few MB), never the weights.
    pub(crate) fn inspect(path: &Path) -> Result<Plan, String> {
        Self::inspect_io(path).map_err(|e| format!("{}: {e}", path.display()))
    }

    fn inspect_io(path: &Path) -> io::Result<Plan> {
        let f = File::open(path)?;
        let file_len = f.metadata()?.len();
        let mut r = Pos { inner: BufReader::with_capacity(1 << 20, f), pos: 0 };
        let mut header_src: Vec<u8> = Vec::new();
        let magic = r.take_vec(4)?;
        if magic.as_slice() != MAGIC { return Err(bad("not a GGUF file (bad magic)")); }
        let version = r.u32()?;
        if version != 3 { return Err(bad(format!("GGUF version {version}; only version 3 is supported"))); }
        let n_tensors = r.u64()?;
        let n_kv = r.u64()?;
        if n_tensors > 1 << 20 || n_kv > 1 << 20 { return Err(bad("implausible GGUF table sizes")); }
        // Re-read the raw header later by range: keep the file, re-open for the copy.
        let mut kvs = Vec::with_capacity(n_kv as usize);
        let (mut arch, mut align) = (String::new(), 32u64);
        for _ in 0..n_kv {
            let start = r.pos as usize;
            let key = r.string()?;
            let ty = r.u32()?;
            let (mut int, mut text) = (None, None);
            if let Some(w) = scalar_width(ty) {
                let b = r.take_vec(w)?;
                int = int_value(ty, &b);
            } else if ty == 8 {
                text = Some(r.string()?);
            } else {
                skip_value(&mut r, ty, 0)?;
            }
            if key == "general.architecture" { arch = text.clone().unwrap_or_default(); }
            if key == "general.alignment" { align = int.filter(|a| *a > 0 && a.is_power_of_two() && *a <= MAX_ALIGN).ok_or_else(|| bad("bad general.alignment"))?; }
            kvs.push(Kv { key, ty, start, end: r.pos as usize, int });
            if r.pos > MAX_HEADER { return Err(bad("GGUF header too large")); }
        }
        let mut tensors = Vec::with_capacity(n_tensors as usize);
        for _ in 0..n_tensors {
            let name = r.string()?;
            let nd = r.u32()?;
            if nd > 8 { return Err(bad(format!("tensor {name} has {nd} dimensions"))); }
            let dims = (0..nd).map(|_| r.u64()).collect::<io::Result<Vec<_>>>()?;
            let ty = r.u32()?;
            let off = r.u64()?;
            tensors.push(Tensor { name, dims, ty, off });
            if r.pos > MAX_HEADER { return Err(bad("GGUF header too large")); }
        }
        // the block count of THIS architecture (a vision tower's `*.block_count` must not decide which layer goes)
        let blocks = kvs.iter().find(|k| k.key == format!("{arch}.block_count")).and_then(|k| k.int);
        let data_start = align_up(r.pos, align);
        // The raw header bytes, for copying unchanged entries exactly.
        let mut f = File::open(path)?;
        header_src.resize(r.pos as usize, 0);
        f.read_exact(&mut header_src)?;
        for t in &tensors {
            if t.off % align != 0 || data_start.checked_add(t.off).map_or(true, |end| end > file_len) {
                return Err(bad(format!("tensor {} sits at a bad offset", t.name)));
            }
        }
        let nextn = kvs.iter().find(|k| k.key == format!("{arch}.nextn_predict_layers")).and_then(|k| k.int).unwrap_or(0);
        Ok(Plan {
            src: path.to_path_buf(), version, header_src, kvs, tensors, data_start, file_len, align, arch,
            blocks: blocks.unwrap_or(0), nextn,
            header_out: std::sync::Arc::new(Vec::new()), pieces: Vec::new(), dropped: Vec::new(), out_len: 0,
        })
    }

    /// True when the file carries an MTP layer that ollama would read as an ordinary one.
    pub(crate) fn needs_repair(&self) -> bool { self.nextn > 0 }

    /// Plan the copy without the MTP layer. Refuses anything it has not been proven on.
    pub(crate) fn drop_mtp(mut self) -> Result<Plan, String> {
        if !SUPPORTED_ARCH.contains(&self.arch.as_str()) {
            return Err(format!("architecture `{}` — the MTP repair is proven only for {SUPPORTED_ARCH:?}", self.arch));
        }
        if self.nextn != 1 {
            return Err(format!("{} MTP layers declared; the repair handles exactly one", self.nextn));
        }
        if self.blocks < 2 { return Err("block_count missing or too small".into()); }
        let mtp = self.blocks - 1;
        let prefix = format!("blk.{mtp}.");
        let by_off = {
            let mut v: Vec<&Tensor> = self.tensors.iter().collect();
            v.sort_by_key(|t| t.off);
            v
        };
        let mut size = std::collections::HashMap::new();
        for (i, t) in by_off.iter().enumerate() {
            let end = by_off.get(i + 1).map_or(self.file_len - self.data_start, |n| n.off);
            size.insert(t.name.clone(), end - t.off);
        }
        self.dropped = self.tensors.iter().filter(|t| t.name.starts_with(&prefix)).map(|t| t.name.clone()).collect();
        if self.dropped.is_empty() { return Err(format!("no tensors named {prefix}* although nextn_predict_layers = 1")); }
        let kept_by_off: Vec<&Tensor> = by_off.into_iter().filter(|t| !t.name.starts_with(&prefix)).collect();
        let mut newoff = std::collections::HashMap::new();
        let mut cur = 0u64;
        let mut pieces = Vec::with_capacity(kept_by_off.len());
        for t in &kept_by_off {
            if cur % self.align != 0 { return Err(format!("tensor {} would land unaligned", t.name)); }
            newoff.insert(t.name.clone(), cur);
            let n = size[&t.name];
            pieces.push((self.data_start + t.off, n));
            cur += n;
        }
        let kept = self.tensors.len() - self.dropped.len();
        let mut h = Vec::with_capacity(self.header_src.len());
        h.extend_from_slice(MAGIC);
        h.extend_from_slice(&self.version.to_le_bytes());
        h.extend_from_slice(&(kept as u64).to_le_bytes());
        h.extend_from_slice(&(self.kvs.len() as u64).to_le_bytes());
        let (bc_key, nx_key) = (format!("{}.block_count", self.arch), format!("{}.nextn_predict_layers", self.arch));
        for kv in &self.kvs {
            let new_val = if kv.key == bc_key { Some(mtp) } else if kv.key == nx_key { Some(0) } else { None };
            match new_val {
                Some(v) => {
                    put_string(&mut h, &kv.key);
                    h.extend_from_slice(&kv.ty.to_le_bytes());
                    h.extend(encode_int(kv.ty, v).ok_or_else(|| format!("{} is not an integer key", kv.key))?);
                }
                None => h.extend_from_slice(&self.header_src[kv.start..kv.end]),
            }
        }
        for t in self.tensors.iter().filter(|t| !t.name.starts_with(&prefix)) {
            put_string(&mut h, &t.name);
            h.extend_from_slice(&(t.dims.len() as u32).to_le_bytes());
            for d in &t.dims { h.extend_from_slice(&d.to_le_bytes()); }
            h.extend_from_slice(&t.ty.to_le_bytes());
            h.extend_from_slice(&newoff[&t.name].to_le_bytes());
        }
        let padded = align_up(h.len() as u64, self.align) as usize;
        h.resize(padded, 0);
        self.out_len = h.len() as u64 + cur;
        self.header_out = std::sync::Arc::new(h);
        self.pieces = pieces;
        Ok(self)
    }

    /// The repaired file as a stream (only after [`Plan::drop_mtp`]).
    pub(crate) fn reader(&self) -> Result<RepairedReader, String> {
        if self.header_out.is_empty() { return Err("drop_mtp() has not planned a repair".into()); }
        let file = File::open(&self.src).map_err(|e| format!("{}: {e}", self.src.display()))?;
        Ok(RepairedReader {
            header: std::sync::Arc::clone(&self.header_out), hpos: 0, file: BufReader::with_capacity(4 << 20, file),
            pieces: self.pieces.clone(), next: 0, left: 0,
        })
    }

    /// sha256 of the repaired file (what ollama names the blob), reading the source once.
    /// `progress(done, total)` is called about every 64 MiB.
    pub(crate) fn sha256_hex(&self, mut progress: impl FnMut(u64, u64)) -> Result<String, String> {
        use sha2::Digest;
        let mut r = self.reader()?;
        let mut h = sha2::Sha256::new();
        let mut buf = vec![0u8; 1 << 20];
        let (mut done, mut last) = (0u64, 0u64);
        loop {
            let n = r.read(&mut buf).map_err(|e| format!("reading {}: {e}", self.src.display()))?;
            if n == 0 { break; }
            h.update(&buf[..n]);
            done += n as u64;
            if done - last >= 64 << 20 { last = done; progress(done, self.out_len); }
        }
        if done != self.out_len { return Err(format!("read {done} bytes, planned {}", self.out_len)); }
        progress(done, self.out_len);
        Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
    }
}

/// The repaired file, produced on the fly from the source: the new header, then each kept tensor's bytes.
pub(crate) struct RepairedReader {
    header: std::sync::Arc<Vec<u8>>,
    hpos: usize,
    file: BufReader<File>,
    pieces: Vec<(u64, u64)>,
    next: usize,
    left: u64,
}

impl Read for RepairedReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() { return Ok(0); }
        if self.hpos < self.header.len() {
            let n = buf.len().min(self.header.len() - self.hpos);
            buf[..n].copy_from_slice(&self.header[self.hpos..self.hpos + n]);
            self.hpos += n;
            return Ok(n);
        }
        while self.left == 0 {
            let Some(&(at, len)) = self.pieces.get(self.next) else { return Ok(0) };
            self.file.seek(SeekFrom::Start(at))?;
            self.left = len;
            self.next += 1;
        }
        let want = (buf.len() as u64).min(self.left) as usize;
        let n = self.file.read(&mut buf[..want])?;
        if n == 0 { return Err(io::ErrorKind::UnexpectedEof.into()); }
        self.left -= n as u64;
        Ok(n)
    }
}

/// A tiny qwen35-shaped test GGUF (for the setup tests that drive the whole repair against a stand-in ollama).
#[cfg(test)]
pub(crate) fn test_gguf(arch: &str, nextn: u32) -> Vec<u8> { tests::tiny_gguf(arch, nextn).0 }

#[cfg(test)]
mod tests {
    use super::*;

    /// A tiny qwen35-shaped GGUF: 3 blocks where blk.2 is the MTP head, a string array, a float, and a tensor
    /// table whose order differs from the data order (as real files do).
    pub(super) fn tiny_gguf(arch: &str, nextn: u32) -> (Vec<u8>, Vec<(&'static str, Vec<u8>)>) {
        let align = 32u64;
        let mut kv = Vec::new();
        let mut n_kv = 0u64;
        let s = |kv: &mut Vec<u8>, k: &str| { put_string(kv, k); };
        s(&mut kv, "general.architecture"); kv.extend(8u32.to_le_bytes()); put_string(&mut kv, arch); n_kv += 1;
        s(&mut kv, "general.alignment"); kv.extend(4u32.to_le_bytes()); kv.extend(32u32.to_le_bytes()); n_kv += 1;
        s(&mut kv, &format!("{arch}.block_count")); kv.extend(4u32.to_le_bytes()); kv.extend(3u32.to_le_bytes()); n_kv += 1;
        s(&mut kv, &format!("{arch}.nextn_predict_layers")); kv.extend(4u32.to_le_bytes()); kv.extend(nextn.to_le_bytes()); n_kv += 1;
        s(&mut kv, "tokenizer.ggml.tokens"); kv.extend(9u32.to_le_bytes()); kv.extend(8u32.to_le_bytes()); kv.extend(3u64.to_le_bytes());
        for t in ["a", "bb", "ccc"] { put_string(&mut kv, t); } n_kv += 1;
        s(&mut kv, "x.rope_scale"); kv.extend(6u32.to_le_bytes()); kv.extend(1.5f32.to_le_bytes()); n_kv += 1;
        s(&mut kv, "x.ids"); kv.extend(9u32.to_le_bytes()); kv.extend(5u32.to_le_bytes()); kv.extend(2u64.to_le_bytes()); kv.extend(7i32.to_le_bytes()); kv.extend((-1i32).to_le_bytes()); n_kv += 1;
        // data order: token_embd, blk.0, blk.2 (mtp), blk.1, blk.2.b (mtp), output_norm
        let data: Vec<(&'static str, Vec<u8>)> = vec![
            ("token_embd.weight", vec![1u8; 40]),
            ("blk.0.attn_qkv.weight", vec![2u8; 64]),
            ("blk.2.nextn.eh_proj.weight", vec![9u8; 32]),
            ("blk.1.attn_qkv.weight", vec![3u8; 33]),
            ("blk.2.attn_norm.weight", vec![8u8; 32]),
            ("output_norm.weight", vec![4u8; 7]),
        ];
        let mut offs = Vec::new();
        let mut cur = 0u64;
        for (_, d) in &data { offs.push(cur); cur = align_up(cur + d.len() as u64, align); }
        // table order differs from data order
        let order = [5usize, 0, 2, 1, 4, 3];
        let mut table = Vec::new();
        for &i in &order {
            put_string(&mut table, data[i].0);
            table.extend(1u32.to_le_bytes()); table.extend((data[i].1.len() as u64).to_le_bytes());
            table.extend(0u32.to_le_bytes()); table.extend(offs[i].to_le_bytes());
        }
        let mut f = Vec::new();
        f.extend(MAGIC); f.extend(3u32.to_le_bytes()); f.extend((data.len() as u64).to_le_bytes()); f.extend(n_kv.to_le_bytes());
        f.extend(kv); f.extend(table);
        f.resize(align_up(f.len() as u64, align) as usize, 0);
        for (i, (_, d)) in data.iter().enumerate() {
            let base = f.len();
            f.extend(d);
            let next = if i + 1 < data.len() { align_up((base + d.len()) as u64, align) as usize } else { base + d.len() };
            f.resize(next, 0);
        }
        (f, data)
    }

    fn write_tmp(bytes: &[u8], name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("sigil-gguf-test-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("m.gguf");
        std::fs::write(&p, bytes).unwrap();
        p
    }

    fn tensor_bytes(p: &Plan, file: &[u8], name: &str) -> Vec<u8> {
        let mut by_off: Vec<&Tensor> = p.tensors.iter().collect();
        by_off.sort_by_key(|t| t.off);
        let i = by_off.iter().position(|t| t.name == name).unwrap();
        let start = (p.data_start + by_off[i].off) as usize;
        let end = by_off.get(i + 1).map_or(file.len(), |n| (p.data_start + n.off) as usize);
        file[start..end].to_vec()
    }

    #[test]
    fn drops_the_mtp_block_and_keeps_everything_else_byte_for_byte() {
        let (src, data) = tiny_gguf("qwen35", 1);
        let path = write_tmp(&src, "ok");
        let plan = Plan::inspect(&path).unwrap();
        assert_eq!((plan.arch.as_str(), plan.blocks, plan.nextn), ("qwen35", 3, 1));
        assert!(plan.needs_repair());
        let plan = plan.drop_mtp().unwrap();
        assert_eq!(plan.dropped, vec!["blk.2.nextn.eh_proj.weight", "blk.2.attn_norm.weight"]);
        let mut out = Vec::new();
        plan.reader().unwrap().read_to_end(&mut out).unwrap();
        assert_eq!(out.len() as u64, plan.out_len);

        let out_path = write_tmp(&out, "out");
        let again = Plan::inspect(&out_path).unwrap();
        assert_eq!((again.blocks, again.nextn), (2, 0), "block_count and nextn rewritten");
        assert!(!again.needs_repair());
        let names: Vec<&str> = again.tensors.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["output_norm.weight", "token_embd.weight", "blk.0.attn_qkv.weight", "blk.1.attn_qkv.weight"],
                   "kept tensors stay in the original table order");
        for (name, d) in data.iter().filter(|(n, _)| !n.starts_with("blk.2.")) {
            let got = tensor_bytes(&again, &out, name);
            assert_eq!(&got[..d.len()], d.as_slice(), "{name} copied byte for byte");
        }
        for t in &again.tensors { assert_eq!(t.off % 32, 0, "{} aligned", t.name); }
        // unchanged key/values (strings, arrays, floats, negative ints) come through as the same bytes
        for key in ["general.architecture", "general.alignment", "tokenizer.ggml.tokens", "x.rope_scale", "x.ids"] {
            let a = plan.kvs.iter().find(|k| k.key == key).unwrap();
            let b = again.kvs.iter().find(|k| k.key == key).unwrap();
            assert_eq!(&plan.header_src[a.start..a.end], &again.header_src[b.start..b.end], "{key}");
        }
        // the hash pass reads the same bytes the reader gives
        use sha2::Digest;
        let want: String = sha2::Sha256::digest(&out).iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(plan.sha256_hex(|_, _| {}).unwrap(), want);
    }

    #[test]
    fn a_file_without_mtp_needs_nothing_and_unknown_shapes_are_refused() {
        let (src, _) = tiny_gguf("qwen35", 0);
        let p = Plan::inspect(&write_tmp(&src, "nomtp")).unwrap();
        assert!(!p.needs_repair());
        assert!(p.drop_mtp().unwrap_err().contains("exactly one"));

        let (src, _) = tiny_gguf("llama", 1);
        let p = Plan::inspect(&write_tmp(&src, "llama")).unwrap();
        assert!(p.drop_mtp().unwrap_err().contains("proven only for"));

        let mut src = tiny_gguf("qwen35", 1).0;
        src[0] = b'X';
        assert!(Plan::inspect(&write_tmp(&src, "magic")).unwrap_err().contains("bad magic"));

        let src = tiny_gguf("qwen35", 1).0;
        assert!(Plan::inspect(&write_tmp(&src[..60], "short")).is_err(), "a cut-off header is an error, not a plan");
    }

    /// Audit 11-26: a hostile header cannot make the repair abort, wrap around or allocate absurdly.
    #[test]
    fn hostile_alignment_and_offsets_are_refused() {
        // alignment 2^40: was accepted (any power of two) and later asked for a 1 TiB allocation
        let mut src = tiny_gguf("qwen35", 1).0;
        let key = b"general.alignment";
        let at = src.windows(key.len()).position(|w| w == key).unwrap() + key.len() + 4;   // past the key and its type
        src[at..at + 4].copy_from_slice(&(1u32 << 31).to_le_bytes());
        assert!(Plan::inspect(&write_tmp(&src, "align")).unwrap_err().contains("alignment"));
        // a tensor offset near u64::MAX: `data_start + off` wrapped in release builds and passed the bounds check
        let mut src = tiny_gguf("qwen35", 1).0;
        let name = b"output_norm.weight";
        let at = src.windows(name.len()).position(|w| w == name).unwrap() + name.len() + 4 + 8 + 4;   // past dims and type
        src[at..at + 8].copy_from_slice(&(u64::MAX - 63).to_le_bytes());
        assert!(Plan::inspect(&write_tmp(&src, "wrap")).unwrap_err().contains("bad offset"));
    }

    /// The real upload, if this machine has it: `SIGIL_GGUF_SRC=<blob> SIGIL_GGUF_SHA256=<hex>`. On Epsilon the
    /// blob is ollama's sha256-81607afa… and the expected digest is fa0ca6cd…, which is what the Python lane
    /// script produced and what `qwythos-9b:flux` already uses.
    #[test]
    #[ignore]
    fn real_upload_repairs_to_the_known_digest() {
        let src = std::env::var("SIGIL_GGUF_SRC").expect("SIGIL_GGUF_SRC");
        let want = std::env::var("SIGIL_GGUF_SHA256").expect("SIGIL_GGUF_SHA256");
        let plan = Plan::inspect(Path::new(&src)).unwrap();
        assert_eq!((plan.blocks, plan.nextn), (33, 1));
        let plan = plan.drop_mtp().unwrap();
        eprintln!("dropped {} tensors, output {} bytes", plan.dropped.len(), plan.out_len);
        assert_eq!(plan.sha256_hex(|_, _| {}).unwrap(), want);
    }
}
