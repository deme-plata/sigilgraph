//! aether.rs — **the node's flux-aether artifact store, over HTTP**.
//!
//! This is what the SIGIL OS terminal talks to. It exists so a user can *see*
//! that a write reached a real node and came back content-addressed, rather
//! than being told so.
//!
//! ## What aether is, precisely — it is NOT a POSIX filesystem
//!
//! The word "filesystem" invites the wrong mental model, so: there are no
//! directories, no paths, no rename, no in-place edit, no `seek`. What exists is
//!
//!   **a name → (version, content-hash) map, plus a content-addressed blob store.**
//!
//! `flux_aether::Manifest` is that map, and it is an **LWW-map CRDT**: `merge` is
//! commutative, associative and idempotent, and `VersionEntry::dominates` is a
//! *total* order (ts, then semver, then content hash). Two nodes that exchange
//! entries in any order converge on byte-identical state. That property is what
//! makes it a mesh filesystem rather than a directory.
//!
//! A "write" is therefore not `open(2)`+`write(2)`. It is: hash the bytes,
//! erasure-code and encrypt them into shards, then **author a signed
//! `VersionEntry`** naming that content hash. The name is a label that resolves
//! to a hash; the hash is the truth.
//!
//! ## Why this is not a toy
//!
//! `sigil-node/src/snapshot.rs` already stores **the entire chain** as
//! Reed-Solomon aether shards under `$SIGIL_DB_PATH/aether` — 3.2 GB on Epsilon
//! as of 2026-09-03. The primitives these endpoints call (`shard_file`,
//! `reassemble`, `rs_shard`) are the same ones the chain's durability depends
//! on. A user writing a note through the terminal is exercising production code.
//!
//! ## What is deliberately NOT claimed
//!
//! 1. **Cross-node convergence is not wired.** `flux_aether::sync_pair` and
//!    `gossip_until_converged` exist, are tested, and have **no caller** in
//!    sigil-node — only `fluxc-mcp`'s handler calls them. So a write here lands
//!    on *this* node's store and does not yet gossip to peers. `GET
//!    /v1/aether/root` returns the manifest root precisely so that, when the
//!    gossip lane does land, convergence becomes checkable by comparing roots
//!    across nodes. Until then the API says `"gossip": "not wired"` in its own
//!    response rather than letting a UI imply otherwise.
//! 2. **The signature is a BLAKE3 MAC, not a public-key signature.**
//!    `NodeIdentity` keys a keyed-hash with its own secret; the crate's own
//!    comment calls this "the SQIsign seam". It authenticates an entry to the
//!    node that wrote it. It does **not** let a third party verify authorship.
//!    Reported as `sig_kind: "blake3-mac"` so nobody reads it as more.
//!
//! ## Safety — this is a public write endpoint on a money node
//!
//! Three independent guards, because "users can write to the node" is also the
//! description of a disk-exhaustion attack:
//!
//! * **A separate directory.** The user store is `<base>/aether-user`, never
//!   `<base>/aether`. A user write physically cannot land among chain snapshot
//!   shards. This is the single most important line in the file.
//! * **Hard caps** on name length, blob size, entry count and total bytes —
//!   checked before any write touches the disk.
//! * **A kill switch.** `SIGIL_AETHER_READONLY=1` makes every write refuse while
//!   reads keep working.
//!
//! Reserved names: anything starting `sys/` is refused, so the demo namespace can
//! never shadow a system artifact if one is added later.

use axum::extract::{Query, State};
use axum::Json;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use flux_aether::{reassemble, shard_file, Manifest, NodeIdentity, Ver, VersionEntry};

use crate::{ApiResponse, AppState};

/// Longest artifact name. Long enough for `user/viktor/notes/2026-09-03`.
pub const MAX_NAME_LEN: usize = 64;
/// Largest single blob. A terminal writes notes, not videos.
pub const MAX_CONTENT_BYTES: usize = 64 * 1024;
/// Most named entries the user store will hold.
pub const MAX_ENTRIES: usize = 4096;
/// Ceiling on the whole user store.
pub const MAX_STORE_BYTES: u64 = 256 * 1024 * 1024;
/// Shard size handed to `shard_file`. Small blobs still get several shards, so
/// the erasure coding is exercised rather than degenerating to one shard.
const SHARD_SIZE: usize = 4 * 1024;
/// Writes allowed per client per minute.
const WRITES_PER_MIN: usize = 20;

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn hex32(h: &[u8; 32]) -> String {
    let mut s = String::with_capacity(64);
    for b in h { s.push_str(&format!("{b:02x}")); }
    s
}

