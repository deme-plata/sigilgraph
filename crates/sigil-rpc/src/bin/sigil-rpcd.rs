//! sigil-rpcd — std-only HTTP transport over the sigil-rpc money keystone.
//!
//! No axum/tokio: raw TcpListener + a tiny HTTP/1.1 handler (FLUXFOOD-light,
//! same std-only ethos as sigil-top). Wraps the verified keystone fns behind a
//! shared Arc<RwLock<Node>> with thread-per-connection (concurrent reads,
//! serialized writes — fixes the single-thread accept-loop throughput wall).
//! Every state change still goes through the 21M-cap
//! chokepoint (commit_state_transition) — the transport adds zero trust.
//!
//!   GET  /health
//!   GET  /balance?wallet=HEX64&token=HEX64        (token omitted = NATIVE)
//!   GET  /pools                                    (the bootstrapped pool)
//!   POST /swap   {"from":HEX,"pool":HEX,"dir":"AtoB|BtoA","amount_in":N,"min_out":N}
//!   POST /mine   {"miner":HEX,"header":"str","nonce":N,"difficulty":N,"reward":N}
//!   GET  /api/v1/mining/challenge?wallet=HEX64   → dual-lane Challenge (flux-miner)
//!   POST /api/v1/mining/submit  {Submission}     → SubmitResult (BLAKE4 Φ + VDF Ω)
//!   POST /credit {"operator_pool":HEX,"pool_amount":N,"verifiers":[HEX,...]}

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::thread;

/// false until the background full-text index build finishes (see main()).
/// `/readyz` reports it; the money/chain routes serve regardless.
static INDEX_READY: AtomicBool = AtomicBool::new(false);

use flux_miner::client::{check_submission, Challenge, Submission, SubmitResult};
use flux_vdf::ModSquaring;
use sigil_dex::SwapDirection;
use sigil_rpc::nation;
use sigil_rpc::onboard::{self, VerifiedRegistry};
use sigil_rpc::{credit_light_verifiers, credit_share, execute_swap, submit_share};
use sigil_state::{
    commit_state_transition, PoolId, PoolState, SigilState, StateMutation, StateTransition,
    TokenId, WalletId, NATIVE,
};

/// The live SIGIL DEX economy. Beyond raw state, the daemon tracks the token +
/// pool registries (so the swarm + dashboard can enumerate them), the verified-
/// student registry (the earning gate), and the citizen roster.
struct Node {
    state: SigilState,
    /// Global state-event counter (DEX swaps + mining both bump it) — used for
    /// state-transition ordering, NOT for emission/difficulty.
    height: u64,
    /// Mining-chain height: a clean, contiguous 0,1,2,… sequence bumped ONLY by an
    /// accepted dual-lane block. Drives the challenge, block_reward (halving), retarget,
    /// and the tip — so swap volume can't distort the emission schedule (red-team:
    /// height-conflation), and mining blocks form a peer-verifiable chain.
    block_height: u64,
    /// Student / verified-user registry — onboarded users that may earn.
    students: VerifiedRegistry,
    /// symbol → token-id (predefined + dynamically deployed via /deploy_token).
    tokens: Vec<(String, TokenId)>,
    /// label → pool-id (predefined + dynamically created).
    pools: Vec<(String, PoolId)>,
    /// attested nation citizens (wallet roster, for /nation/citizens).
    citizens: Vec<WalletId>,
    /// flux-native explorer index: every tx appended as a searchable HistoryEntry
    /// (flux-db persist + flux-search TF-IDF). None if the store couldn't open.
    history: Option<flux_history::HistoryStore>,
    /// Mining chain tip — folds every accepted dual-lane block. The per-height
    /// challenge seed = BLAKE3(tip ‖ height), so the challenge for a future height
    /// is UNKNOWN until the prior block is accepted → no precompute (fix #1).
    tip_hash: [u8; 32],
    /// BLAKE4 Lane-A difficulty (target = u64::MAX >> bits). Retargeted from real
    /// block times every RETARGET_WINDOW blocks (fix #2).
    bits: u32,
    retarget_anchor_ts: u64,
    retarget_anchor_height: u64,
    /// flux-db handle for state persistence — the money (SigilState) + chain
    /// (height/tip/bits) are snapshotted here so they survive a restart.
    statedb: Option<flux_db::Database>,
    /// Per-wallet highest accepted request nonce — the replay guard for the
    /// authenticated mutating routes (a request's nonce must strictly exceed the
    /// last one this wallet used). Persisted in the snapshot so replay protection
    /// survives a restart. See `sigil_rpc::auth`.
    auth_nonces: std::collections::HashMap<WalletId, u64>,
    /// Per-IP onboarding rate-limit window: client-ip → (window_start_ms, count).
    /// In-memory (resets on restart, fine) — bounds the /onboard faucet against
    /// sybil drain. The faucet itself is finite (debits OPERATOR, see /onboard).
    onboard_rl: std::collections::HashMap<String, (u64, u32)>,
    /// LANE-R time-based emission anchor: the genesis block's timestamp (µs since the unix
    /// epoch). Set ONCE at genesis, persisted, never changed — emission halves on WALL-CLOCK
    /// time elapsed since this, not on block height. All nodes must agree on it.
    genesis_ts_us: u128,
    /// The previous accepted block's timestamp (µs). `block_reward_time` integrates the
    /// emission rate over [last_block_ts_us, this_block_ts]. Updated on every accept/apply so
    /// produce and follower-replay compute the SAME reward.
    last_block_ts_us: u128,
    /// LANE-R EXACT CARRY: the sub-unit numerator remainder carried across blocks so a 400µs
    /// block loses ZERO emission to integer truncation and the curve reaches EXACTLY 21M.
    /// Part of committed/snapshot state → deterministic replay (a follower threads the SAME
    /// carry from genesis, or restores it from a snapshot). Produce and verify MUST agree.
    emission_carry: u128,
    /// LANE-X collateral-credit vault (lock SIGIL → mint CREDIT @50% LTV). Persisted under
    /// its OWN flux-db key (`credit_vault`), NEVER inside `Snapshot` — bincode is positional,
    /// so appending a field to Snapshot breaks decode of pre-existing snapshots → genesis
    /// reseed → the restart-reset failure class LANE-X exists to prevent.
    credit_vault: sigil_bank::credit::CreditVault,
    /// LANE-Y agent spend-mandates — the on-chain home for the agent-money guard. Persisted under
    /// its OWN flux-db key (`mandates`), additive like `credit_vault` (never inside `Snapshot`).
    mandates: sigil_bank::mandate::MandateBook,
    /// LANE-Y bank 2-of-2 council (treasury transfers need two members). Own key (`bank_council`).
    council: sigil_bank::council::Council,
}

/// Persisted snapshot: the money + chain state (NOT students — VerifiedRegistry isn't
/// serde; re-onboard on restart. history persists itself in its own flux-db).
#[derive(serde::Serialize, serde::Deserialize)]
struct Snapshot {
    state: SigilState,
    height: u64,
    #[serde(default)]
    block_height: u64,
    tip_hash: [u8; 32],
    bits: u32,
    retarget_anchor_ts: u64,
    retarget_anchor_height: u64,
    tokens: Vec<(String, TokenId)>,
    pools: Vec<(String, PoolId)>,
    citizens: Vec<WalletId>,
    /// Per-wallet replay-nonce watermark (see Node.auth_nonces). `serde(default)`
    /// so snapshots written before auth landed still load.
    #[serde(default)]
    auth_nonces: std::collections::HashMap<WalletId, u64>,
    /// LANE-R time-based emission anchors (µs). `serde(default)` so pre-LANE-R snapshots
    /// still load (a chain that predates time-based emission keeps genesis_ts_us=0 until a
    /// fresh-genesis cut sets it).
    #[serde(default)]
    genesis_ts_us: u128,
    #[serde(default)]
    last_block_ts_us: u128,
    #[serde(default)]
    emission_carry: u128,
}

/// Write the current money+chain state to flux-db (bincode — handles u128 + tuple-key
/// maps that JSON can't). Called after every state-mutating request.
fn persist(node: &Node) {
    let Some(db) = node.statedb.as_ref() else { return };
    let snap = Snapshot {
        state: node.state.clone(), height: node.height, block_height: node.block_height, tip_hash: node.tip_hash,
        bits: node.bits, retarget_anchor_ts: node.retarget_anchor_ts, retarget_anchor_height: node.retarget_anchor_height,
        tokens: node.tokens.clone(), pools: node.pools.clone(), citizens: node.citizens.clone(),
        auth_nonces: node.auth_nonces.clone(),
        genesis_ts_us: node.genesis_ts_us, last_block_ts_us: node.last_block_ts_us,
        emission_carry: node.emission_carry,
    };
    if let Ok(bytes) = bincode::serialize(&snap) { let _ = db.put(b"snapshot", &bytes); }
    // LANE-X: the credit vault persists under its OWN key — additive, so the
    // legacy `snapshot` blob keeps decoding on old AND new binaries.
    if let Ok(bytes) = bincode::serialize(&node.credit_vault) { let _ = db.put(b"credit_vault", &bytes); }
    // LANE-Y: agent mandates + bank council each persist under their OWN additive key.
    if let Ok(bytes) = bincode::serialize(&node.mandates) { let _ = db.put(b"mandates", &bytes); }
    if let Ok(bytes) = bincode::serialize(&node.council) { let _ = db.put(b"bank_council", &bytes); }
}
/// Load a persisted snapshot, if one exists + decodes.
fn load_snapshot(db: &flux_db::Database) -> Option<Snapshot> {
    db.get(b"snapshot").ok().flatten().and_then(|b| bincode::deserialize(&b).ok())
}
/// Load the persisted credit vault (own key, see `persist`). Missing/undecodable
/// → empty vault (a chain that predates LANE-X simply has no positions yet).
fn load_credit_vault(db: Option<&flux_db::Database>) -> sigil_bank::credit::CreditVault {
    db.and_then(|d| d.get(b"credit_vault").ok().flatten())
        .and_then(|b| bincode::deserialize(&b).ok())
        .unwrap_or_default()
}
/// LANE-Y: load the persisted mandate book (own key). Missing/undecodable → empty book.
fn load_mandates(db: Option<&flux_db::Database>) -> sigil_bank::mandate::MandateBook {
    db.and_then(|d| d.get(b"mandates").ok().flatten())
        .and_then(|b| bincode::deserialize(&b).ok())
        .unwrap_or_default()
}
/// LANE-Y: load the persisted bank council (own key), seeded 2-of-2 [MASTER, OPERATOR] when empty.
fn load_council(db: Option<&flux_db::Database>) -> sigil_bank::council::Council {
    let mut c: sigil_bank::council::Council = db
        .and_then(|d| d.get(b"bank_council").ok().flatten())
        .and_then(|b| bincode::deserialize(&b).ok())
        .unwrap_or_default();
    c.seed(vec![MASTER, OPERATOR], 2); // idempotent — only seeds an empty roster
    c
}
/// Persist an accepted dual-lane block under its mining height so peers can pull it
/// (`GET /block?height=`) and INDEPENDENTLY re-verify the chain (verify-don't-trust).
fn store_block(node: &Node, bh: u64, sub: &Submission, reward: u128, prev_tip: [u8; 32], new_tip: [u8; 32], ts_us: u128) {
    let Some(db) = node.statedb.as_ref() else { return };
    // LANE-R: persist the block's µs timestamp so a follower/standalone verifier recomputes
    // the SAME time-based reward (block_reward_time(genesis_ts, prev_block_ts, ts, 0)).
    let rec = serde_json::json!({
        "height": bh, "prev_tip": hexs(&prev_tip), "tip": hexs(&new_tip),
        "reward": reward.to_string(), "bits": node.bits, "vdf_t": mining_vdf_t(), "submission": sub,
        "ts": ts_us.to_string(),
    });
    let _ = db.put(format!("block/{bh:020}").as_bytes(), rec.to_string().as_bytes());
}

/// The tip-fold (fix #3): tip_{h+1} = BLAKE3(domain ‖ prev_tip ‖ height ‖ blake4 ‖ nonce ‖
/// BLAKE3(vdf)). Shared by the producer (on accept) and the follower (on apply) so both
/// derive identical tips — the chain is deterministic.
fn fold_tip(prev_tip: &[u8; 32], bh: u64, blake4_hash: u64, nonce: u64, vdf: &flux_vdf::VdfProof) -> [u8; 32] {
    let vdf_commit = blake3::hash(&serde_json::to_vec(vdf).unwrap_or_default());
    let mut hh = blake3::Hasher::new();
    hh.update(b"sigil-g0/tip/v2");
    hh.update(prev_tip);
    hh.update(&bh.to_le_bytes());
    hh.update(&blake4_hash.to_le_bytes());
    hh.update(&nonce.to_le_bytes());
    hh.update(vdf_commit.as_bytes());
    *hh.finalize().as_bytes()
}