/// Is `name` acceptable? Lowercase alphanumerics plus `. _ - /`, no `..`, no
/// leading/trailing slash, no `sys/` prefix.
///
/// The `..` and absolute-path checks matter even though names are not paths:
/// the blob store is on a real filesystem, and a name that survives into any
/// future path-joining logic must not be able to escape. Rejecting it at the
/// boundary is cheaper than auditing every consumer.
pub fn name_ok(name: &str) -> Result<(), String> {
    if name.is_empty() { return Err("name is empty".into()); }
    if name.len() > MAX_NAME_LEN { return Err(format!("name longer than {MAX_NAME_LEN} chars")); }
    if name.starts_with('/') || name.ends_with('/') { return Err("name cannot start or end with '/'".into()); }
    if name.contains("//") { return Err("name cannot contain '//'".into()); }
    if name.split('/').any(|seg| seg == ".." || seg == "." || seg.is_empty()) {
        return Err("name segments cannot be empty, '.' or '..'".into());
    }
    if !name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-' | '/')) {
        return Err("name may use a-z 0-9 . _ - / only".into());
    }
    if name == "sys" || name.starts_with("sys/") {
        return Err("'sys/' is reserved for system artifacts".into());
    }
    Ok(())
}

/// Per-client write budget — a fixed window, deliberately crude. A precise
/// limiter is not the point; refusing an unbounded write loop is.
#[derive(Default)]
struct RateLimiter {
    /// client key → (window start secs, writes in this window)
    seen: HashMap<String, (u64, usize)>,
}

impl RateLimiter {
    fn allow(&mut self, who: &str) -> bool {
        let now = now_secs();
        // Bound the map itself: an attacker rotating client keys must not be
        // able to grow it without limit.
        if self.seen.len() > 8192 { self.seen.retain(|_, (start, _)| now.saturating_sub(*start) < 60); }
        let e = self.seen.entry(who.to_string()).or_insert((now, 0));
        if now.saturating_sub(e.0) >= 60 { *e = (now, 0); }
        if e.1 >= WRITES_PER_MIN { return false; }
        e.1 += 1;
        true
    }
}

/// The node's user-writable aether store: a persisted CRDT manifest plus a
/// content-addressed blob directory.
pub struct AetherStore {
    dir: PathBuf,
    identity: NodeIdentity,
    node_name: String,
    manifest: RwLock<Manifest>,
    limiter: Mutex<RateLimiter>,
}

impl AetherStore {
    /// Open (or create) the user store.
    ///
    /// `base` is the node's data directory. The store goes in `aether-user`,
    /// **not** `aether` — see the safety note in the module docs.
    pub fn open(base: &Path, node_name: &str) -> Self {
        let dir = base.join("aether-user");
        let _ = std::fs::create_dir_all(dir.join("blobs"));
        let manifest = Self::load_manifest(&dir).unwrap_or_default();
        Self {
            dir,
            identity: NodeIdentity::from_seed(node_name.as_bytes()),
            node_name: node_name.to_string(),
            manifest: RwLock::new(manifest),
            limiter: Mutex::new(RateLimiter::default()),
        }
    }

    /// Open using the same env vars the node's snapshot code reads, so the user
    /// store always sits beside the chain store without ever being inside it.
    pub fn open_from_env() -> Self {
        let base = std::env::var("SIGIL_AETHER_USER_BASE")
            .or_else(|_| std::env::var("SIGIL_DB_PATH"))
            .unwrap_or_else(|_| "./data".to_string());
        let node = std::env::var("SIGIL_NODE_ID").unwrap_or_else(|_| "sigil-node".to_string());
        Self::open(Path::new(&base), &node)
    }

    fn manifest_path(dir: &Path) -> PathBuf { dir.join("manifest.json") }

    fn load_manifest(dir: &Path) -> Option<Manifest> {
        let body = std::fs::read_to_string(Self::manifest_path(dir)).ok()?;
        serde_json::from_str(&body).ok()
    }