/// Minimal std HTTP GET (host:port + path) → response body. For peer sync.
fn http_get(host_port: &str, path: &str) -> Option<String> {
    let mut s = std::net::TcpStream::connect(host_port).ok()?;
    let _ = s.set_read_timeout(Some(std::time::Duration::from_secs(8)));
    s.write_all(format!("GET {path} HTTP/1.1\r\nHost: {host_port}\r\nConnection: close\r\n\r\n").as_bytes()).ok()?;
    let mut o = String::new();
    s.read_to_string(&mut o).ok()?;
    Some(o.split("\r\n\r\n").nth(1)?.to_string())
}

/// Follower APPLY: independently RE-VERIFY a peer's block against our tip, then apply it
/// (credit + advance) so our state converges. verify-don't-trust — a bad block is rejected,
/// never applied. Returns the applied height.
fn apply_block(n: &mut Node, rec: &serde_json::Value, g: &ModSquaring) -> Result<u64, String> {
    let bh = rec["height"].as_u64().ok_or("no height")?;
    if bh != n.block_height { return Err(format!("height gap: peer block {bh} != my tip {}", n.block_height)); }
    if hex32(rec["prev_tip"].as_str().unwrap_or("")).ok_or("bad prev_tip")? != n.tip_hash {
        return Err(format!("block {bh}: prev_tip != my tip (fork/divergence)"));
    }
    let bits = rec["bits"].as_u64().unwrap_or(0) as u32;
    let vdf_t = rec["vdf_t"].as_u64().unwrap_or(0);
    let reward: u128 = rec["reward"].as_str().unwrap_or("0").parse().map_err(|_| "bad reward")?;
    let rec_ts: u128 = rec["ts"].as_str().unwrap_or("0").parse().unwrap_or(0);
    let sub: Submission = serde_json::from_value(rec["submission"].clone()).map_err(|e| format!("bad submission: {e}"))?;
    let miner = hex32(&sub.wallet).ok_or("bad wallet")?;
    let c = Challenge { height: bh, vdf_input: mining_seed(&n.tip_hash, bh), blake4_target: target_from_bits(bits), vdf_t };
    if !check_submission(g, &c, &sub) { return Err(format!("block {bh}: dual-lane verify FAILED")); }
    // LANE-R: recompute the reward the SAME way the producer did — time-based from the block's
    // stored µs ts when genesis is anchored, else the legacy block-based schedule. A follower
    // that diverges here would fork, so this MUST mirror the produce path exactly.
    let (expected_reward, new_carry) = if n.genesis_ts_us == 0 {
        (sigil_emission::block_reward(bh), 0u128)
    } else {
        sigil_emission::block_reward_time(n.genesis_ts_us, n.last_block_ts_us, rec_ts, n.emission_carry)
    };
    if reward != expected_reward { return Err(format!("block {bh}: reward {reward} != schedule {expected_reward}")); }
    let new_tip = fold_tip(&n.tip_hash, bh, sub.block.blake4_hash, sub.block.nonce, &sub.block.vdf);
    if hex32(rec["tip"].as_str().unwrap_or("")).ok_or("bad tip")? != new_tip { return Err(format!("block {bh}: tip-fold mismatch")); }
    // All checks passed — APPLY (mirror the producer's accept exactly).
    let eh = n.height;
    credit_share(&mut n.state, eh, miner, reward).map_err(|e| e.to_string())?;
    let prev_tip = n.tip_hash;
    n.tip_hash = new_tip;
    store_block(n, bh, &sub, reward, prev_tip, new_tip, rec_ts);
    if n.genesis_ts_us != 0 { n.last_block_ts_us = rec_ts; n.emission_carry = new_carry; } // advance clock + carry
    n.block_height += 1;
    n.height += 1;
    // ADOPT the producer's difficulty from the chain — a follower must NOT run its own
    // wall-clock retarget (it applies blocks all at once → different timing → divergent bits).
    // (Trust boundary until step ③: difficulty becomes fully chain-derived via in-block
    // timestamps + a verified retarget rule.)
    n.bits = bits;
    n.retarget_anchor_ts = now_ms();
    n.retarget_anchor_height = n.block_height;
    ingest(n, "mine", format!("synced block #{bh} reward {reward} (from peer)"), &[miner], "dual-lane blake4+vdf synced");
    Ok(bh)
}

/// Background sync loop: poll a peer's /tip and pull+verify+apply every block we're missing.
fn follow_peer(node: std::sync::Arc<RwLock<Node>>, peer: String) {
    let g = ModSquaring::bench_2048();
    loop {
        if let Some(tj) = http_get(&peer, "/api/v1/tip") {
            if let Ok(ti) = serde_json::from_str::<serde_json::Value>(&tj) {
                let head = ti["block_height"].as_u64().unwrap_or(0);
                loop {
                    let my_bh = node.read().unwrap().block_height;
                    if my_bh >= head { break; }
                    let Some(bj) = http_get(&peer, &format!("/api/v1/block?height={my_bh}")) else { break };
                    let Ok(rec) = serde_json::from_str::<serde_json::Value>(&bj) else { break };
                    if rec.get("found") == Some(&serde_json::Value::Bool(false)) { break; }
                    let mut n = node.write().unwrap();
                    match apply_block(&mut n, &rec, &g) {
                        Ok(applied) => { persist(&n); eprintln!("follower: applied block #{applied} (tip now {})", n.block_height); }
                        Err(e) => { eprintln!("follower: REJECT block {my_bh} — {e} (halting sync)"); break; }
                    }
                }
            }
        }
        std::thread::sleep(std::time::Duration::from_secs(2));
    }
}

/// Content address (blake3 CID, hex) — flux-aether's content-addressing primitive.
/// The record's bytes are stored in flux-db (via flux-history); the CID is its hash,
/// so retrieval (/aether?cid=) can verify blake3(content)==cid (verify-don't-trust).
fn cid_of(s: &str) -> String { blake3::hash(s.as_bytes()).to_hex().to_string() }

/// Wall-clock microseconds since the unix epoch. LANE-R: emission integrates over µs so a
/// 400 µs producer block isn't truncated to a 0 reward (ms resolution would collapse dt→0).
fn now_us() -> u128 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_micros()).unwrap_or(0)
}

/// hex of a 32-byte id (searchable content + the `addr` tag).
fn hexs(b: &[u8; 32]) -> String {
    let mut s = String::with_capacity(64);
    for x in b { s.push_str(&format!("{x:02x}")); }
    s
}
fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}
/// Append a tx to the flux-history index. `addrs` go into the full-text content +
/// an `addr` tag so a wallet-address search returns this tx.
fn ingest(node: &mut Node, kind: &str, title: String, addrs: &[[u8; 32]], extra: &str) {
    let height = node.height;
    let ts = now_ms();
    let addr_str: Vec<String> = addrs.iter().map(hexs).collect();
    let content = format!("block {height} {} {}", addr_str.join(" "), extra);
    let cid = cid_of(&content); // blake3 content-address
    let Some(h) = node.history.as_mut() else { return };
    let mut entry = flux_history::HistoryEntry::new(kind, addr_str.first().cloned().unwrap_or_default(), title, content, ts)
        .with_tag("height", &height.to_string()).with_tag("cid", &cid);
    for a in &addr_str { entry = entry.with_tag("addr", a); }
    let _ = h.append(entry);
}

// ── Token ids (legible byte-fills; dynamically deployed tokens are hashed). ──
const USDS: TokenId = [0xAA; 32]; // stablecoin quote asset
const WQUG: TokenId = [0xBB; 32]; // wrapped Quillon QUG
const CLAI: TokenId = [0xC1; 32]; // Claude-Liaison inter-agent token
const PACI: TokenId = [0xCA; 32]; // Rocky's PACI
const SCAL: TokenId = [0x5C; 32]; // Codex's SCALPEL
const GPU_T: TokenId = [0x9B; 32]; // compute credit token

// ── Pools (constant-product, 30 bps LP fee). USDS is the common quote asset. ─
const POOL: PoolId = [0xCC; 32]; // USDS/wQUG (kept for nation/back-compat)
const POOL_CLAI: PoolId = [0xC2; 32]; // CLAI/USDS
const POOL_PACI: PoolId = [0xC3; 32]; // PACI/USDS
const POOL_SCAL: PoolId = [0xC4; 32]; // SCAL/USDS
const POOL_SIGIL: PoolId = [0xC5; 32]; // SIGIL(NATIVE)/USDS

const OPERATOR: WalletId = [0xEE; 32];
// Master dev-fee wallet (Viktor, 095b0e1f…3dd8) — 5% of mining coinbase + 0.3% DEX.
const MASTER: WalletId = sigil_bank::DEV_MASTER_WALLET;
// flux-nation demo citizen (funded + attested at bootstrap so the page's pay works)
const CITIZEN: WalletId = [0x11; 32];
const POWER_CO: WalletId = [0x9E; 32];
const CPR: [u8; 32] = [0x42; 32]; // hash of a CPR (raw CPR never stored)

/// How many swarm trader wallets to fund at genesis.
const TRADERS: u8 = 8;
/// Deterministic swarm trader wallet `i` → `a0a0…a0<i>`.
fn trader(i: u8) -> WalletId {
    let mut w = [0xA0; 32];
    w[31] = i;
    w
}

// ── Dual-lane mining (BLAKE4 Φ + VDF Ω) params ───────────────────────────────
// These define the `/api/v1/mining/{challenge,submit}` work and MUST match the
// flux-miner client's group (ModSquaring::bench_2048) + the reference
// flux-mining-node defaults, so the same `flux-miner mine <url> <wallet>` binary
// drives sigil-rpcd unchanged. Operator-tunable via env.
const MINING_BLAKE4_BITS: u32 = 16; // BLAKE4 target = u64::MAX >> bits (Lane A)
const MINING_VDF_T: u64 = 600; // sequential VDF squarings per share (Lane B)
// Coinbase reward follows the sigil-emission halving schedule (block_reward(height)),
// NOT a flat constant — see /mining/submit.

fn mining_bits() -> u32 {
    std::env::var("SIGIL_MINING_BLAKE4_BITS").ok().and_then(|s| s.parse().ok()).unwrap_or(MINING_BLAKE4_BITS)
}
fn mining_vdf_t() -> u64 {
    std::env::var("SIGIL_MINING_VDF_T").ok().and_then(|s| s.parse().ok()).unwrap_or(MINING_VDF_T)
}
/// BLAKE4 Lane-A target for a difficulty `bits`: target = u64::MAX >> bits.
fn target_from_bits(bits: u32) -> u64 { u64::MAX >> bits.min(63) }

// ── Difficulty retarget (fix #2): adjust BLAKE4 `bits` from REAL block times ──
const RETARGET_WINDOW: u64 = 16; // retarget every N accepted blocks
const TARGET_BLOCK_MS_DEFAULT: u64 = 5000; // desired average block interval
fn target_block_ms() -> u64 {
    std::env::var("SIGIL_TARGET_BLOCK_MS").ok().and_then(|s| s.parse().ok()).unwrap_or(TARGET_BLOCK_MS_DEFAULT)
}
/// Bitcoin-style difficulty retarget on the BLAKE4 lane. Called after each accepted
/// block; fires once per RETARGET_WINDOW. Meaningful ONLY because fix #1 made block
/// timestamps real (no precompute). Clamped to ±2 bits/window (≤4x) and bits ∈ [4,48].
fn retarget(node: &mut Node) {
    if node.block_height < node.retarget_anchor_height + RETARGET_WINDOW { return; }
    let now = now_ms();
    let actual = now.saturating_sub(node.retarget_anchor_ts).max(1);
    let expected = RETARGET_WINDOW * target_block_ms();
    // ratio>1 ⇒ blocks came too FAST ⇒ raise bits (harder); <1 ⇒ too slow ⇒ lower.
    let delta = (expected as f64 / actual as f64).log2().round().clamp(-2.0, 2.0) as i64;
    let new_bits = (node.bits as i64 + delta).clamp(4, 48) as u32;
    eprintln!("retarget: bh={} actual={}ms expected={}ms bits {}→{}", node.block_height, actual, expected, node.bits, new_bits);
    node.bits = new_bits;
    node.retarget_anchor_ts = now;
    node.retarget_anchor_height = node.block_height;
}
/// Per-height VDF challenge seed bound to the chain TIP: BLAKE3(tip ‖ height).
/// Because `tip` folds the prior accepted block, the seed for height H is
/// unknowable until H-1 lands — defeating precompute of future challenges.
fn mining_seed(tip: &[u8; 32], height: u64) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(b"sigil-g0/mining-challenge/v1");
    h.update(tip);
    h.update(&height.to_le_bytes());
    *h.finalize().as_bytes()
}

/// Integer sqrt (initial LP shares = √(a·b), Uniswap-V2 style).
fn isqrt_u128(n: u128) -> u128 {
    if n == 0 {
        return 0;
    }
    let (mut x, mut y) = (n, (n + 1) / 2);
    while y < x {
        x = y;
        y = (x + n / x) / 2;
    }
    x
}

/// Deterministic non-crypto token id from a symbol (for /deploy_token). FNV-1a
/// then a bit-mixer folded across 32 bytes; first byte tagged `0x70` so dynamic
/// tokens are visually distinct from the predefined byte-fill ids.
fn token_id_for(sym: &str) -> TokenId {
    let mut out = [0u8; 32];
    let mut h: u64 = 0xcbf29ce484222325;
    for b in sym.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    for o in out.iter_mut() {
        h ^= h >> 33;
        h = h.wrapping_mul(0xff51afd7ed558ccd);
        *o = h as u8;
    }
    out[0] = 0x70;
    out
}

fn bootstrap() -> Node {
    // flux-history index (its own flux-db, persists itself).
    let hist_path = std::env::var("SIGIL_HISTORY_PATH").unwrap_or_else(|_| "/home/orobit/sigil-data/history".into());
    // open_fast: bind the port NOW, build the full-text index off the hot path.
    // The synchronous index rebuild used to add ~15s (and, pre-bulk_load, was an
    // O(n²) hang) to every restart before :8099 came up. main() spawns the index
    // build in the background and flips INDEX_READY when it's done.
    let mut history = match flux_history::HistoryStore::open_fast(&hist_path) {
        Ok(s) => { eprintln!("flux-history: {} entries @ {hist_path} (index building in background)", s.len()); Some(s) }
        Err(e) => { eprintln!("flux-history: disabled ({e})"); None }
    };
    if let Some(h) = history.as_mut() { backfill_blocks(h); }
    // state persistence (flux-db): the money + chain survive restarts.
    let state_path = std::env::var("SIGIL_STATE_PATH").unwrap_or_else(|_| "/home/orobit/sigil-data/state".into());
    let statedb = match flux_db::Database::open(&state_path) {
        Ok(d) => Some(d),
        Err(e) => { eprintln!("flux-db state: disabled, in-memory only ({e})"); None }
    };
    if let Some(snap) = statedb.as_ref().and_then(load_snapshot) {
        eprintln!("flux-db: RESTORED state @ height {} ({} wallets, native supply {}) from {state_path}",
            snap.height, snap.state.wallet_count(), snap.state.native_supply());
        // LANE-X: restore the credit vault from its own key + ensure the CREDIT
        // token is in the registry (idempotent — older snapshots don't list it).
        let credit_vault = load_credit_vault(statedb.as_ref());
        eprintln!("flux-db: credit vault — {} position(s), {} collateral locked, reserve {}",
            credit_vault.status().position_count, credit_vault.total_collateral, credit_vault.protocol_reserve);
        let mandates = load_mandates(statedb.as_ref());
        let council = load_council(statedb.as_ref());
        eprintln!("flux-db: LANE-Y — {} mandate(s), council {}-of-{}",
            mandates.mandates.len(), council.threshold, council.members.len());
        let mut tokens = snap.tokens;
        if !tokens.iter().any(|(_, id)| *id == sigil_bank::credit::CREDIT_TOKEN) {
            tokens.push(("CREDIT".into(), sigil_bank::credit::CREDIT_TOKEN));
        }
        return Node {
            state: snap.state, height: snap.height, block_height: snap.block_height, students: VerifiedRegistry::new(),
            tokens, pools: snap.pools, citizens: snap.citizens, history,
            tip_hash: snap.tip_hash, bits: snap.bits,
            retarget_anchor_ts: snap.retarget_anchor_ts, retarget_anchor_height: snap.retarget_anchor_height,
            statedb,
            auth_nonces: snap.auth_nonces,
            onboard_rl: std::collections::HashMap::new(),
            genesis_ts_us: snap.genesis_ts_us, last_block_ts_us: snap.last_block_ts_us,
            emission_carry: snap.emission_carry,
            credit_vault,
            mandates,
            council,
        };
    }
    eprintln!("flux-db: no snapshot — seeding fresh genesis @ {state_path}");

    // ── FRESH GENESIS — seed initial state, then snapshot it ──
    let mut state = SigilState::new();
    let mut m: Vec<StateMutation> = vec![StateMutation::SetMasterWallet { wallet: MASTER }];

    // Operator + demo citizen seed funding (NATIVE well under the 21M cap).
    m.push(StateMutation::SetBalance { wallet: OPERATOR, token: NATIVE, amount: 2_000_000 });
    m.push(StateMutation::SetBalance { wallet: OPERATOR, token: USDS, amount: 5_000_000 });
    m.push(StateMutation::SetBalance { wallet: CITIZEN, token: NATIVE, amount: 1_000 });

    // Fund the swarm traders: a stack of every token so they can hit any pool.
    for i in 0..TRADERS {
        let w = trader(i);
        m.push(StateMutation::SetBalance { wallet: w, token: NATIVE, amount: 2_000 });
        m.push(StateMutation::SetBalance { wallet: w, token: USDS, amount: 100_000 });
        m.push(StateMutation::SetBalance { wallet: w, token: WQUG, amount: 50_000 });
        m.push(StateMutation::SetBalance { wallet: w, token: CLAI, amount: 20_000 });
        m.push(StateMutation::SetBalance { wallet: w, token: PACI, amount: 40_000 });
        m.push(StateMutation::SetBalance { wallet: w, token: SCAL, amount: 200_000 });
        m.push(StateMutation::SetBalance { wallet: w, token: GPU_T, amount: 10_000 });
    }

    // Seed the LP pools with real reserves (lp_shares = √(a·b)).
    let seed = |pool: PoolId, ta: TokenId, tb: TokenId, ra: u128, rb: u128| StateMutation::SetPool {
        pool,
        state: PoolState {
            token_a: ta, token_b: tb,
            reserve_a: ra, reserve_b: rb,
            lp_shares: isqrt_u128(ra.saturating_mul(rb)),
            fee_bps: 30, accrued_fees: 0,
        },
    };
    m.push(seed(POOL, USDS, WQUG, 100_000, 100_000));
    m.push(seed(POOL_CLAI, CLAI, USDS, 50_000, 100_000));
    m.push(seed(POOL_PACI, PACI, USDS, 200_000, 100_000));
    m.push(seed(POOL_SCAL, SCAL, USDS, 1_000_000, 50_000));
    m.push(seed(POOL_SIGIL, NATIVE, USDS, 100_000, 100_000));

    commit_state_transition(&mut state, &StateTransition { at_height: 0, mutations: m }, 0)
        .expect("genesis");

    // flux-nation: attest the demo citizen so /nation/pay works against real state.
    nation::attest_citizen(&mut state, 1, nation::BORGER_AUTHORITY, CITIZEN, CPR)
        .expect("attest demo citizen");

    let tokens = vec![
        ("SIGIL".into(), NATIVE), ("USDS".into(), USDS), ("wQUG".into(), WQUG),
        ("CLAI".into(), CLAI), ("PACI".into(), PACI), ("SCAL".into(), SCAL),
        ("GPU".into(), GPU_T),
        ("CREDIT".into(), sigil_bank::credit::CREDIT_TOKEN), // LANE-X collateral-credit
    ];
    let pools = vec![
        ("USDS/wQUG".into(), POOL), ("CLAI/USDS".into(), POOL_CLAI),
        ("PACI/USDS".into(), POOL_PACI), ("SCAL/USDS".into(), POOL_SCAL),
        ("SIGIL/USDS".into(), POOL_SIGIL),
    ];
    let tip_hash = *blake3::hash(b"sigil-g0/mining-genesis").as_bytes();
    // LANE-R: anchor time-based emission at THIS fresh genesis. genesis_ts_us is the wall-clock
    // µs the chain started; the first block integrates from here. Persisted + gossiped via the
    // snapshot so every node halves on the same clock. (Set SIGIL_GENESIS_TS_US to pin an exact
    // genesis instant across a coordinated fleet cut; else use now.)
    let genesis_ts_us = std::env::var("SIGIL_GENESIS_TS_US").ok()
        .and_then(|v| v.parse::<u128>().ok()).unwrap_or_else(now_us);
    let node = Node { state, height: 2, block_height: 0, students: VerifiedRegistry::new(), tokens, pools, citizens: vec![CITIZEN], history,
        tip_hash, bits: mining_bits(), retarget_anchor_ts: now_ms(), retarget_anchor_height: 0, statedb,
        auth_nonces: std::collections::HashMap::new(), onboard_rl: std::collections::HashMap::new(),
        genesis_ts_us, last_block_ts_us: genesis_ts_us, emission_carry: 0,
        credit_vault: sigil_bank::credit::CreditVault::new(),
        mandates: sigil_bank::mandate::MandateBook::default(),
        council: { let mut c = sigil_bank::council::Council::default(); c.seed(vec![MASTER, OPERATOR], 2); c } };
    persist(&node); // write the genesis snapshot so the next boot restores
    node
}

/// One-time backfill: ingest the persisted DAG block headers (dag-blocks.json) into
/// flux-history so historical blocks are searchable (by height/hash/producer). Guarded
/// by a `meta=backfill` marker so it runs once. (Pre-session per-address tx data doesn't
/// exist — the in-memory node never persisted it — so this backfills BLOCK headers.)
fn backfill_blocks(history: &mut flux_history::HistoryStore) {
    if history.by_tag("meta", "backfill").map(|v| !v.is_empty()).unwrap_or(false) { return; }
    let path = std::env::var("SIGIL_DAG_BLOCKS").unwrap_or_else(|_| "/home/orobit/q-narwhalknight/dist-fluxapp/dag-blocks.json".into());
    let Ok(data) = std::fs::read_to_string(&path) else { eprintln!("flux-history: backfill skipped (no {path})"); return };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&data) else { return };
    let blocks = v.get("blocks").and_then(|b| b.as_array()).cloned().unwrap_or_default();
    let mut n = 0u64;
    for b in &blocks {
        let h = b.get("h").and_then(|x| x.as_u64()).unwrap_or(0);
        let hash = b.get("hash").and_then(|x| x.as_str()).unwrap_or("");
        let prod = b.get("prod").and_then(|x| x.as_str()).unwrap_or("?");
        let content = format!("block {h} hash {hash} producer {prod}");
        let cid = cid_of(&content); // blake3 content-address
        let entry = flux_history::HistoryEntry::new("block", prod, format!("Block #{h}"), content, now_ms())
            .with_tag("height", &h.to_string()).with_tag("hash", hash).with_tag("kind", "block").with_tag("cid", &cid);
        if history.append(entry).is_ok() { n += 1; }
    }
    let _ = history.append(flux_history::HistoryEntry::new("_meta", "backfill", "backfill done",
        format!("backfilled {n} blocks from {path}"), now_ms()).with_tag("meta", "backfill"));
    eprintln!("flux-history: backfilled {n} block headers from {path}");
}

/// Symbol for a token id given the live registry (falls back to a hex stub).
fn sym_of(node: &Node, t: &TokenId) -> String {
    node.tokens.iter().find(|(_, id)| id == t).map(|(s, _)| s.clone())
        .unwrap_or_else(|| format!("0x{}", &to_hex(t)[..6]))
}

fn hex32(s: &str) -> Option<[u8; 32]> {
    let s = s.trim();
    if s.len() != 64 { return None; }
    let mut out = [0u8; 32];
    for i in 0..32 {
        out[i] = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}
fn to_hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

// minimal JSON field pluckers (the bodies are small + flat)
fn jnum(body: &str, key: &str) -> Option<u128> {
    let p = format!("\"{key}\"");
    let i = body.find(&p)? + p.len();
    let rest = &body[i..];
    let c = rest.find(':')? + 1;
    let tail = rest[c..].trim_start();
    let end = tail.find(|ch: char| !ch.is_ascii_digit()).unwrap_or(tail.len());
    tail[..end].parse().ok()
}
fn jstr<'a>(body: &'a str, key: &str) -> Option<&'a str> {
    let p = format!("\"{key}\"");
    let i = body.find(&p)? + p.len();
    let rest = &body[i..];
    let q1 = rest[rest.find(':')? + 1..].find('"')? + rest.find(':')? + 2;
    let q2 = rest[q1..].find('"')? + q1;
    Some(&rest[q1..q2])
}