    /// Persist the manifest via tmp+rename so a crash mid-write cannot leave a
    /// truncated manifest that loses every name in the store.
    fn save_manifest(&self, m: &Manifest) -> Result<(), String> {
        let body = serde_json::to_vec_pretty(m).map_err(|e| e.to_string())?;
        let tmp = self.dir.join("manifest.json.tmp");
        std::fs::write(&tmp, &body).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, Self::manifest_path(&self.dir)).map_err(|e| e.to_string())
    }

    fn blob_path(&self, content: &[u8; 32]) -> PathBuf {
        self.dir.join("blobs").join(hex32(content))
    }

    /// Bytes currently held in the blob directory.
    fn store_bytes(&self) -> u64 {
        std::fs::read_dir(self.dir.join("blobs"))
            .map(|rd| rd.filter_map(|e| e.ok()).filter_map(|e| e.metadata().ok()).map(|m| m.len()).sum())
            .unwrap_or(0)
    }

    /// Are writes enabled? Reads are never gated.
    fn writable() -> bool {
        !matches!(std::env::var("SIGIL_AETHER_READONLY").as_deref(), Ok("1") | Ok("true"))
    }

    /// The blob encryption key. Per-node, derived from the identity seed — the
    /// shards on disk are ciphertext, which is aether's actual contract. It is
    /// NOT a per-user secret and is not presented as one.
    fn blob_key(&self) -> [u8; 32] {
        *blake3::hash(format!("aether-user-blob/{}", self.node_name).as_bytes()).as_bytes()
    }

    /// Write bytes: shard + encrypt + content-address, then author a signed
    /// `VersionEntry` under `name`. Returns the entry actually stored.
    pub fn put(&self, name: &str, content: &[u8], ver: Option<Ver>) -> Result<PutOutcome, String> {
        if !Self::writable() { return Err("aether writes are disabled on this node (SIGIL_AETHER_READONLY=1)".into()); }
        name_ok(name)?;
        if content.is_empty() { return Err("content is empty".into()); }
        if content.len() > MAX_CONTENT_BYTES {
            return Err(format!("content is {} bytes, limit is {MAX_CONTENT_BYTES}", content.len()));
        }

        let key = self.blob_key();
        let producer = self.identity.id;
        let (fb, shards) = shard_file(content, SHARD_SIZE, &key, producer);

        // Refuse on capacity BEFORE writing anything, and treat a re-put of
        // identical bytes as free — it is the same blob, already on disk.
        let already = self.blob_path(&fb.content_root).exists();
        {
            let m = self.manifest.read().map_err(|_| "manifest lock poisoned")?;
            if !already {
                if m.len() >= MAX_ENTRIES && m.digest().get(name).is_none() {
                    return Err(format!("store is full ({MAX_ENTRIES} entries)"));
                }
                if self.store_bytes().saturating_add(content.len() as u64) > MAX_STORE_BYTES {
                    return Err(format!("store is full ({} MiB)", MAX_STORE_BYTES / 1024 / 1024));
                }
            }
        }

        // The blob is stored as the ORIGINAL bytes plus the shard set beside it.
        // Storing the plaintext blob is what makes `cat` cheap; the shards are
        // what make the durability property real. Both, not one.
        if !already {
            std::fs::write(self.blob_path(&fb.content_root), content).map_err(|e| e.to_string())?;
            let sdir = self.dir.join("shards").join(hex32(&fb.content_root));
            std::fs::create_dir_all(&sdir).map_err(|e| e.to_string())?;
            // The FileBlock must be persisted, not reconstructed: it carries the
            // per-file `nonce`, and without it the shard bytes cannot be
            // decrypted. Losing it turns the shards into noise.
            std::fs::write(sdir.join("block.json"),
                serde_json::to_vec(&fb).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
            std::fs::write(sdir.join("shards.json"),
                serde_json::to_vec(&shards).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
        }

        // Version: caller's, else bump the patch of whatever is there.
        let ver = ver.unwrap_or_else(|| {
            let m = self.manifest.read().ok();
            let cur = m.as_ref().and_then(|m| m.entries_for(&[name.to_string()]).into_iter().next());
            match cur { Some(e) => Ver::new(e.ver.major, e.ver.minor, e.ver.patch + 1), None => Ver::new(1, 0, 0) }
        });

        let entry = self.identity.author(name, ver, fb.content_root, now_secs());
        let (root, count) = {
            let mut m = self.manifest.write().map_err(|_| "manifest lock poisoned")?;
            m.put(entry.clone());
            let snapshot = m.clone();
            drop(m);
            self.save_manifest(&snapshot)?;
            (hex32(&snapshot.root()), snapshot.len())
        };

        Ok(PutOutcome {
            name: entry.name.clone(),
            version: entry.ver.display(),
            content_hash: hex32(&entry.content),
            shard_merkle_root: hex32(&fb.shard_merkle_root),
            shards: shards.len(),
            bytes: content.len(),
            deduplicated: already,
            producer: hex32(&entry.producer),
            sig: hex32(&entry.sig),
            sig_kind: "blake3-mac".into(),
            fingerprint: hex32(&entry.fingerprint()),
            manifest_root: root,
            manifest_entries: count,
        })
    }

    /// Read the bytes behind a name. Prefers the plaintext blob; falls back to
    /// RS-reassembling from shards, which is the path that proves the erasure
    /// coding actually works.
    pub fn cat(&self, name: &str) -> Result<(VersionEntry, Vec<u8>, &'static str), String> {
        name_ok(name)?;
        let entry = self.stat(name)?;
        let p = self.blob_path(&entry.content);
        if let Ok(b) = std::fs::read(&p) { return Ok((entry, b, "blob")); }

        let (fb, shards) = self.load_shards(&entry.content)
            .ok_or_else(|| format!("'{name}' names content this node does not hold"))?;
        let bytes = reassemble(&fb, &shards, &self.blob_key())
            .map_err(|e| format!("reassemble failed: {e:?}"))?;
        Ok((entry, bytes, "shards"))
    }

    /// Load the persisted `FileBlock` + shard set for a content hash.
    fn load_shards(&self, content: &[u8; 32]) -> Option<(flux_aether::FileBlock, Vec<flux_aether::Shard>)> {
        let sdir = self.dir.join("shards").join(hex32(content));
        let fb: flux_aether::FileBlock = serde_json::from_slice(&std::fs::read(sdir.join("block.json")).ok()?).ok()?;
        let shards: Vec<flux_aether::Shard> = serde_json::from_slice(&std::fs::read(sdir.join("shards.json")).ok()?).ok()?;
        Some((fb, shards))
    }

    /// **Prove the erasure coding, on demand.** Reassemble the content while
    /// deliberately WITHHOLDING one data shard, so recovery has to come from
    /// the parity shard. Nothing is deleted — the withheld shard is simply not
    /// handed to `reassemble`.
    ///
    /// This exists because "your data is erasure-coded" is a claim, and a claim
    /// a user cannot check is worth very little. Here they can check it.
    pub fn recover(&self, name: &str) -> Result<RecoverOutcome, String> {
        name_ok(name)?;
        let entry = self.stat(name)?;
        let (fb, all) = self.load_shards(&entry.content)
            .ok_or_else(|| format!("'{name}' has no shard set on this node"))?;
        if fb.k < 2 {
            return Err(format!(
                "'{name}' is {} bytes — it fits in a single data shard, so there is nothing for parity to recover. \
                 Write something larger than {SHARD_SIZE} bytes to see K-of-N recovery.",
                fb.len
            ));
        }
        // Drop data shard 0; keep the rest, including parity.
        let withheld: Vec<flux_aether::Shard> =
            all.iter().filter(|s| !(s.index == 0 && !s.is_parity)).cloned().collect();
        let bytes = reassemble(&fb, &withheld, &self.blob_key())
            .map_err(|e| format!("recovery FAILED: {e:?}"))?;
        let ok = *blake3::hash(&bytes).as_bytes() == entry.content;
        Ok(RecoverOutcome {
            name: name.to_string(),
            k: fb.k,
            n: fb.n,
            shards_total: all.len(),
            shards_used: withheld.len(),
            withheld_index: 0,
            bytes_recovered: bytes.len(),
            content_hash: hex32(&entry.content),
            rehash_matches: ok,
        })
    }

    /// The winning `VersionEntry` for a name.
    pub fn stat(&self, name: &str) -> Result<VersionEntry, String> {
        name_ok(name)?;
        let m = self.manifest.read().map_err(|_| "manifest lock poisoned")?;
        m.entries_for(&[name.to_string()])
            .into_iter()
            .next()
            .ok_or_else(|| format!("no such artifact: '{name}'"))
    }

    /// Every entry, newest first.
    pub fn ls(&self) -> Vec<VersionEntry> {
        let m = match self.manifest.read() { Ok(m) => m, Err(_) => return Vec::new() };
        let names: Vec<String> = m.digest().keys().cloned().collect();
        let mut v = m.entries_for(&names);
        v.sort_by(|a, b| b.ts.cmp(&a.ts).then_with(|| a.name.cmp(&b.name)));
        v
    }

    /// Manifest root — the value that will make cross-node convergence checkable.
    pub fn root(&self) -> String {
        self.manifest.read().map(|m| hex32(&m.root())).unwrap_or_default()
    }

    fn allow_write(&self, who: &str) -> bool {
        self.limiter.lock().map(|mut l| l.allow(who)).unwrap_or(false)
    }
}

// ── wire types ──────────────────────────────────────────────────────────────

/// What a successful `put` produced.
#[derive(Debug, Serialize, Deserialize)]
pub struct PutOutcome {
    /// The artifact name written.
    pub name: String,
    /// Version stored, e.g. `"v1.0.0"`.
    pub version: String,
    /// BLAKE3 of the content — the commitment. This is the address.
    pub content_hash: String,
    /// Merkle root over the shard set (≈ a block's `txs_merkle_root`).
    pub shard_merkle_root: String,
    /// How many shards the content was split into.
    pub shards: usize,
    /// Content length in bytes.
    pub bytes: usize,
    /// True when these exact bytes were already stored — content addressing
    /// means an identical re-put costs no new disk.
    pub deduplicated: bool,
    /// Authoring node's public id.
    pub producer: String,
    /// The entry's signature.
    pub sig: String,
    /// **`"blake3-mac"`** — a keyed hash, not a public-key signature. Named
    /// explicitly so no UI can present it as third-party-verifiable.
    pub sig_kind: String,
    /// Entry identity fingerprint (everything but the sig).
    pub fingerprint: String,
    /// Manifest root after the write.
    pub manifest_root: String,
    /// Entry count after the write.
    pub manifest_entries: usize,
}

/// What `recover` proved.
#[derive(Debug, Serialize, Deserialize)]
pub struct RecoverOutcome {
    /// Artifact name.
    pub name: String,
    /// Data shards (any K reconstruct).
    pub k: u32,
    /// Total shards, K data + parity.
    pub n: u32,
    /// Shards held on disk.
    pub shards_total: usize,
    /// Shards actually handed to `reassemble`.
    pub shards_used: usize,
    /// Index of the data shard deliberately withheld.
    pub withheld_index: u32,
    /// Bytes recovered.
    pub bytes_recovered: usize,
    /// The content address.
    pub content_hash: String,
    /// **The verdict.** True means the bytes rebuilt from a short shard set
    /// re-hash to the original content address — erasure recovery worked.
    pub rehash_matches: bool,
}

/// One row of `ls`.
#[derive(Debug, Serialize, Deserialize)]
pub struct LsEntry {
    /// Artifact name.
    pub name: String,
    /// Version string.
    pub version: String,
    /// Content hash (the address).
    pub content_hash: String,
    /// Authoring node id.
    pub producer: String,
    /// Authorship time, unix seconds.
    pub ts: u64,
}

impl From<&VersionEntry> for LsEntry {
    fn from(e: &VersionEntry) -> Self {
        Self {
            name: e.name.clone(),
            version: e.ver.display(),
            content_hash: hex32(&e.content),
            producer: hex32(&e.producer),
            ts: e.ts,
        }
    }
}

/// `GET /v1/aether/ls`
#[derive(Debug, Serialize, Deserialize)]
pub struct LsResponse {
    /// The entries, newest first.
    pub entries: Vec<LsEntry>,
    /// Manifest root over all of them.
    pub manifest_root: String,
    /// Bytes held in the blob store.
    pub store_bytes: u64,
    /// Whether writes are currently accepted.
    pub writable: bool,
}

/// `GET /v1/aether/root` — the store's identity and honest limits.
#[derive(Debug, Serialize, Deserialize)]
pub struct RootResponse {
    /// Manifest root, hex.
    pub manifest_root: String,
    /// Number of named entries.
    pub entries: usize,
    /// This node's aether id.
    pub node_id: String,
    /// Bytes in the blob store.
    pub store_bytes: u64,
    /// Cap on the blob store.
    pub max_store_bytes: u64,
    /// Cap on one blob.
    pub max_content_bytes: usize,
    /// Cap on entry count.
    pub max_entries: usize,
    /// Whether writes are accepted right now.
    pub writable: bool,
    /// **`"not wired"`** — `sync_pair`/`gossip_until_converged` exist in
    /// flux-aether and have no caller in sigil-node, so a write here does not
    /// yet reach peers. Stated in the payload so a UI cannot imply otherwise.
    pub gossip: String,
    /// Signature scheme actually in use.
    pub sig_kind: String,
}

/// `GET /v1/aether/cat`
#[derive(Debug, Serialize, Deserialize)]
pub struct CatResponse {
    /// Artifact name.
    pub name: String,
    /// Version string.
    pub version: String,
    /// Content hash.
    pub content_hash: String,
    /// Content, when it is valid UTF-8.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Content base64, when it is not UTF-8.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base64: Option<String>,
    /// Byte length.
    pub bytes: usize,
    /// `"blob"` if read directly, `"shards"` if RS-reassembled.
    pub read_via: String,
    /// True when the returned bytes re-hash to `content_hash`. The client is
    /// meant to check this itself; the server reporting it is a convenience,
    /// not the proof.
    pub verified: bool,
}

/// `GET /v1/aether/stat`
#[derive(Debug, Serialize, Deserialize)]
pub struct StatResponse {
    /// The entry.
    pub entry: LsEntry,
    /// Identity fingerprint.
    pub fingerprint: String,
    /// The entry's signature.
    pub sig: String,
    /// Signature scheme.
    pub sig_kind: String,
    /// Shards on disk for this content.
    pub shards_on_disk: usize,
    /// Whether the plaintext blob is present.
    pub blob_present: bool,
}

/// Body of `POST /v1/aether/put`.
#[derive(Debug, Deserialize)]
pub struct PutRequest {
    /// Artifact name, e.g. `user/notes/hello`.
    pub name: String,
    /// Content as text. Exactly one of `content`/`base64` is required.
    #[serde(default)]
    pub content: Option<String>,
    /// Content as base64, for non-text bytes.
    #[serde(default)]
    pub base64: Option<String>,
    /// Explicit version; defaults to a patch bump of what is there.
    #[serde(default)]
    pub version: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct NameQuery {
    pub name: String,
}

fn parse_ver(s: &str) -> Option<Ver> {
    let t = s.trim().trim_start_matches('v');
    let mut it = t.split('.');
    let a = it.next()?.parse().ok()?;
    let b = it.next().unwrap_or("0").parse().ok()?;
    let c = it.next().unwrap_or("0").parse().ok()?;
    if it.next().is_some() { return None; }
    Some(Ver::new(a, b, c))
}

// ── handlers ────────────────────────────────────────────────────────────────

#[flux_api_macros::api(GET, "/v1/aether/root", summary = "Aether store identity, manifest root and honest limits")]
pub async fn aether_root(State(st): State<AppState>) -> Json<ApiResponse<RootResponse>> {
    let a = &st.aether;
    ApiResponse::ok(RootResponse {
        manifest_root: a.root(),
        entries: a.ls().len(),
        node_id: hex32(&a.identity.id),
        store_bytes: a.store_bytes(),
        max_store_bytes: MAX_STORE_BYTES,
        max_content_bytes: MAX_CONTENT_BYTES,
        max_entries: MAX_ENTRIES,
        writable: AetherStore::writable(),
        gossip: "not wired".into(),
        sig_kind: "blake3-mac".into(),
    })
}

#[flux_api_macros::api(GET, "/v1/aether/ls", summary = "List every named artifact in the node's aether store")]
pub async fn aether_ls(State(st): State<AppState>) -> Json<ApiResponse<LsResponse>> {
    let entries = st.aether.ls();
    ApiResponse::ok(LsResponse {
        entries: entries.iter().map(LsEntry::from).collect(),
        manifest_root: st.aether.root(),
        store_bytes: st.aether.store_bytes(),
        writable: AetherStore::writable(),
    })
}

#[flux_api_macros::api(GET, "/v1/aether/stat", summary = "One artifact's version entry, signature and shard status")]
pub async fn aether_stat(State(st): State<AppState>, Query(q): Query<NameQuery>) -> Json<ApiResponse<StatResponse>> {
    let e = match st.aether.stat(&q.name) { Ok(e) => e, Err(m) => return ApiResponse::err(m) };
    let shards = st.aether.load_shards(&e.content).map(|(_, v)| v.len()).unwrap_or(0);
    let blob_present = st.aether.blob_path(&e.content).exists();
    ApiResponse::ok(StatResponse {
        fingerprint: hex32(&e.fingerprint()),
        sig: hex32(&e.sig),
        sig_kind: "blake3-mac".into(),
        shards_on_disk: shards,
        blob_present,
        entry: LsEntry::from(&e),
    })
}

#[flux_api_macros::api(GET, "/v1/aether/cat", summary = "Read the content behind an artifact name")]
pub async fn aether_cat(State(st): State<AppState>, Query(q): Query<NameQuery>) -> Json<ApiResponse<CatResponse>> {
    let (entry, bytes, via) = match st.aether.cat(&q.name) { Ok(v) => v, Err(m) => return ApiResponse::err(m) };
    let verified = *blake3::hash(&bytes).as_bytes() == entry.content;
    let (text, b64) = match String::from_utf8(bytes.clone()) {
        Ok(s) => (Some(s), None),
        Err(_) => (None, Some(b64_encode(&bytes))),
    };
    ApiResponse::ok(CatResponse {
        name: entry.name.clone(),
        version: entry.ver.display(),
        content_hash: hex32(&entry.content),
        text,
        base64: b64,
        bytes: bytes.len(),
        read_via: via.into(),
        verified,
    })
}

#[flux_api_macros::api(GET, "/v1/aether/recover", summary = "Rebuild an artifact from a DELIBERATELY short shard set, proving the erasure coding")]
pub async fn aether_recover(State(st): State<AppState>, Query(q): Query<NameQuery>) -> Json<ApiResponse<RecoverOutcome>> {
    match st.aether.recover(&q.name) {
        Ok(o) => ApiResponse::ok(o),
        Err(m) => ApiResponse::err(m),
    }
}

#[flux_api_macros::api(POST, "/v1/aether/put", summary = "Write bytes into the node's aether store; returns the content address")]
pub async fn aether_put(State(st): State<AppState>, Json(req): Json<PutRequest>) -> Json<ApiResponse<PutOutcome>> {
    // One shared client key: this endpoint sits behind q-flux, so a real
    // per-IP key needs the forwarded header. Until that is threaded through,
    // ONE global budget is the honest choice — it is a cap that actually holds,
    // rather than a per-IP cap that any client can reset by lying.
    if !st.aether.allow_write("global") {
        return ApiResponse::err(format!("rate limited — {WRITES_PER_MIN} writes per minute"));
    }
    let bytes = match (&req.content, &req.base64) {
        (Some(t), None) => t.as_bytes().to_vec(),
        (None, Some(b)) => match b64_decode(b) { Some(v) => v, None => return ApiResponse::err("base64 is not valid") },
        (Some(_), Some(_)) => return ApiResponse::err("send content OR base64, not both"),
        (None, None) => return ApiResponse::err("send content or base64"),
    };
    let ver = match req.version.as_deref() {
        Some(s) => match parse_ver(s) { Some(v) => Some(v), None => return ApiResponse::err("version must look like 1.2.3") },
        None => None,
    };
    match st.aether.put(&req.name, &bytes, ver) {
        Ok(o) => ApiResponse::ok(o),
        Err(m) => ApiResponse::err(m),
    }
}

// ── base64 (no new dependency for four lines of table lookup) ───────────────

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn b64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity((data.len() + 2) / 3 * 4);
    for c in data.chunks(3) {
        let b = [c[0], *c.get(1).unwrap_or(&0), *c.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(B64[(n >> 18 & 63) as usize] as char);
        out.push(B64[(n >> 12 & 63) as usize] as char);
        out.push(if c.len() > 1 { B64[(n >> 6 & 63) as usize] as char } else { '=' });
        out.push(if c.len() > 2 { B64[(n & 63) as usize] as char } else { '=' });
    }
    out
}

fn b64_decode(s: &str) -> Option<Vec<u8>> {
    let mut acc = 0u32;
    let mut bits = 0u32;
    let mut out = Vec::new();
    for ch in s.bytes() {
        if ch == b'=' || ch.is_ascii_whitespace() { continue; }
        let v = B64.iter().position(|&c| c == ch)? as u32;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_store(tag: &str) -> AetherStore {
        let base = std::env::temp_dir().join(format!("sigil-aether-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        AetherStore::open(&base, "test-node")
    }

    #[test]
    fn user_store_is_never_the_chain_snapshot_dir() {
        // THE safety property of this module. `sigil-node/src/snapshot.rs` uses
        // `<base>/aether`; a user write must be physically incapable of landing
        // there. If this assertion ever fails, a public endpoint can corrupt
        // chain durability shards.
        let s = tmp_store("isolation");
        assert!(s.dir.ends_with("aether-user"));
        assert!(!s.dir.ends_with("aether"));
    }

    #[test]
    fn put_then_cat_roundtrips_and_verifies() {
        let s = tmp_store("roundtrip");
        let out = s.put("user/notes/hello", b"hello aether", None).unwrap();
        assert_eq!(out.version, "v1.0.0");
        assert!(out.shards >= 1);
        let (entry, bytes, _via) = s.cat("user/notes/hello").unwrap();
        assert_eq!(bytes, b"hello aether");
        // The content hash IS the address — re-hashing the bytes must reproduce it.
        assert_eq!(*blake3::hash(&bytes).as_bytes(), entry.content);
        assert_eq!(hex32(&entry.content), out.content_hash);
    }

    #[test]
    fn identical_bytes_deduplicate() {
        let s = tmp_store("dedup");
        let a = s.put("user/a", b"same bytes", None).unwrap();
        let b = s.put("user/b", b"same bytes", None).unwrap();
        assert_eq!(a.content_hash, b.content_hash, "content addressing means one address");
        assert!(!a.deduplicated && b.deduplicated);
    }

    #[test]
    fn reput_bumps_the_patch_version() {
        let s = tmp_store("bump");
        assert_eq!(s.put("user/x", b"one", None).unwrap().version, "v1.0.0");
        assert_eq!(s.put("user/x", b"two", None).unwrap().version, "v1.0.1");
        // and the name now resolves to the NEW content (LWW)
        let (_, bytes, _) = s.cat("user/x").unwrap();
        assert_eq!(bytes, b"two");
    }

    #[test]
    fn explicit_version_is_honoured() {
        let s = tmp_store("explicit");
        let o = s.put("user/v", b"z", parse_ver("2.3.4")).unwrap();
        assert_eq!(o.version, "v2.3.4");
    }

    #[test]
    fn manifest_root_changes_on_write_and_survives_reopen() {
        let base = std::env::temp_dir().join(format!("sigil-aether-persist-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let root_after = {
            let s = AetherStore::open(&base, "test-node");
            let empty = s.root();
            s.put("user/persisted", b"durable", None).unwrap();
            let after = s.root();
            assert_ne!(empty, after, "a write must move the manifest root");
            after
        };
        // Reopening must reload the manifest — otherwise every restart silently
        // loses every name a user wrote.
        let s2 = AetherStore::open(&base, "test-node");
        assert_eq!(s2.root(), root_after);
        assert_eq!(s2.cat("user/persisted").unwrap().1, b"durable");
    }

    #[test]
    fn recover_rebuilds_from_a_short_shard_set() {
        let s = tmp_store("recover");
        // Bigger than one shard, so there are >= 2 data shards + parity and
        // parity actually has something to reconstruct.
        let content: Vec<u8> = (0..(SHARD_SIZE * 3 + 17)).map(|i| (i % 251) as u8).collect();
        s.put("user/big-note", &content, None).unwrap();
        let r = s.recover("user/big-note").unwrap();
        assert!(r.k >= 2, "expected multiple data shards, got k={}", r.k);
        assert!(r.shards_used < r.shards_total, "a shard must actually be withheld");
        assert_eq!(r.bytes_recovered, content.len());
        assert!(r.rehash_matches, "content rebuilt from k-1 data shards + parity must re-hash to its address");
    }

    #[test]
    fn recover_says_why_it_cannot_prove_anything_for_tiny_content() {
        // A blob that fits in one shard has no recovery story. Saying so beats
        // reporting a vacuous success.
        let s = tmp_store("recover-tiny");
        s.put("user/tiny", b"hi", None).unwrap();
        let e = s.recover("user/tiny").unwrap_err();
        assert!(e.contains("single data shard"), "{e}");
    }

    #[test]
    fn hostile_names_are_refused() {
        for bad in [
            "", "/leading", "trailing/", "a//b", "../escape", "a/../b", "a/./b",
            "sys", "sys/thing", "UPPER", "has space", "emoji🙂", "semi;colon",
        ] {
            assert!(name_ok(bad).is_err(), "{bad:?} should be refused");
        }
        for good in ["a", "user/notes/hello", "user/2026-09-03", "a_b.c-d", "user/deep/nest/ing"] {
            assert!(name_ok(good).is_ok(), "{good:?} should be allowed");
        }
        assert!(name_ok(&"x".repeat(MAX_NAME_LEN)).is_ok());
        assert!(name_ok(&"x".repeat(MAX_NAME_LEN + 1)).is_err());
    }

    #[test]
    fn oversize_and_empty_content_refused() {
        let s = tmp_store("caps");
        assert!(s.put("user/empty", b"", None).is_err());
        let big = vec![b'x'; MAX_CONTENT_BYTES + 1];
        let e = s.put("user/big", &big, None).unwrap_err();
        assert!(e.contains("limit is"), "{e}");
        // exactly at the limit is fine
        assert!(s.put("user/edge", &vec![b'y'; MAX_CONTENT_BYTES], None).is_ok());
    }

    #[test]
    fn readonly_env_blocks_writes_but_not_reads() {
        let s = tmp_store("readonly");
        s.put("user/before", b"data", None).unwrap();
        std::env::set_var("SIGIL_AETHER_READONLY", "1");
        let e = s.put("user/after", b"nope", None).unwrap_err();
        assert!(e.contains("disabled"), "{e}");
        // reads keep working — the kill switch is not an outage
        assert_eq!(s.cat("user/before").unwrap().1, b"data");
        std::env::remove_var("SIGIL_AETHER_READONLY");
        assert!(s.put("user/after", b"yes", None).is_ok());
    }

    #[test]
    fn rate_limiter_refuses_a_write_loop() {
        let mut l = RateLimiter::default();
        for i in 0..WRITES_PER_MIN { assert!(l.allow("who"), "write {i} should pass"); }
        assert!(!l.allow("who"), "the {}th write in a minute must be refused", WRITES_PER_MIN + 1);
    }

    #[test]
    fn stat_and_ls_agree_with_what_was_written() {
        let s = tmp_store("ls");
        s.put("user/one", b"1", None).unwrap();
        s.put("user/two", b"22", None).unwrap();
        let names: Vec<String> = s.ls().iter().map(|e| e.name.clone()).collect();
        assert_eq!(names.len(), 2);
        assert!(names.contains(&"user/one".to_string()));
        let st = s.stat("user/two").unwrap();
        assert_eq!(st.name, "user/two");
        assert!(s.stat("user/missing").is_err());
    }

    #[test]
    fn base64_roundtrips_including_non_utf8() {
        for case in [vec![], vec![0u8], vec![0xff, 0xfe, 0xfd], (0u8..=255).collect::<Vec<_>>()] {
            let enc = b64_encode(&case);
            assert_eq!(b64_decode(&enc).unwrap(), case, "failed for {} bytes", case.len());
        }
        assert!(b64_decode("!!!!").is_none());
    }

    #[test]
    fn version_parsing_is_strict() {
        assert_eq!(parse_ver("1.2.3"), Some(Ver::new(1, 2, 3)));
        assert_eq!(parse_ver("v1.2.3"), Some(Ver::new(1, 2, 3)));
        assert_eq!(parse_ver("2"), Some(Ver::new(2, 0, 0)));
        assert_eq!(parse_ver("1.2.3.4"), None);
        assert_eq!(parse_ver("x.y.z"), None);
    }
}