/// Parse a 128-hex (64-byte) ed25519 wallet signature.
fn hex64(s: &str) -> Option<[u8; 64]> {
    let s = s.trim();
    if s.len() != 128 { return None; }
    let mut out = [0u8; 64];
    for i in 0..64 {
        out[i] = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

/// Per-request wallet-signature gate for a mutating route (audit C2–C5/H8).
/// `actor` is the wallet whose funds move; the caller must sign the canonical
/// message (`sigil_rpc::auth::auth_message`) for `action`+`fields`+`req_nonce`
/// with the actor wallet's ed25519 key and include `sig` (128-hex) + `req_nonce`
/// in the body. Enforces a strictly-increasing per-wallet `req_nonce` so a
/// captured request can't be replayed. Returns Err(reason); the route maps it to
/// `bad(..)`. NOTE: the replay nonce field is `req_nonce`, NOT `nonce`, so it
/// never collides with a route's own `nonce` (e.g. /mine's PoW nonce).
///
/// `SIGIL_RPC_NO_AUTH=1` bypasses the gate — local dev / client-migration ONLY,
/// never on the public daemon.
fn authorize(n: &mut Node, actor: &WalletId, action: &str, fields: &[String], body: &str) -> Result<(), String> {
    if std::env::var("SIGIL_RPC_NO_AUTH").is_ok() {
        return Ok(());
    }
    let sig_hex = jstr(body, "sig").ok_or("missing 'sig' (128-hex wallet signature)")?;
    let sig = hex64(sig_hex).ok_or("'sig' must be 128-hex (64 bytes)")?;
    let req_nonce = jnum(body, "req_nonce")
        .ok_or("missing 'req_nonce' (must strictly increase per wallet — use a ms timestamp)")? as u64;
    let last = n.auth_nonces.get(actor).copied().unwrap_or(0);
    if req_nonce <= last {
        return Err(format!("stale/replayed req_nonce {req_nonce} (last accepted {last})"));
    }
    let f: Vec<&str> = fields.iter().map(|s| s.as_str()).collect();
    if !sigil_rpc::auth::verify_request(actor, action, &f, req_nonce, &sig) {
        return Err("bad wallet signature (does not authorize this action for the actor wallet)".into());
    }
    n.auth_nonces.insert(*actor, req_nonce);
    Ok(())
}

fn ok(body: String) -> String {
    format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nAccess-Control-Allow-Origin: *\r\nContent-Length: {}\r\n\r\n{}", body.len(), body)
}
fn bad(msg: &str) -> String {
    let b = format!("{{\"ok\":false,\"error\":\"{}\"}}", msg.replace('"', "'"));
    format!("HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nAccess-Control-Allow-Origin: *\r\nContent-Length: {}\r\n\r\n{}", b.len(), b)
}

fn route(node: &RwLock<Node>, method: &str, path: &str, query: &str, body: &str, peer_ip: &str) -> String {
    // The SIGIL wallet (a q-api fork) calls the `/api/v1/*` shape — map it onto
    // our flat routes so the wallet works unchanged against sigil-rpcd.
    let path = path.strip_prefix("/api/v1").unwrap_or(path);
    // CORS preflight (harmless when served same-origin via the q-flux vhost).
    if method == "OPTIONS" {
        return ok("{}".into());
    }
    match (method, path) {
        ("GET", "/health") => ok("{\"ok\":true,\"service\":\"sigil-rpcd\"}".into()),
        // ── peer sync (decentralization ①): expose the mining chain for verify-don't-trust ──
        ("GET", "/tip") => {
            let n = node.read().unwrap();
            // LANE-R: expose genesis_ts_us so a standalone verifier (chain_verify) can recompute
            // the time-based reward (0 on a legacy block-based chain).
            ok(format!("{{\"ok\":true,\"block_height\":{},\"height\":{},\"tip\":\"{}\",\"bits\":{},\"genesis_ts_us\":\"{}\"}}",
                n.block_height, n.height, hexs(&n.tip_hash), n.bits, n.genesis_ts_us))
        }
        ("GET", "/block") => {
            let h: u64 = match query_get(query, "height").and_then(|s| s.parse().ok()) {
                Some(h) => h, None => return bad("height query param required"),
            };
            let n = node.read().unwrap();
            match n.statedb.as_ref().and_then(|db| db.get(format!("block/{h:020}").as_bytes()).ok().flatten()) {
                Some(bytes) => ok(String::from_utf8_lossy(&bytes).to_string()),
                None => ok(format!("{{\"ok\":true,\"height\":{h},\"found\":false}}")),
            }
        }
        // Readiness probe: true once the background search index is live. The
        // money/chain routes serve from boot regardless — this only gates search.
        ("GET", "/readyz") => {
            let ready = INDEX_READY.load(Ordering::Relaxed);
            ok(format!("{{\"ok\":true,\"index_ready\":{ready}}}"))
        }
        ("GET", "/status") => {
            let n = node.read().unwrap();
            ok(format!(
                "{{\"ok\":true,\"service\":\"sigil-rpcd\",\"network\":\"sigil-g0\",\"height\":{},\"version\":\"0.0.7\",\"peers\":1,\"index_ready\":{}}}",
                n.height, INDEX_READY.load(Ordering::Relaxed)
            ))
        }
        // flux-search over the tx history (flux-db + flux-search). Search a wallet
        // address → that address's transactions; or a block hash/height/keyword.
        ("GET", "/search") => {
            let q = query_get(query, "q").unwrap_or("").trim().to_string();
            if q.len() < 2 { return ok("{\"ok\":true,\"q\":\"\",\"count\":0,\"results\":[]}".into()); }
            let js = |s: &str| serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into());
            // EXACT tag lookups for the ID-like queries (flux-search TF-IDF misses
            // them — IDF=0 for ubiquitous terms, and pure-numeric tokens don't match):
            //   64-hex → wallet ADDRESS; all-digits → block HEIGHT. Else full-text.
            let is_addr = q.len() == 64 && q.chars().all(|c| c.is_ascii_hexdigit());
            let is_height = q.chars().all(|c| c.is_ascii_digit());
            let map_e = |es: Vec<flux_history::HistoryEntry>| -> Vec<String> { es.iter().map(|e| format!(
                "{{\"title\":{},\"snippet\":{},\"id\":{},\"kind\":{},\"ts\":{},\"score\":1.0}}",
                serde_json::to_string(&e.title).unwrap_or_else(|_| "\"\"".into()),
                serde_json::to_string(&e.content).unwrap_or_else(|_| "\"\"".into()),
                serde_json::to_string(&e.id).unwrap_or_else(|_| "\"\"".into()),
                serde_json::to_string(&e.kind).unwrap_or_else(|_| "\"\"".into()), e.ts_ms)).collect() };
            let mut n = node.write().unwrap();
            let items: Vec<String> = match n.history.as_mut() {
                None => vec![],
                Some(h) if is_addr => map_e(h.by_tag("addr", &q.to_lowercase()).unwrap_or_default()),
                Some(h) if is_height => map_e(h.by_tag("height", &q).unwrap_or_default()),
                Some(h) => h.search(&q, 25).iter().map(|r| format!(
                    "{{\"title\":{},\"snippet\":{},\"id\":{},\"score\":{:.3}}}",
                    js(&r.title), js(&r.snippet), js(&r.url), r.score)).collect(),
            };
            ok(format!("{{\"ok\":true,\"q\":{},\"count\":{},\"results\":[{}]}}", js(&q), items.len(), items.join(",")))
        }
        // Recent entries from flux-history (read lock — by_kind/recent are &self).
        // ?kind=block → blocks; ?kind=tx → mine+swap; else → latest of all kinds.
        ("GET", "/recent") => {
            let kind = query_get(query, "kind").unwrap_or("");
            let limit: usize = query_get(query, "limit").and_then(|s| s.parse().ok()).unwrap_or(20);
            let js = |s: &str| serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into());
            let n = node.read().unwrap();
            let mut entries = match n.history.as_ref() {
                None => vec![],
                Some(h) => match kind {
                    "block" => h.by_kind("block").unwrap_or_default(),
                    "tx" => { let mut v = h.by_kind("mine").unwrap_or_default(); v.extend(h.by_kind("swap").unwrap_or_default()); v }
                    _ => h.recent(limit.max(1)).unwrap_or_default(),
                },
            };
            entries.sort_by(|a, b| b.ts_ms.cmp(&a.ts_ms)); // newest first
            entries.truncate(limit.max(1));
            let items: Vec<String> = entries.iter().map(|e| format!(
                "{{\"title\":{},\"content\":{},\"kind\":{},\"ts\":{},\"h\":{},\"hash\":{},\"prod\":{},\"cid\":{}}}",
                js(&e.title), js(&e.content), js(&e.kind), e.ts_ms,
                e.tags.get("height").and_then(|s| s.parse::<u64>().ok()).unwrap_or(0),
                js(e.tags.get("hash").map(|s| s.as_str()).unwrap_or("")),
                js(&e.source), js(e.tags.get("cid").map(|s| s.as_str()).unwrap_or("")))).collect();
            ok(format!("{{\"ok\":true,\"kind\":{},\"count\":{},\"results\":[{}]}}", js(kind), items.len(), items.join(",")))
        }
        // Content-addressed retrieve: /aether?cid=<blake3>. The record's bytes live in
        // flux-db (flux-history); we look it up by its cid tag and VERIFY blake3(content)==cid
        // (verify-don't-trust — the address is the hash, so a tampered record is detectable).
        ("GET", "/aether") => {
            let cid = query_get(query, "cid").unwrap_or("").trim().to_string();
            let js = |s: &str| serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into());
            let n = node.read().unwrap();
            let found = n.history.as_ref().and_then(|h| h.by_tag("cid", &cid).ok()).and_then(|v| v.into_iter().next());
            match found {
                Some(e) => {
                    let verified = cid_of(&e.content) == cid;
                    ok(format!("{{\"ok\":true,\"cid\":{},\"found\":true,\"verified\":{},\"title\":{},\"content\":{}}}",
                        js(&cid), verified, js(&e.title), js(&e.content)))
                }
                None => ok(format!("{{\"ok\":true,\"cid\":{},\"found\":false}}", js(&cid))),
            }
        }
        ("GET", "/balance") => {
            let w = query_get(query, "wallet").and_then(hex32);
            let t = query_get(query, "token").and_then(hex32).unwrap_or(NATIVE);
            match w {
                Some(w) => {
                    let n = node.read().unwrap();
                    ok(format!("{{\"ok\":true,\"balance\":{}}}", n.state.balance_of(&w, &t)))
                }
                None => bad("wallet must be 64-hex"),
            }
        }
        ("GET", "/pools") => {
            let n = node.read().unwrap();
            let mut items = Vec::new();
            for (label, pid) in &n.pools {
                if let Some(p) = n.state.pool(pid) {
                    items.push(format!(
                        "{{\"id\":\"{}\",\"label\":\"{}\",\"token_a\":\"{}\",\"token_b\":\"{}\",\"sym_a\":\"{}\",\"sym_b\":\"{}\",\"reserve_a\":{},\"reserve_b\":{},\"lp_shares\":{},\"fee_bps\":{},\"accrued_fees\":{}}}",
                        to_hex(pid), label, to_hex(&p.token_a), to_hex(&p.token_b),
                        sym_of(&n, &p.token_a), sym_of(&n, &p.token_b),
                        p.reserve_a, p.reserve_b, p.lp_shares, p.fee_bps, p.accrued_fees));
                }
            }
            ok(format!("{{\"ok\":true,\"pools\":[{}]}}", items.join(",")))
        }
        ("GET", "/tokens") => {
            let n = node.read().unwrap();
            let items: Vec<String> = n.tokens.iter()
                .map(|(s, id)| format!("{{\"symbol\":\"{}\",\"id\":\"{}\"}}", s, to_hex(id)))
                .collect();
            ok(format!("{{\"ok\":true,\"tokens\":[{}]}}", items.join(",")))
        }
        ("GET", "/wallets") => {
            // The funded swarm trader wallets, with per-token balances.
            let n = node.read().unwrap();
            let mut items = Vec::new();
            for i in 0..TRADERS {
                let w = trader(i);
                let bals: Vec<String> = n.tokens.iter()
                    .map(|(s, id)| format!("\"{}\":{}", s, n.state.balance_of(&w, id)))
                    .collect();
                items.push(format!("{{\"i\":{},\"wallet\":\"{}\",\"balances\":{{{}}}}}", i, to_hex(&w), bals.join(",")));
            }
            ok(format!("{{\"ok\":true,\"traders\":[{}]}}", items.join(",")))
        }
        ("GET", "/economy") => {
            let n = node.read().unwrap();
            ok(format!(
                "{{\"ok\":true,\"network\":\"sigil-g0\",\"height\":{},\"tokens\":{},\"pools\":{},\"traders\":{},\"students\":{},\"citizens\":{},\"treasury_usds\":{}}}",
                n.height, n.tokens.len(), n.pools.len(), TRADERS, n.students.len(), n.citizens.len(),
                n.state.balance_of(&MASTER, &USDS)))
        }
        ("POST", "/deploy_token") => {
            let symbol = match jstr(body, "symbol") { Some(s) if !s.is_empty() => s.to_string(), _ => return bad("symbol required") };
            let supply = jnum(body, "supply").unwrap_or(1_000_000);
            let to = jstr(body, "to").and_then(hex32).unwrap_or(OPERATOR);
            let id = token_id_for(&symbol);
            let mut n = node.write().unwrap();
            // C5: the creator (`to`) must sign — was unauthenticated, so anyone
            // could mint arbitrary supply of any token to any wallet.
            if let Err(e) = authorize(&mut n, &to, "deploy_token",
                &[symbol.clone(), supply.to_string(), to_hex(&to)], body) { return bad(&e); }
            if n.tokens.iter().any(|(s, _)| s.eq_ignore_ascii_case(&symbol)) { return bad("token symbol already deployed"); }
            let h = n.height;
            let cur = n.state.balance_of(&to, &id);
            match commit_state_transition(&mut n.state, &StateTransition { at_height: h, mutations: vec![StateMutation::SetBalance { wallet: to, token: id, amount: cur.saturating_add(supply) }] }, h) {
                Ok(_) => { n.height += 1; n.tokens.push((symbol.clone(), id)); ok(format!("{{\"ok\":true,\"symbol\":\"{}\",\"id\":\"{}\",\"supply\":{},\"to\":\"{}\"}}", symbol, to_hex(&id), supply, to_hex(&to))) }
                Err(e) => bad(&e.to_string()),
            }
        }
        ("POST", "/add_liquidity") => {
            let from = jstr(body, "from").and_then(hex32);
            let pool = jstr(body, "pool").and_then(hex32);
            let amt_a = jnum(body, "amount_a");
            let amt_b = jnum(body, "amount_b");
            match (from, pool, amt_a, amt_b) {
                (Some(from), Some(pool), Some(amt_a), Some(amt_b)) if amt_a > 0 && amt_b > 0 => {
                    let mut n = node.write().unwrap();
                    // C3: `from` must sign — was unauthenticated, so anyone could
                    // add liquidity FROM another wallet's balance.
                    if let Err(e) = authorize(&mut n, &from, "add_liquidity",
                        &[to_hex(&from), to_hex(&pool), amt_a.to_string(), amt_b.to_string()], body) { return bad(&e); }
                    let prev = match n.state.pool(&pool) { Some(p) => p.clone(), None => return bad("unknown pool") };
                    // proportional shares vs the limiting side (Uniswap-V2 model).
                    let s_a = amt_a.saturating_mul(prev.lp_shares) / prev.reserve_a.max(1);
                    let s_b = amt_b.saturating_mul(prev.lp_shares) / prev.reserve_b.max(1);
                    let shares = s_a.min(s_b);
                    if shares == 0 { return bad("deposit too small to mint shares"); }
                    // checked: unbounded u128 + could panic (debug) / wrap (release) the reserves.
                    let (ra, rb, ls) = match (
                        prev.reserve_a.checked_add(amt_a),
                        prev.reserve_b.checked_add(amt_b),
                        prev.lp_shares.checked_add(shares),
                    ) {
                        (Some(ra), Some(rb), Some(ls)) => (ra, rb, ls),
                        _ => return bad("reserve/shares overflow"),
                    };
                    let pool_after = PoolState {
                        token_a: prev.token_a, token_b: prev.token_b,
                        reserve_a: ra, reserve_b: rb,
                        lp_shares: ls, fee_bps: prev.fee_bps, accrued_fees: prev.accrued_fees,
                    };
                    let h = n.height;
                    match commit_state_transition(&mut n.state, &StateTransition { at_height: h, mutations: vec![StateMutation::LpDelta { from, pool, amt_a, amt_b, shares_minted: shares, fee: 0, pool_after }] }, h) {
                        Ok(_) => { n.height += 1; ok(format!("{{\"ok\":true,\"shares_minted\":{},\"pool\":\"{}\"}}", shares, to_hex(&pool))) }
                        Err(e) => bad(&e.to_string()),
                    }
                }
                _ => bad("need from,pool (64-hex) + amount_a,amount_b > 0"),
            }
        }
        ("POST", "/onboard") => {
            // seed: explicit 64-hex, else derive from time entropy (xorshift).
            let seed: [u8; 32] = jstr(body, "seed").and_then(hex32).unwrap_or_else(|| {
                let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(1);
                let mut s = [0u8; 32];
                let mut x = nanos as u64 ^ 0x9E3779B97F4A7C15;
                for o in s.iter_mut() { x ^= x >> 12; x ^= x << 25; x ^= x >> 27; *o = (x.wrapping_mul(0x2545F4914F6CDD1D) >> 32) as u8; }
                s
            });
            let mut n = node.write().unwrap();
            // C4 — per-IP rate-limit. /onboard can't pre-auth (cold start), so the
            // sybil defense is a finite faucet (below) + this throttle. Max
            // SIGIL_ONBOARD_MAX_PER_HOUR (default 3) onboards per client IP per hour.
            {
                let max_per_window: u32 = std::env::var("SIGIL_ONBOARD_MAX_PER_HOUR").ok()
                    .and_then(|s| s.parse().ok()).unwrap_or(3);
                const WINDOW_MS: u64 = 3_600_000;
                let now = now_ms();
                let e = n.onboard_rl.entry(peer_ip.to_string()).or_insert((now, 0));
                if now.saturating_sub(e.0) >= WINDOW_MS { *e = (now, 0); } // window rolled over
                if e.1 >= max_per_window {
                    return bad("onboard rate limit exceeded for this IP — try again later");
                }
                e.1 += 1;
            }
            let nonce = n.height;
            let (kp, verification) = onboard::onboard_user(&seed, nonce);
            let wallet = kp.pubkey();
            // student: register the self-signed verification (the earning gate).
            if let Err(e) = n.students.register(verification) { return bad(&format!("student: {:?}", e)); }
            // nation: attest as citizen with a derived (non-zero) cpr_hash.
            let mut cpr = [0u8; 32];
            for (c, w) in cpr.iter_mut().zip(wallet.iter()) { *c = w ^ 0x5a; }
            cpr[0] |= 1;
            let h = n.height;
            if let Err(e) = nation::attest_citizen(&mut n.state, h, nation::BORGER_AUTHORITY, wallet, cpr) { return bad(&format!("nation: {:?}", e)); }
            n.height += 1;
            // C4 — starter funding is now a TRANSFER from the OPERATOR faucet, NOT a
            // mint. This conserves total supply (the old `saturating_add` minted
            // free NATIVE+USDS on every call — an unbounded inflation faucet) and is
            // intrinsically finite: grants stop when OPERATOR is drained. Grant
            // min(target, available) so a near-empty faucet degrades gracefully.
            let h2 = n.height;
            let op_nat = n.state.balance_of(&OPERATOR, &NATIVE);
            let op_usd = n.state.balance_of(&OPERATOR, &USDS);
            let grant_nat = 100u128.min(op_nat);
            let grant_usd = 1_000u128.min(op_usd);
            let nat = n.state.balance_of(&wallet, &NATIVE).saturating_add(grant_nat);
            let usd = n.state.balance_of(&wallet, &USDS).saturating_add(grant_usd);
            let _ = commit_state_transition(&mut n.state, &StateTransition { at_height: h2, mutations: vec![
                StateMutation::SetBalance { wallet: OPERATOR, token: NATIVE, amount: op_nat - grant_nat },
                StateMutation::SetBalance { wallet, token: NATIVE, amount: nat },
                StateMutation::SetBalance { wallet: OPERATOR, token: USDS, amount: op_usd - grant_usd },
                StateMutation::SetBalance { wallet, token: USDS, amount: usd },
            ] }, h2);
            n.height += 1;
            n.citizens.push(wallet);
            ok(format!("{{\"ok\":true,\"wallet\":\"{}\",\"seed\":\"{}\",\"student\":true,\"citizen\":true,\"native\":{},\"usds\":{}}}",
                to_hex(&wallet), to_hex(&seed), nat, usd))
        }
        ("GET", "/nation/citizens") => {
            let n = node.read().unwrap();
            let items: Vec<String> = n.citizens.iter().map(|w| format!("\"{}\"", to_hex(w))).collect();
            ok(format!("{{\"ok\":true,\"count\":{},\"citizens\":[{}]}}", n.citizens.len(), items.join(",")))
        }
        ("POST", "/swap") => {
            let from = jstr(body, "from").and_then(hex32);
            let pool = jstr(body, "pool").and_then(hex32);
            let dir = match jstr(body, "dir") { Some("AtoB") => SwapDirection::AtoB, Some("BtoA") => SwapDirection::BtoA, _ => return bad("dir must be AtoB|BtoA") };
            let amount_in = jnum(body, "amount_in");
            let min_out = jnum(body, "min_out").unwrap_or(1);
            match (from, pool, amount_in) {
                (Some(from), Some(pool), Some(amount_in)) => {
                    let mut n = node.write().unwrap();
                    // C3: `from` must sign — was unauthenticated, so anyone could
                    // swap FROM another wallet's balance.
                    let dir_s = match dir { SwapDirection::AtoB => "AtoB", SwapDirection::BtoA => "BtoA" };
                    if let Err(e) = authorize(&mut n, &from, "swap",
                        &[to_hex(&from), to_hex(&pool), dir_s.to_string(), amount_in.to_string(), min_out.to_string()], body) { return bad(&e); }
                    let h = n.height;
                    match execute_swap(&mut n.state, h, from, pool, dir, amount_in, min_out) {
                        Ok(r) => { n.height += 1; ingest(&mut n, "swap", format!("swap {amount_in} in → {} out", r.amount_out), &[from], "swap dex"); ok(format!("{{\"ok\":true,\"amount_out\":{},\"lp_fee\":{},\"protocol_fee\":{}}}", r.amount_out, r.fee, r.protocol_fee)) }
                        Err(e) => bad(&e.to_string()),
                    }
                }
                _ => bad("need from,pool (64-hex) + amount_in"),
            }
        }
        // Diagnostic collector — miners POST GPU init/search errors here so they
        // can be read server-side (no file-pasting). Appends the raw body to a log.
        ("POST", "/diag") => {
            use std::io::Write;
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open("/home/orobit/sigil-miner-diag.log")
            {
                let _ = writeln!(f, "{}", body);
            }
            ok("{\"ok\":true}".into())
        }
        ("POST", "/mine") => {
            let miner = jstr(body, "miner").and_then(hex32);
            let header = jstr(body, "header").unwrap_or("sigil-block").to_string();
            let pow_nonce = jnum(body, "nonce").unwrap_or(0) as u64;
            match miner {
                Some(miner) => {
                    let mut n = node.write().unwrap();
                    // C1 (the worst hole): `difficulty` and `reward` USED to be
                    // caller-supplied — difficulty=0 made every PoW pass and an
                    // arbitrary reward let one unauthenticated POST mint the whole
                    // 21M cap. Now: the miner must SIGN (prove wallet control), the
                    // difficulty is the live server target, and the reward is the
                    // emission-schedule value. The req_nonce also blocks the replay
                    // (the legacy /mine PoW is deterministic). Prefer /mining/submit
                    // (dual-lane, tip-bound); this legacy lane is now at least safe.
                    if let Err(e) = authorize(&mut n, &miner, "mine",
                        &[to_hex(&miner), header.clone(), pow_nonce.to_string()], body) { return bad(&e); }
                    let difficulty = n.bits;
                    // LANE-R: the legacy single-lane /mine credits LOCALLY (no stored/verifiable
                    // block), so it must NOT touch the time-based emission clock (last_block_ts_us)
                    // — advancing it here but not on a follower would FORK the reward of the next
                    // dual-lane block. On a time-based chain it's disabled (use /mining/submit, the
                    // verifiable lane). On the legacy block-based chain it keeps the old schedule.
                    if n.genesis_ts_us != 0 {
                        return bad("legacy /mine is disabled on time-based emission — use /mining/submit (dual-lane, verifiable)");
                    }
                    let reward = sigil_emission::block_reward(n.block_height);
                    let h = n.height;
                    match submit_share(&mut n.state, h, miner, header.as_bytes(), pow_nonce, difficulty, reward) {
                        Ok(bal) => { n.height += 1; ingest(&mut n, "mine", format!("mine reward {reward} SIGIL"), &[miner], "mining block reward"); ok(format!("{{\"ok\":true,\"new_balance\":{}}}", bal)) }
                        Err(e) => bad(&e.to_string()),
                    }
                }
                None => bad("miner must be 64-hex"),
            }
        }
        // ── Dual-lane miner (flux-miner): the BLAKE4 Φ + VDF Ω work surface. ──
        // GET issues a height-bound challenge; POST verifies the solved block via
        // flux-miner's shared `check_submission` gate, then credits through the
        // cap-enforced money chokepoint. The wallet is bound into the header at
        // solve time, so a share can't be re-pointed to a different miner.
        ("GET", "/mining/challenge") => {
            let n = node.read().unwrap();
            let h = n.block_height; // mining-chain height (contiguous; emission-clean)
            // seed binds to the CURRENT tip → unpredictable until the prior block lands.
            let c = Challenge {
                height: h,
                vdf_input: mining_seed(&n.tip_hash, h),
                blake4_target: target_from_bits(n.bits),
                vdf_t: mining_vdf_t(),
            };
            ok(serde_json::to_string(&c).unwrap_or_else(|_| "{}".into()))
        }
        ("POST", "/mining/submit") => {
            let sub: Submission = match serde_json::from_str(body) {
                Ok(s) => s,
                Err(e) => return bad(&format!("bad submission json: {e}")),
            };
            let miner = match hex32(&sub.wallet) {
                Some(w) => w,
                None => return bad("wallet must be 64-hex"),
            };
            let reject = |reason: String| {
                let r = SubmitResult { accepted: false, reason: Some(reason) };
                ok(serde_json::to_string(&r).unwrap_or_else(|_| "{}".into()))
            };
            // A follower node syncs from a peer and must NOT mint its own blocks (would fork).
            if std::env::var("SIGIL_FOLLOW_PEER").is_ok() {
                return reject("this node is a follower (SIGIL_FOLLOW_PEER set) — it syncs, does not mine".into());
            }
            // Take the write lock FIRST so the height + tip we validate against are the
            // ones we'll advance — no TOCTOU between check and credit.
            let mut n = node.write().unwrap();
            let bh = n.block_height; // MINING-chain height (contiguous; emission-clean)
            // fix #1 (precompute): ONLY the current tip is mineable. A precomputed
            // submission for a future/stale height is rejected here, and even at the
            // right height its seed (BLAKE3(tip‖bh)) won't match unless it was built on
            // THIS tip — which is unknown until the prior block was accepted.
            if sub.height != bh {
                return reject(format!("stale height: submitted {} but the mineable tip is {}", sub.height, bh));
            }
            let c = Challenge {
                height: bh,
                vdf_input: mining_seed(&n.tip_hash, bh),
                blake4_target: target_from_bits(n.bits),
                vdf_t: mining_vdf_t(),
            };
            let g = ModSquaring::bench_2048();
            if !check_submission(&g, &c, &sub) {
                return reject("dual-lane verify / header mismatch (wrong tip, target, or VDF)".into());
            }
            // Verified on the current tip — credit the HALVING block reward on the MINING
            // height (block_reward(bh) — swap volume no longer distorts emission). Schedule +
            // 21M cap are two independent guards. State event ordered by the global height.
            // LANE-R TIME-BASED EMISSION: reward = ∫ rate dt over [last_block_ts, now] (stateless
            // carry=0, deterministic). genesis_ts_us==0 → pre-LANE-R chain, keep the block-based
            // schedule (time-based only activates on a fresh genesis that anchors genesis_ts_us).
            let ts_us = now_us();
            // EXACT CARRY: thread the sub-unit remainder so nothing is lost to truncation; the
            // returned carry is committed AFTER the block is accepted (alongside last_block_ts).
            let (reward, new_carry) = if n.genesis_ts_us == 0 {
                (sigil_emission::block_reward(bh), 0u128)
            } else {
                sigil_emission::block_reward_time(n.genesis_ts_us, n.last_block_ts_us, ts_us, n.emission_carry)
            };
            let eh = n.height;
            match credit_share(&mut n.state, eh, miner, reward) {
                Ok(bal) => {
                    // fix #3 (VDF-chaining): fold the VDF OUTPUT into the tip → height bh+1's
                    // challenge depends on this block's VDF output (sequential timeline).
                    let prev_tip = n.tip_hash;
                    let new_tip = fold_tip(&prev_tip, bh, sub.block.blake4_hash, sub.block.nonce, &sub.block.vdf);
                    n.tip_hash = new_tip;
                    // decentralization step ①: persist the block (+ its µs ts) so PEERS can pull
                    // + recompute the SAME time-based reward.
                    store_block(&n, bh, &sub, reward, prev_tip, new_tip, ts_us);
                    n.last_block_ts_us = ts_us;   // advance the emission clock for the next block
                    n.emission_carry = new_carry; // commit the carried sub-unit remainder
                    n.block_height += 1;
                    n.height += 1;
                    retarget(&mut n); // fix #2: adjust BLAKE4 difficulty from real block times
                    ingest(&mut n, "mine", format!("dual-lane block #{bh} reward {} (halving)", reward), &[miner], "dual-lane blake4+vdf");
                    ok(format!("{{\"accepted\":true,\"reason\":null,\"block_height\":{},\"new_balance\":{}}}", bh, bal))
                }
                Err(e) => reject(e.to_string()),
            }
        }
        ("POST", "/credit") => {
            let opw = jstr(body, "operator_pool").and_then(hex32);
            let pool_amount = jnum(body, "pool_amount").unwrap_or(0);
            // verifiers: comma-joined 64-hex inside the array, parsed loosely
            let verifiers: Vec<WalletId> = body
                .split('"').filter_map(hex32).filter(|w| *w != opw.unwrap_or([0u8;32])).collect();
            match opw {
                Some(opw) => {
                    let mut n = node.write().unwrap();
                    // H8: the operator pool owner must sign — was unauthenticated,
                    // so anyone could drain any funded wallet by naming it as the
                    // `operator_pool` and themselves as the sole verifier. The
                    // req_nonce also closes the double-credit replay the lib warns of.
                    let vjoin = verifiers.iter().map(|w| to_hex(w)).collect::<Vec<_>>().join(",");
                    if let Err(e) = authorize(&mut n, &opw, "credit",
                        &[to_hex(&opw), pool_amount.to_string(), vjoin], body) { return bad(&e); }
                    let h = n.height;
                    match credit_light_verifiers(&mut n.state, h, opw, pool_amount, &verifiers) {
                        Ok(r) => { n.height += 1; ok(format!("{{\"ok\":true,\"per_verifier\":{},\"credited\":{},\"num_verifiers\":{}}}", r.per_verifier, r.credited, r.num_verifiers)) }
                        Err(e) => bad(&e.to_string()),
                    }
                }
                None => bad("operator_pool must be 64-hex"),
            }
        }
        // ── LANE-X collateral credit (QCREDIT port): lock SIGIL → mint CREDIT @50% LTV. ──
        // Vault math lives in sigil-bank::credit (pure); every balance move below goes
        // through the commit_state_transition chokepoint; the vault persists in its own
        // flux-db key so a restart NEVER loses a loan (the LANE-X acceptance gate).
        ("GET", "/credit/status") => {
            let n = node.read().unwrap();
            let s = n.credit_vault.status();
            let vault_native = n.state.balance_of(&sigil_bank::credit::CREDIT_VAULT_WALLET, &NATIVE);
            ok(serde_json::json!({
                "ok": true,
                "total_collateral": s.total_collateral.to_string(),
                "total_credit_supply": s.total_credit_supply.to_string(),
                "protocol_reserve": s.protocol_reserve.to_string(),
                "total_yield_paid": s.total_yield_paid.to_string(),
                "total_liquidated": s.total_liquidated.to_string(),
                "position_count": s.position_count,
                "vault_wallet": to_hex(&sigil_bank::credit::CREDIT_VAULT_WALLET),
                "vault_native_balance": vault_native.to_string(),
                "credit_token": to_hex(&sigil_bank::credit::CREDIT_TOKEN),
                "ltv_bps": sigil_bank::credit::LTV_BPS as u64,
                "tiers": s.tiers.iter().map(|t| serde_json::json!({
                    "name": t.tier.display_name(), "lock_days": t.lock_days,
                    "apy_percent": t.apy_percent, "ltv_percent": t.ltv_percent,
                })).collect::<Vec<_>>(),
            }).to_string())
        }
        ("GET", "/credit/tiers") => {
            let tiers = sigil_bank::credit::CreditVault::get_tiers();
            ok(serde_json::json!({
                "ok": true,
                "ltv_bps": sigil_bank::credit::LTV_BPS as u64,
                "tiers": tiers.iter().map(|t| serde_json::json!({
                    "name": t.tier.display_name(), "lock_days": t.lock_days,
                    "apy_percent": t.apy_percent, "ltv_percent": t.ltv_percent,
                })).collect::<Vec<_>>(),
            }).to_string())
        }
        ("GET", "/credit/position") => {
            let w = match query_get(query, "wallet").and_then(hex32) {
                Some(w) => w, None => return bad("wallet must be 64-hex"),
            };
            let now = now_ms() / 1000;
            let n = node.read().unwrap();
            let mut total_locked = 0u128;
            let mut total_pending = 0u128;
            let details: Vec<serde_json::Value> = n.credit_vault.positions_with_yield(&w, now)
                .iter().enumerate().map(|(i, (p, pending))| {
                    total_locked = total_locked.saturating_add(p.collateral_locked);
                    total_pending = total_pending.saturating_add(*pending);
                    let remaining = p.unlock_timestamp.saturating_sub(now);
                    serde_json::json!({
                        "index": i,
                        "collateral_locked": p.collateral_locked.to_string(),
                        "credit_minted": p.credit_minted.to_string(),
                        "tier": p.tier.display_name(),
                        "apy_percent": p.tier.apy_bps() as f64 / 100.0,
                        "lock_timestamp": p.lock_timestamp,
                        "unlock_timestamp": p.unlock_timestamp,
                        "is_unlockable": p.is_unlockable(now),
                        "is_breached": p.is_breached(now),
                        "claimed_yield": p.claimed_yield.to_string(),
                        "pending_yield": pending.to_string(),
                        "lock_days_remaining": remaining / 86_400,
                    })
                }).collect();
            ok(serde_json::json!({
                "ok": true, "wallet": to_hex(&w), "positions": details,
                "total_collateral": total_locked.to_string(),
                "total_pending_yield": total_pending.to_string(),
                "credit_balance": n.state.balance_of(&w, &sigil_bank::credit::CREDIT_TOKEN).to_string(),
            }).to_string())
        }
        ("POST", "/credit/lock") => {
            let wallet = match jstr(body, "wallet").and_then(hex32) { Some(w) => w, None => return bad("wallet must be 64-hex") };
            let amount = match jnum(body, "amount") { Some(a) if a > 0 => a, _ => return bad("amount required (positive, SIGIL base units)") };
            let tier = match jstr(body, "tier").and_then(sigil_bank::credit::CreditTier::from_str_name) {
                Some(t) => t, None => return bad("tier must be bronze|silver|gold|platinum"),
            };
            let now = now_ms() / 1000;
            let mut n = node.write().unwrap();
            // the locker must sign — collateral moves FROM this wallet.
            if let Err(e) = authorize(&mut n, &wallet, "credit_lock",
                &[to_hex(&wallet), amount.to_string(), tier.display_name().to_lowercase()], body) { return bad(&e); }
            let vault_w = sigil_bank::credit::CREDIT_VAULT_WALLET;
            let bal = n.state.balance_of(&wallet, &NATIVE);
            if bal < amount { return bad(&format!("insufficient SIGIL: have {bal}, need {amount}")); }
            let vault_bal = n.state.balance_of(&vault_w, &NATIVE);
            let credit_bal = n.state.balance_of(&wallet, &sigil_bank::credit::CREDIT_TOKEN);
            // vault math first (pure, rollback-able), then ONE atomic chokepoint commit.
            let before = n.credit_vault.clone();
            let pos = match n.credit_vault.lock(wallet, amount, tier, now) { Ok(p) => p, Err(e) => return bad(&e) };
            let h = n.height;
            let mutations = vec![
                StateMutation::SetBalance { wallet, token: NATIVE, amount: bal - amount },
                StateMutation::SetBalance { wallet: vault_w, token: NATIVE, amount: vault_bal.saturating_add(amount) },
                StateMutation::SetBalance { wallet, token: sigil_bank::credit::CREDIT_TOKEN, amount: credit_bal.saturating_add(pos.credit_minted) },
            ];
            match commit_state_transition(&mut n.state, &StateTransition { at_height: h, mutations }, h) {
                Ok(_) => {
                    n.height += 1;
                    ingest(&mut n, "credit", format!("credit lock {amount} SIGIL → {} CREDIT ({} tier)", pos.credit_minted, tier.display_name()), &[wallet], "credit lock collateral");
                    ok(serde_json::json!({
                        "ok": true, "collateral_locked": amount.to_string(),
                        "credit_minted": pos.credit_minted.to_string(),
                        "tier": tier.display_name(), "unlock_timestamp": pos.unlock_timestamp,
                    }).to_string())
                }
                Err(e) => { n.credit_vault = before; bad(&e.to_string()) }
            }
        }
        ("POST", "/credit/unlock") => {
            let wallet = match jstr(body, "wallet").and_then(hex32) { Some(w) => w, None => return bad("wallet must be 64-hex") };
            let idx = jnum(body, "position_index").unwrap_or(0) as usize;
            let now = now_ms() / 1000;
            let mut n = node.write().unwrap();
            if let Err(e) = authorize(&mut n, &wallet, "credit_unlock",
                &[to_hex(&wallet), idx.to_string()], body) { return bad(&e); }
            let vault_w = sigil_bank::credit::CREDIT_VAULT_WALLET;
            // the wallet must hold the full mint back — unlock burns it.
            let need_burn = match n.credit_vault.positions.get(&wallet).and_then(|v| v.get(idx)) {
                Some(p) => p.credit_minted, None => return bad("no such position"),
            };
            let credit_bal = n.state.balance_of(&wallet, &sigil_bank::credit::CREDIT_TOKEN);
            if credit_bal < need_burn {
                return bad(&format!("insufficient CREDIT to unlock: have {credit_bal}, must burn {need_burn} (the full mint)"));
            }
            let before = n.credit_vault.clone();
            let out = match n.credit_vault.unlock(&wallet, idx, now) { Ok(o) => o, Err(e) => return bad(&e) };
            let payout = out.collateral_returned.saturating_add(out.yield_paid);
            let vault_bal = n.state.balance_of(&vault_w, &NATIVE);
            let Some(vault_after) = vault_bal.checked_sub(payout) else {
                n.credit_vault = before;
                return bad("vault underfunded — invariant breach, refusing payout");
            };
            let nat_bal = n.state.balance_of(&wallet, &NATIVE);
            let h = n.height;
            let mutations = vec![
                StateMutation::SetBalance { wallet, token: sigil_bank::credit::CREDIT_TOKEN, amount: credit_bal - out.credit_burned },
                StateMutation::SetBalance { wallet: vault_w, token: NATIVE, amount: vault_after },
                StateMutation::SetBalance { wallet, token: NATIVE, amount: nat_bal.saturating_add(payout) },
            ];
            match commit_state_transition(&mut n.state, &StateTransition { at_height: h, mutations }, h) {
                Ok(_) => {
                    n.height += 1;
                    ingest(&mut n, "credit", format!("credit unlock: {} SIGIL returned + {} yield, {} CREDIT burned", out.collateral_returned, out.yield_paid, out.credit_burned), &[wallet], "credit unlock");
                    ok(serde_json::json!({
                        "ok": true, "collateral_returned": out.collateral_returned.to_string(),
                        "credit_burned": out.credit_burned.to_string(),
                        "yield_paid": out.yield_paid.to_string(),
                        "total_received": payout.to_string(),
                    }).to_string())
                }
                Err(e) => { n.credit_vault = before; bad(&e.to_string()) }
            }
        }
        ("POST", "/credit/claim") => {
            let wallet = match jstr(body, "wallet").and_then(hex32) { Some(w) => w, None => return bad("wallet must be 64-hex") };
            let idx = jnum(body, "position_index").unwrap_or(0) as usize;
            let now = now_ms() / 1000;
            let mut n = node.write().unwrap();
            if let Err(e) = authorize(&mut n, &wallet, "credit_claim",
                &[to_hex(&wallet), idx.to_string()], body) { return bad(&e); }
            let vault_w = sigil_bank::credit::CREDIT_VAULT_WALLET;
            let before = n.credit_vault.clone();
            let y = match n.credit_vault.claim_yield(&wallet, idx, now) { Ok(y) => y, Err(e) => return bad(&e) };
            let vault_bal = n.state.balance_of(&vault_w, &NATIVE);
            let Some(vault_after) = vault_bal.checked_sub(y) else {
                n.credit_vault = before;
                return bad("vault underfunded — invariant breach, refusing payout");
            };
            let nat_bal = n.state.balance_of(&wallet, &NATIVE);
            let h = n.height;
            let mutations = vec![
                StateMutation::SetBalance { wallet: vault_w, token: NATIVE, amount: vault_after },
                StateMutation::SetBalance { wallet, token: NATIVE, amount: nat_bal.saturating_add(y) },
            ];
            match commit_state_transition(&mut n.state, &StateTransition { at_height: h, mutations }, h) {
                Ok(_) => {
                    n.height += 1;
                    ingest(&mut n, "credit", format!("credit yield claim {y} SIGIL"), &[wallet], "credit claim yield");
                    ok(serde_json::json!({ "ok": true, "yield_claimed": y.to_string() }).to_string())
                }
                Err(e) => { n.credit_vault = before; bad(&e.to_string()) }
            }
        }
        ("POST", "/credit/liquidate") => {
            // Breach enforcement: term + grace expired without unlock → the bank pool
            // takes the collateral. Destination is HARDCODED (MASTER), the breach test
            // is chain-state-determined, so the caller gains nothing — callable by anyone
            // (a keeper). The minted CREDIT stays circulating, backed 2:1 by the seizure.
            // INTENTIONALLY NOT gated by authorize(): permissionless keeper route — the
            // destination is hardcoded (MASTER), the breach is chain-state-determined, and
            // the caller gains nothing, so requiring a signature would only break liveness.
            let wallet = match jstr(body, "wallet").and_then(hex32) { Some(w) => w, None => return bad("wallet must be 64-hex") };
            let idx = jnum(body, "position_index").unwrap_or(0) as usize;
            let now = now_ms() / 1000;
            let mut n = node.write().unwrap();
            let vault_w = sigil_bank::credit::CREDIT_VAULT_WALLET;
            let before = n.credit_vault.clone();
            let out = match n.credit_vault.liquidate(&wallet, idx, now) { Ok(o) => o, Err(e) => return bad(&e) };
            let vault_bal = n.state.balance_of(&vault_w, &NATIVE);
            let Some(vault_after) = vault_bal.checked_sub(out.collateral_seized) else {
                n.credit_vault = before;
                return bad("vault underfunded — invariant breach, refusing seizure");
            };
            let bank_bal = n.state.balance_of(&MASTER, &NATIVE);
            let h = n.height;
            let mutations = vec![
                StateMutation::SetBalance { wallet: vault_w, token: NATIVE, amount: vault_after },
                StateMutation::SetBalance { wallet: MASTER, token: NATIVE, amount: bank_bal.saturating_add(out.collateral_seized) },
            ];
            match commit_state_transition(&mut n.state, &StateTransition { at_height: h, mutations }, h) {
                Ok(_) => {
                    n.height += 1;
                    ingest(&mut n, "credit", format!("credit LIQUIDATION: bank seized {} SIGIL ({} CREDIT outstanding)", out.collateral_seized, out.credit_outstanding), &[wallet], "credit liquidate breach");
                    ok(serde_json::json!({
                        "ok": true, "collateral_seized": out.collateral_seized.to_string(),
                        "credit_outstanding": out.credit_outstanding.to_string(),
                    }).to_string())
                }
                Err(e) => { n.credit_vault = before; bad(&e.to_string()) }
            }
        }
        ("POST", "/credit/fund") => {
            // Fund the yield reserve: a signed transfer from `from` into the vault
            // wallet, mirrored into the vault's reserve accounting (bank fees land here).
            let from = match jstr(body, "from").and_then(hex32) { Some(w) => w, None => return bad("from must be 64-hex") };
            let amount = match jnum(body, "amount") { Some(a) if a > 0 => a, _ => return bad("amount required (positive)") };
            let mut n = node.write().unwrap();
            if let Err(e) = authorize(&mut n, &from, "credit_fund",
                &[to_hex(&from), amount.to_string()], body) { return bad(&e); }
            let vault_w = sigil_bank::credit::CREDIT_VAULT_WALLET;
            let bal = n.state.balance_of(&from, &NATIVE);
            if bal < amount { return bad(&format!("insufficient SIGIL: have {bal}, need {amount}")); }
            let vault_bal = n.state.balance_of(&vault_w, &NATIVE);
            let h = n.height;
            let mutations = vec![
                StateMutation::SetBalance { wallet: from, token: NATIVE, amount: bal - amount },
                StateMutation::SetBalance { wallet: vault_w, token: NATIVE, amount: vault_bal.saturating_add(amount) },
            ];
            match commit_state_transition(&mut n.state, &StateTransition { at_height: h, mutations }, h) {
                Ok(_) => {
                    n.height += 1;
                    n.credit_vault.fund_reserve(amount);
                    ingest(&mut n, "credit", format!("credit reserve funded +{amount} SIGIL"), &[from], "credit fund reserve");
                    ok(serde_json::json!({ "ok": true, "funded": amount.to_string(),
                        "protocol_reserve": n.credit_vault.protocol_reserve.to_string() }).to_string())
                }
                Err(e) => bad(&e.to_string()),
            }
        }
        ("OPTIONS", _) => ok("{}".into()),
        ("GET", "/nation/state") => {
            let n = node.read().unwrap();
            ok(format!("{{\"ok\":true,\"citizen\":\"{}\",\"is_citizen\":{},\"citizen_native\":{},\"provider_native\":{}}}",
                to_hex(&CITIZEN), nation::is_citizen(&n.state, &CITIZEN),
                n.state.balance_of(&CITIZEN, &NATIVE), n.state.balance_of(&POWER_CO, &NATIVE)))
        }
        ("POST", "/nation/pay") => {
            let amount = jnum(body, "amount").unwrap_or(0);
            if amount == 0 { bad("amount required (positive)") }
            else {
                let mut n = node.write().unwrap();
                // C2/audit: the citizen whose funds move must sign — was unauthenticated,
                // so anyone could drain the demo citizen wallet on the public daemon.
                if let Err(e) = authorize(&mut n, &CITIZEN, "nation_pay",
                    &[to_hex(&CITIZEN), amount.to_string()], body) { return bad(&e); }
                let h = n.height;
                match nation::pay_utility_bill(&mut n.state, h, CITIZEN, POWER_CO, amount) {
                    Ok(left) => { n.height += 1; ok(format!("{{\"ok\":true,\"paid\":{},\"citizen_native\":{},\"provider_native\":{}}}",
                        amount, left, n.state.balance_of(&POWER_CO, &NATIVE))) }
                    Err(e) => bad(&format!("nation: {:?}", e)),
                }
            }
        }
        ("POST", "/nation/eboks") => {
            match jstr(body, "doc").and_then(hex32) {
                Some(doc) => {
                    let mut n = node.write().unwrap();
                    // C2/audit: CITIZEN must sign — was unauthenticated state mutation.
                    if let Err(e) = authorize(&mut n, &CITIZEN, "nation_eboks",
                        &[to_hex(&CITIZEN), to_hex(&doc)], body) { return bad(&e); }
                    let h = n.height;
                    match nation::issue_eboks_receipt(&mut n.state, h, CITIZEN, doc) {
                        Ok(()) => { n.height += 1; ok(format!("{{\"ok\":true,\"doc\":\"{}\"}}", to_hex(&doc))) }
                        Err(e) => bad(&format!("nation: {:?}", e)),
                    }
                }
                None => bad("doc must be 64-hex"),
            }
        }
        ("GET", "/nation/verify") => {
            match query_get(query, "doc").and_then(hex32) {
                Some(doc) => {
                    let n = node.read().unwrap();
                    ok(format!("{{\"ok\":true,\"verified\":{}}}", nation::verify_eboks_receipt(&n.state, &CITIZEN, &doc)))
                }
                None => bad("doc must be 64-hex"),
            }
        }
        // ── LANE-Y: agent spend-mandates (ON-CHAIN — CLI + MCP share one truth) ─────────
        ("POST", "/mandate/create") => {
            let agent = match jstr(body, "agent").and_then(hex32) { Some(a) => a, None => return bad("agent must be 64-hex") };
            let max_amount = match jnum(body, "max_amount") { Some(m) if m > 0 => m, _ => return bad("max_amount required (>0, base units)") };
            let purpose = jstr(body, "purpose").unwrap_or("").to_string();
            let ttl_secs = jnum(body, "ttl_secs").unwrap_or(86_400) as u64; // default 24h
            let mut n = node.write().unwrap();
            // the agent signs — a mandate is the agent's own bounded spend authority
            if let Err(e) = authorize(&mut n, &agent, "mandate_create",
                &[to_hex(&agent), max_amount.to_string(), purpose.clone(), ttl_secs.to_string()], body) { return bad(&e); }
            let now = now_ms() / 1000;
            let id = format!("mn-{}", &to_hex(blake3::hash(format!("{}|{}|{}", to_hex(&agent), now, n.mandates.mandates.len()).as_bytes()).as_bytes())[..12]);
            let m = n.mandates.create(id, agent, max_amount, purpose, ttl_secs, now);
            persist(&n);
            ok(format!("{{\"ok\":true,\"id\":\"{}\",\"agent\":\"{}\",\"max_amount\":{},\"expires_ts\":{},\"purpose\":{}}}",
                m.id, to_hex(&agent), m.max_amount, m.expires_ts, serde_json::to_string(&m.purpose).unwrap_or_else(|_| "\"\"".into())))
        }
        ("GET", "/mandate/list") => {
            let n = node.read().unwrap();
            let now = now_ms() / 1000;
            let items: Vec<String> = n.mandates.mandates.iter().map(|m| format!(
                "{{\"id\":\"{}\",\"agent\":\"{}\",\"max_amount\":{},\"spent\":{},\"headroom\":{},\"purpose\":{},\"expires_ts\":{},\"status\":\"{}\",\"live\":{}}}",
                m.id, to_hex(&m.agent), m.max_amount, m.spent, m.headroom(),
                serde_json::to_string(&m.purpose).unwrap_or_else(|_| "\"\"".into()), m.expires_ts, m.status, m.is_live(now))).collect();
            ok(format!("{{\"ok\":true,\"mandates\":[{}]}}", items.join(",")))
        }
        ("POST", "/mandate/close") => {
            let id = jstr(body, "id").unwrap_or("").to_string();
            let agent = match jstr(body, "agent").and_then(hex32) { Some(a) => a, None => return bad("agent must be 64-hex") };
            let mut n = node.write().unwrap();
            match n.mandates.get(&id) { Some(m) if m.agent == agent => {}, Some(_) => return bad("not your mandate"), None => return bad("mandate not found") }
            if let Err(e) = authorize(&mut n, &agent, "mandate_close", &[id.clone(), to_hex(&agent)], body) { return bad(&e); }
            n.mandates.close(&id);
            persist(&n);
            ok(format!("{{\"ok\":true,\"id\":\"{}\",\"status\":\"closed\"}}", id))
        }
        // ── LANE-Y: bank 2-of-2 council treasury transfers ─────────────────────────────
        ("GET", "/bank/status") => {
            let n = node.read().unwrap();
            let members: Vec<String> = n.council.members.iter().map(|w| format!("\"{}\"", to_hex(w))).collect();
            let props: Vec<String> = n.council.proposals.iter().map(|p| format!(
                "{{\"id\":\"{}\",\"from\":\"{}\",\"to\":\"{}\",\"token\":\"{}\",\"amount\":{},\"approvals\":{},\"status\":\"{}\"}}",
                p.id, to_hex(&p.from), to_hex(&p.to), to_hex(&p.token), p.amount, p.approvals.len(), p.status)).collect();
            ok(format!("{{\"ok\":true,\"threshold\":{},\"members\":[{}],\"proposals\":[{}]}}",
                n.council.threshold, members.join(","), props.join(",")))
        }
        ("POST", "/bank/propose_transfer") => {
            let proposer = match jstr(body, "proposer").and_then(hex32) { Some(a) => a, None => return bad("proposer must be 64-hex (a council member)") };
            let to = match jstr(body, "to").and_then(hex32) { Some(a) => a, None => return bad("to must be 64-hex") };
            let from = jstr(body, "from").and_then(hex32).unwrap_or(MASTER); // treasury = master by default
            let amount = match jnum(body, "amount") { Some(a) if a > 0 => a, _ => return bad("amount required (>0)") };
            let tok_s = jstr(body, "token").unwrap_or("SIGIL").to_string();
            let mut n = node.write().unwrap();
            // resolve an EXISTING symbol (SIGIL→NATIVE, USDS, …) via the registry, then raw hex,
            // then a derived id (deploy convention) — NOT token_id_for first, which would mint a
            // brand-new id for "SIGIL" that no wallet holds.
            let token = hex32(&tok_s)
                .or_else(|| n.tokens.iter().find(|(s, _)| s.eq_ignore_ascii_case(&tok_s)).map(|(_, id)| *id))
                .unwrap_or_else(|| token_id_for(&tok_s));
            if !n.council.is_member(&proposer) { return bad("proposer is not a council member"); }
            if let Err(e) = authorize(&mut n, &proposer, "bank_propose",
                &[to_hex(&from), to_hex(&to), to_hex(&token), amount.to_string()], body) { return bad(&e); }
            let now = now_ms() / 1000;
            let id = format!("pr-{}", &to_hex(blake3::hash(format!("{}|{}|{}", to_hex(&to), amount, now).as_bytes()).as_bytes())[..12]);
            match n.council.propose(id, from, to, token, amount, proposer, now) {
                Ok(p) => { let id = p.id.clone(); let ap = p.approvals.len(); let th = n.council.threshold; persist(&n);
                    ok(format!("{{\"ok\":true,\"id\":\"{}\",\"approvals\":{},\"threshold\":{},\"status\":\"pending\"}}", id, ap, th)) }
                Err(e) => bad(&e),
            }
        }
        ("POST", "/bank/approve") => {
            let id = jstr(body, "id").unwrap_or("").to_string();
            let approver = match jstr(body, "approver").and_then(hex32) { Some(a) => a, None => return bad("approver must be 64-hex (a council member)") };
            let mut n = node.write().unwrap();
            if !n.council.is_member(&approver) { return bad("approver is not a council member"); }
            if let Err(e) = authorize(&mut n, &approver, "bank_approve", &[id.clone(), to_hex(&approver)], body) { return bad(&e); }
            let ready = match n.council.approve(&id, approver) { Ok(r) => r, Err(e) => return bad(&e) };
            if !ready {
                let cnt = n.council.get(&id).map(|p| p.approvals.len()).unwrap_or(0);
                let th = n.council.threshold;
                persist(&n);
                return ok(format!("{{\"ok\":true,\"id\":\"{}\",\"approvals\":{},\"threshold\":{},\"status\":\"pending\"}}", id, cnt, th));
            }
            // 2-of-2 reached → execute the transfer through the state chokepoint
            let p = n.council.get(&id).cloned().unwrap();
            let from_bal = n.state.balance_of(&p.from, &p.token);
            if from_bal < p.amount { return bad("insufficient treasury balance for the approved transfer"); }
            let to_bal = n.state.balance_of(&p.to, &p.token);
            let muts = vec![
                StateMutation::SetBalance { wallet: p.from, token: p.token, amount: from_bal - p.amount },
                StateMutation::SetBalance { wallet: p.to,   token: p.token, amount: to_bal + p.amount },
            ];
            let h = n.height;
            if let Err(e) = commit_state_transition(&mut n.state, &StateTransition { at_height: h, mutations: muts }, h) {
                return bad(&format!("transfer commit failed: {e:?}"));
            }
            n.height += 1;
            n.council.mark_executed(&id);
            persist(&n);
            ok(format!("{{\"ok\":true,\"id\":\"{}\",\"status\":\"executed\",\"amount\":{},\"to\":\"{}\"}}", id, p.amount, to_hex(&p.to)))
        }
        _ => bad("unknown route"),
    }
}

fn query_get<'a>(query: &'a str, key: &str) -> Option<&'a str> {
    for kv in query.split('&') {
        let mut it = kv.splitn(2, '=');
        if it.next() == Some(key) { return it.next(); }
    }
    None
}

fn handle(mut stream: TcpStream, node: &RwLock<Node>) {
    // Read until headers are complete, then until the full Content-Length body
    // has arrived (a single read() can miss a body in a later TCP segment).
    let mut data: Vec<u8> = Vec::with_capacity(2048);
    let mut tmp = [0u8; 8192];
    let mut header_end;
    loop {
        match stream.read(&mut tmp) {
            Ok(0) => return,
            Ok(k) => data.extend_from_slice(&tmp[..k]),
            Err(_) => return,
        }
        if let Some(pos) = find_sub(&data, b"\r\n\r\n") { header_end = pos + 4; break; }
        if data.len() > 1 << 20 { return; }
    }
    let head = String::from_utf8_lossy(&data[..header_end]).to_string();
    let content_len: usize = head
        .lines()
        .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse().unwrap_or(0)))
        .unwrap_or(0);
    while data.len() < header_end + content_len {
        match stream.read(&mut tmp) {
            Ok(0) => break,
            Ok(k) => data.extend_from_slice(&tmp[..k]),
            Err(_) => break,
        }
    }
    let first = head.lines().next().unwrap_or("");
    let mut parts = first.split_whitespace();
    let method = parts.next().unwrap_or("");
    let full_path = parts.next().unwrap_or("/");
    let (path, query) = match full_path.split_once('?') { Some((p, q)) => (p, q), None => (full_path, "") };
    let body = String::from_utf8_lossy(&data[header_end..]).to_string();
    // Client IP for rate-limiting. We sit behind q-flux, so prefer the FIRST hop
    // in X-Forwarded-For (the real client); fall back to the socket peer for a
    // direct connection. Header match is case-insensitive (q-flux/proxies vary).
    let peer_ip = head
        .lines()
        .find_map(|l| {
            let ll = l.to_ascii_lowercase();
            ll.strip_prefix("x-forwarded-for:").map(|v| {
                v.trim().split(',').next().unwrap_or("").trim().to_string()
            })
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| stream.peer_addr().map(|a| a.ip().to_string()).unwrap_or_else(|_| "unknown".into()));
    let resp = route(node, method, path, query, &body, &peer_ip);
    // Persist the money+chain snapshot after a SUCCESSFUL mutating request so a restart
    // restores balances/pools/height/tip instead of re-seeding genesis. Gating on the
    // 200 response (C4 / audit DoS amplifier): a POST that fails validation, auth, or the
    // /onboard rate-limit returns 400 and must NOT trigger a full-state serialize+flux-db
    // write — otherwise anonymous invalid-POST spam forces an O(state) write per request.
    if method == "POST" && resp.starts_with("HTTP/1.1 200") {
        if let Ok(n) = node.read() { persist(&n); }
    }
    let _ = stream.write_all(resp.as_bytes());
}

fn find_sub(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn main() {
    let addr = std::env::var("SIGIL_RPC_ADDR").unwrap_or_else(|_| "127.0.0.1:8099".into());
    let node = Arc::new(RwLock::new(bootstrap()));
    let listener = TcpListener::bind(&addr).expect("bind");
    eprintln!("sigil-rpcd listening on {addr} — thread-per-conn + RwLock (concurrent reads, serialized writes); pool USDS/wQUG, trader+operator funded");

    // Background full-text index build: the port is already bound and serving the
    // money/chain routes. We build the explorer search index off the hot path so a
    // restart never blocks on re-tokenizing the whole history. The build reads under
    // a SHARED read lock (balance/status readers stay responsive — only writers wait
    // the few seconds it takes), then swaps the finished index in under a brief
    // exclusive lock. `/readyz` flips true when search is live.
    {
        let inode = Arc::clone(&node);
        thread::spawn(move || {
            let t0 = std::time::Instant::now();
            let built = {
                let n = inode.read().unwrap();
                n.history.as_ref().map(|h| h.build_detached_index())
            };
            match built {
                Some(Ok(engine)) => {
                    let mut n = inode.write().unwrap();
                    if let Some(h) = n.history.as_mut() { h.install_index(engine); }
                    drop(n);
                    INDEX_READY.store(true, Ordering::Relaxed);
                    eprintln!("flux-history: search index built in {:?} — /readyz ✓", t0.elapsed());
                }
                Some(Err(e)) => eprintln!("flux-history: background index build failed ({e}) — search disabled"),
                None => INDEX_READY.store(true, Ordering::Relaxed), // no history store; nothing to build
            }
        });
    }
    // Follower mode: if SIGIL_FOLLOW_PEER is set, sync the mining chain from that peer
    // (pull + independently verify + apply) so this node converges with the producer.
    if let Ok(peer) = std::env::var("SIGIL_FOLLOW_PEER") {
        eprintln!("follower mode: syncing the mining chain from peer {peer}");
        let fnode = Arc::clone(&node);
        thread::spawn(move || follow_peer(fnode, peer));
    }
    // Status writer: keep $SIGIL_STATUS_OUT fresh so the frontend (sigilgraph)
    // reflects the LIVE chain. Without this the file froze (was stale since the
    // writer was dropped from rpcd) and the site showed a stuck height forever.
    // Atomic tmp+rename so the frontend never reads a half-written file.
    if let Ok(out) = std::env::var("SIGIL_STATUS_OUT") {
        eprintln!("status writer: refreshing {out} every 2s");
        let snode = Arc::clone(&node);
        thread::spawn(move || loop {
            {
                let n = snode.read().unwrap();
                let json = format!(
                    "{{\"status\":{{\"height\":{h},\"network_id\":\"sigil-g0\",\"peers\":1,\"supply\":\"{s}\",\"max_supply\":\"21000000\"}},\"tip\":{{\"height\":{h},\"hash\":\"{tip}\",\"roots\":{{}}}},\"blocks\":[]}}",
                    h = n.block_height,
                    s = n.state.native_supply(),
                    tip = to_hex(&n.tip_hash),
                );
                drop(n);
                let tmp = format!("{out}.tmp");
                if std::fs::write(&tmp, json.as_bytes()).is_ok() {
                    let _ = std::fs::rename(&tmp, &out);
                }
            }
            thread::sleep(std::time::Duration::from_secs(2));
        });
    }
    for stream in listener.incoming().flatten() {
        let n = Arc::clone(&node);
        thread::spawn(move || handle(stream, &n));
    }
}
