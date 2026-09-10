//! court.rs — **the SIGIL Nation Supreme Court, live on the node**.
//!
//! Exposes `sigil-court` over HTTP: the constitution, the docket, the bench, precedent, and the one
//! act that matters to a user — **exporting a sealed disclosure packet and getting a link they can
//! hand to their accountant, their bank, or a tax office.**
//!
//! ## The share link is a bearer capability, and is treated as one
//!
//! `GET /v1/court/packet/{root}` returns a citizen's records to whoever holds the URL. That is the
//! point (an authority must be able to fetch it without an account) and it is also the risk, so
//! three limits are enforced on every fetch, not just at export:
//!
//! * **It expires.** \art{XI} — past the order's `expires_ts` the route 410s. A link mailed to an
//!   accountant stops working when the order does.
//! * **It is revocable.** `POST /v1/court/disclosure/revoke` kills the link immediately; the docket
//!   records why.
//! * **It is unguessable.** The address is the packet root: 32 bytes of BLAKE3 over the records.
//!
//! What it is NOT: access-controlled. Anyone with the URL sees the packet. Sharing the link is
//! exactly as consequential as sending the file, and the UI says so in those words rather than
//! implying a permission system that does not exist.
//!
//! ## What is real here and what is not — read this before believing a packet
//!
//! A disclosure is only as good as the blocks it is built from. This module keeps a **bounded
//! archive of recent blocks fed by the producer** ([`CourtBridge::record_block`]). Three
//! consequences, each surfaced in the API rather than left to be discovered:
//!
//! 1. **Coverage is finite.** Every response reports `archive_from`/`archive_to`. An export whose
//!    range falls outside is REFUSED, naming the covered range — never silently emptied. A packet
//!    with zero records looks identical to a citizen with no activity, which is precisely the
//!    confusion that would make the whole thing untrustworthy.
//! 2. **The archive is in memory.** A node restart empties it, and with it the court's docket. This
//!    is a live surface, not durable consensus state; `court_root` is computed and served but is
//!    **not yet committed into `contract_state_root`** — that changes what the producer emits and
//!    needs an explicit operator decision.
//! 3. **Blocks are trusted as the producer handed them over.** `record_block` recomputes the
//!    event-log root from the events and refuses a mismatch, so a corrupted feed cannot enter. It
//!    does not independently re-derive the root from a signed header.
//!
//! ## Who may order what
//!
//! * **Reads** — public. The constitution, the docket, the bench and precedent are the public record
//!   of a public court; a court whose proceedings are secret is not one.
//! * **Self-disclosure** (`POST /v1/court/disclosure/self`) — a citizen exporting their OWN records.
//!   Authorised by an ed25519 signature over a bound challenge string, proving control of the very
//!   wallet being disclosed. This is the button in the wallet.
//! * **Third-party orders** (`POST /v1/court/disclosure/order`) — a tax office asking about someone
//!   else. Requires `SIGIL_COURT_ADMIN_TOKEN` as a bearer token. **Unset means the route refuses
//!   everything**, which is the correct default for a route that discloses other people's records.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::RwLock;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::Json;
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use sigil_court::{
    bank_scope, Verdict, constitution_hash_hex, disclosure, tax_scope, verify_packet, Article, BlockEvents,
    CourtEvent, DisclosurePacket, DisclosureRequest, Jurisdiction, Purpose, Rank,
    SupremeCourt, CONSTITUTION_VERSION,
};
use sigil_events::SigilEvent;
use sigil_state::WalletId;

use crate::{hex32, ApiResponse, AppState};

/// How many recent blocks the court keeps for export. ~2 h at 6 blk/s.
const DEFAULT_ARCHIVE_BLOCKS: usize = 50_000;
/// How many exported packets stay fetchable. Oldest evicted first.
const MAX_PACKETS: usize = 512;
/// Default life of a self-disclosure link.
const SELF_LINK_TTL_SECS: u64 = 7 * 24 * 3600;

fn now_s() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

struct StoredPacket {
    packet: DisclosurePacket,
    created_ts: u64,
    seq: u64,
}

/// The node's live court.
pub struct CourtBridge {
    court: RwLock<SupremeCourt>,
    archive: RwLock<BTreeMap<u64, BlockEvents>>,
    packets: RwLock<HashMap<[u8; 32], StoredPacket>>,
    seq: RwLock<u64>,
    max_blocks: usize,
    /// Public base for share links, e.g. `https://sigilgraph.org`. SIGIL's own home — never
    /// quillon.xyz, which is the other project's surface entirely.
    public_base: String,
    /// Seats named by `SIGIL_COURT_BENCH`.
    named_seats: usize,
    /// Anonymous seats derived from the seal to make a panel quorate.
    derived_seats: usize,
}

/// Parse a `SIGIL_COURT_BENCH` spec (`<64-hex>:chief,<64-hex>:justice,…`) and seat it. Pure over
/// the spec so the exact string in the systemd drop-in can be tested without an environment.
/// Returns how many seats were taken. Malformed entries are logged and skipped, never fatal — a
/// typo in one seat must not leave the nation with no court at all.
pub fn seat_from_spec(court: &mut SupremeCourt, spec: &str) -> usize {
    let mut n = 0;
    for (i, entry) in spec.split(',').map(|e| e.trim()).filter(|e| !e.is_empty()).enumerate() {
        let (w_hex, rank_s) = match entry.split_once(':') {
            Some((a, b)) => (a.trim(), b.trim()),
            None => (entry, "justice"),
        };
        let Some(w) = hex32(w_hex) else {
            eprintln!("⚠ SIGIL_COURT_BENCH entry {i} is not a 64-hex wallet, skipped: {w_hex:?}");
            continue;
        };
        let rank = match rank_s.to_ascii_lowercase().as_str() {
            "chief" | "chiefjustice" | "chief_justice" => Rank::ChiefJustice,
            "justice" => Rank::Justice,
            "magistrate" => Rank::Magistrate,
            "advocate" => Rank::Advocate,
            "clerk" => Rank::Clerk,
            other => {
                eprintln!("⚠ SIGIL_COURT_BENCH entry {i} has unknown rank {other:?}, seating as justice");
                Rank::Justice
            }
        };
        match court.appoint(w, rank, 0) {
            Ok(()) => n += 1,
            Err(e) => eprintln!("⚠ SIGIL_COURT_BENCH could not seat {w_hex}: {e}"),
        }
    }
    n
}

/// Add anonymous seats derived from the seal until three members may sit. Returns how many.
pub fn top_up_bench(court: &mut SupremeCourt, seed: &[u8; 32]) -> usize {
    let mut derived = 0;
    let mut i = 0u8;
    while court.bench().at_least(Rank::Magistrate).len() < 3 {
        let w = *blake3::hash(&[b"sigil-court/seat".as_slice(), &[i], seed].concat()).as_bytes();
        if court.appoint(w, Rank::Magistrate, 0).is_ok() { derived += 1; }
        i += 1;
        if i > 16 { break; }
    }
    derived
}

/// Parse a `SIGIL_COURT_HONOURS` spec and confer each honour through the nation's own
/// [`sigil_events::HonorPolicy`]. Returns how many were actually conferred — an honour whose
/// quorum cannot be met is REFUSED and logged, never silently downgraded.
pub fn confer_from_spec(court: &mut SupremeCourt, spec: &str) -> usize {
    let mut n = 0;
    for entry in spec.split(',').map(|e| e.trim()).filter(|e| !e.is_empty()) {
        let parts: Vec<&str> = entry.split(':').collect();
        let Some(w) = parts.first().and_then(|h| hex32(h)) else {
            eprintln!("⚠ SIGIL_COURT_HONOURS: {entry:?} does not start with a 64-hex wallet");
            continue;
        };
        let kind = parts.get(1).copied().unwrap_or("knight").to_ascii_lowercase();
        // Distinct authorities available to approve: seated Justices other than the honoree.
        let approvals = court.bench().at_least(Rank::Justice).into_iter().filter(|x| *x != w).count();
        let (order, rank, citation) = if kind.starts_with("ele") || kind.starts_with("ære") || kind.starts_with("aere") {
            (sigil_court::Order::Elefantordenen, String::new(), parts.get(2..).map(|r| r.join(":")).unwrap_or_default())
        } else {
            (sigil_court::Order::Ridderkorset,
             parts.get(2).copied().unwrap_or("Ridder").to_string(),
             parts.get(3..).map(|r| r.join(":")).unwrap_or_default())
        };
        let citation = if citation.trim().is_empty() { "conferred at the founding of the court".to_string() } else { citation };
        match court.confer_honour(order, rank, w, citation, [0u8; 32], approvals, true, 0) {
            Ok(_) => n += 1,
            Err(e) => eprintln!("⚠ SIGIL_COURT_HONOURS refused for {}: {e}", hex::encode(w)),
        }
    }
    n
}

impl Default for CourtBridge {
    fn default() -> Self {
        Self::new()
    }
}

impl CourtBridge {
    /// Seat a court from `SIGIL_COURT_SEAL_SEED` (64 hex) or, failing that, a seed derived from the
    /// node's own network id. A derived seal is deterministic and therefore **reproducible by
    /// anyone**, so it authenticates nothing; the status route reports `seal: "derived"` so a
    /// verifier is never misled into trusting it. Set the env var, chmod 600, for a real seal.
    pub fn new() -> Self {
        // Prefer a seed held in a chmod-600 FILE over one in the environment: a systemd
        // `Environment=` line is readable via `systemctl cat` and `/proc/<pid>/environ`, so the
        // root of trust for every disclosure this court ever seals would sit in process metadata.
        // The file path may be in the environment; the secret should not be.
        let from_file = std::env::var("SIGIL_COURT_SEAL_SEED_FILE").ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|t| hex32(t.trim()));
        let (seed, derived) = match from_file.or_else(|| std::env::var("SIGIL_COURT_SEAL_SEED").ok().and_then(|s| hex32(&s))) {
            Some(s) => (s, false),
            None => (*blake3::hash(b"sigil-court/derived-seal/not-authoritative").as_bytes(), true),
        };
        let mut court = SupremeCourt::from_seed(&seed);

        // ── Seating the founding bench ───────────────────────────────────────────────────────
        //
        // A constitution has to come from somewhere. `Bench::appoint` is the CONSTITUENT act — the
        // founding bench is seated by the power that founds it — and from that moment Art. VIII
        // governs every ELEVATION: no rank is granted solo, ever, and the exhaustive vote search
        // in `sigil_court::science::solo_search` proves it over the whole space. Seating is not a
        // loophole in that rule; it is what the rule presupposes. Every seat lands on the docket
        // as a `JusticeAppointed`, so who was seated by fiat and who climbed is legible forever.
        //
        // `SIGIL_COURT_BENCH` names the bench: `<64-hex>:chief,<64-hex>:justice,<64-hex>:magistrate`.
        // Unset, the court falls back to seats derived from the seal — reproducible by anyone,
        // therefore anonymous, and reported as such by `/v1/court/status`.
        let configured = std::env::var("SIGIL_COURT_BENCH").ok().filter(|s| !s.trim().is_empty());
        let seated_named = seat_from_spec(&mut court, configured.as_deref().unwrap_or(""));

        // A panel needs three who MAY SIT (Magistrate and up). If the named bench is short of that,
        // top it up with derived seats rather than shipping a court that refuses every order — and
        // say so in the status, because a bench padded with anonymous seats is a weaker bench.
        let derived_seats = top_up_bench(&mut court, &seed);
        if derived {
            court.record_contempt([0u8; 32], [0u8; 32], "seal is derived, not operator-provided: this court's signature authenticates nothing", 0);
        }

        // ── Founding honours ─────────────────────────────────────────────────────────────────
        //
        // `SIGIL_COURT_HONOURS` = `<64-hex>:elephant:<citation>` or `<64-hex>:knight:<rank>:<citation>`,
        // comma-separated. The gate is NOT this loop — it is `sigil_events::HonorPolicy`, reached
        // through `SupremeCourt::confer_honour`, which refuses the Elephant unless the operator
        // co-signs AND at least three distinct authorities approve. Setting the variable IS the
        // operator's co-signature; the approvals are the sitting Justices, counted here rather
        // than asserted. If the bench is too small the conferral is REFUSED and logged, because an
        // honour that bypassed its own quorum would be worth nothing.
        let honours = confer_from_spec(&mut court, &std::env::var("SIGIL_COURT_HONOURS").unwrap_or_default());
        let _ = honours;
        let max_blocks = std::env::var("SIGIL_COURT_ARCHIVE_BLOCKS").ok().and_then(|s| s.parse().ok()).unwrap_or(DEFAULT_ARCHIVE_BLOCKS);
        let public_base = std::env::var("SIGIL_COURT_PUBLIC_BASE").unwrap_or_else(|_| "https://sigilgraph.org".into());
        Self {
            court: RwLock::new(court),
            archive: RwLock::new(BTreeMap::new()),
            packets: RwLock::new(HashMap::new()),
            seq: RwLock::new(0),
            max_blocks,
            public_base,
            named_seats: seated_named,
            derived_seats,
        }
    }

    fn derived_seal(&self) -> bool {
        let file_ok = std::env::var("SIGIL_COURT_SEAL_SEED_FILE").ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|t| hex32(t.trim()))
            .is_some();
        !file_ok && std::env::var("SIGIL_COURT_SEAL_SEED").ok().and_then(|s| hex32(&s)).is_none()
    }

    /// Feed a sealed block into the court's archive. Called from the producer loop ONLY.
    ///
    /// Refuses a block whose supplied `event_log_root` does not match its own events — a corrupt
    /// feed must not become evidence. Returns whether it was accepted.
    pub fn record_block(&self, height: u64, event_log_root: [u8; 32], events: Vec<SigilEvent>) -> bool {
        let b = BlockEvents::seal(height, events);
        if b.event_log_root != event_log_root {
            return false;
        }
        let Ok(mut a) = self.archive.write() else { return false };
        a.insert(height, b);
        while a.len() > self.max_blocks {
            let Some(&first) = a.keys().next() else { break };
            a.remove(&first);
        }
        true
    }

    /// Inclusive height range currently held, or `None` when empty.
    pub fn coverage(&self) -> Option<(u64, u64)> {
        let a = self.archive.read().ok()?;
        Some((*a.keys().next()?, *a.keys().next_back()?))
    }

    fn blocks_in(&self, from: u64, to: u64) -> Vec<BlockEvents> {
        self.archive.read().map(|a| a.range(from..=to).map(|(_, b)| b.clone()).collect()).unwrap_or_default()
    }

    fn store(&self, packet: DisclosurePacket) -> [u8; 32] {
        let root = packet.seal.body.packet_root;
        let mut seq = self.seq.write().map(|mut s| { *s += 1; *s }).unwrap_or(0);
        if seq == 0 { seq = now_s(); }
        if let Ok(mut p) = self.packets.write() {
            p.insert(root, StoredPacket { packet, created_ts: now_s(), seq });
            while p.len() > MAX_PACKETS {
                if let Some(oldest) = p.values().min_by_key(|s| s.seq).map(|s| s.packet.seal.body.packet_root) {
                    p.remove(&oldest);
                } else {
                    break;
                }
            }
        }
        root
    }

    fn share_url(&self, root: &[u8; 32]) -> String {
        format!("{}/v1/court/packet/{}", self.public_base.trim_end_matches('/'), hex::encode(root))
    }
}

// ───────────────────────────── responses ─────────────────────────────

#[derive(Debug, Serialize)]
pub struct CourtStatus {
    pub constitution_version: u32,
    pub constitution_hash: String,
    pub articles: usize,
    pub court_root: String,
    pub docket_entries: usize,
    pub docket_head: String,
    pub docket_root: String,
    pub bench_size: usize,
    pub live_precedents: usize,
    pub open_cases: usize,
    pub court_pubkey: String,
    /// `"operator"` when a real seal seed is configured, `"derived"` when it is not — a derived
    /// seal is reproducible by anyone and authenticates nothing.
    pub seal: &'static str,
    pub archive_from: Option<u64>,
    pub archive_to: Option<u64>,
    pub archive_blocks: usize,
    pub archive_capacity: usize,
    pub packets_held: usize,
    /// Seats named by the operator via `SIGIL_COURT_BENCH`.
    pub named_seats: usize,
    /// Anonymous seats derived from the seal to reach a quorate panel. A bench padded with these
    /// is weaker than one that is fully named, and says so rather than hiding it.
    pub derived_seats: usize,
    /// Wallets bearing an Order of the nation (æresborgere).
    pub honoured: Vec<String>,
    /// `court_root` is served but NOT yet written into `contract_state_root`.
    pub committed_on_chain: bool,
    pub notes: Vec<&'static str>,
}

pub async fn court_status(State(s): State<AppState>) -> Json<ApiResponse<CourtStatus>> {
    let c = match s.court.court.read() { Ok(c) => c, Err(_) => return ApiResponse::err("court lock poisoned") };
    let (from, to) = match s.court.coverage() { Some((a, b)) => (Some(a), Some(b)), None => (None, None) };
    let archive_blocks = s.court.archive.read().map(|a| a.len()).unwrap_or(0);
    ApiResponse::ok(CourtStatus {
        constitution_version: CONSTITUTION_VERSION,
        constitution_hash: constitution_hash_hex(),
        articles: Article::ALL.len(),
        court_root: c.court_root_hex(),
        docket_entries: c.docket().len(),
        docket_head: hex::encode(c.docket().head()),
        docket_root: hex::encode(c.docket().root()),
        bench_size: c.bench().len(),
        live_precedents: c.register().live_precedents().len(),
        open_cases: c.register().cases().filter(|x| !matches!(x.status, sigil_court::CaseStatus::Final)).count(),
        court_pubkey: hex::encode(c.public_key()),
        seal: if s.court.derived_seal() { "derived" } else { "operator" },
        archive_from: from,
        archive_to: to,
        archive_blocks,
        archive_capacity: s.court.max_blocks,
        packets_held: s.court.packets.read().map(|p| p.len()).unwrap_or(0),
        named_seats: s.court.named_seats,
        derived_seats: s.court.derived_seats,
        honoured: c.bench().members().filter(|j| j.has(sigil_court::Credential::Aeresborger)).map(|j| hex::encode(j.wallet)).collect(),
        committed_on_chain: false,
        notes: vec![
            "court_root is computed and served but NOT yet committed into contract_state_root; that changes what the producer emits and needs an operator decision.",
            "The docket and the block archive are in memory: a node restart empties both.",
            "A share link is a bearer capability. Anyone holding the URL can read the packet until it expires or is revoked.",
        ],
    })
}

#[derive(Debug, Serialize)]
pub struct ArticleView { pub numeral: &'static str, pub name: String, pub text: &'static str }

#[derive(Debug, Serialize)]
pub struct ConstitutionView { pub version: u32, pub hash: String, pub articles: Vec<ArticleView> }

pub async fn court_constitution() -> Json<ApiResponse<ConstitutionView>> {
    ApiResponse::ok(ConstitutionView {
        version: CONSTITUTION_VERSION,
        hash: constitution_hash_hex(),
        articles: Article::ALL.iter().map(|a| ArticleView { numeral: a.numeral(), name: format!("{a:?}"), text: a.text() }).collect(),
    })
}

#[derive(Debug, Deserialize)]
pub struct DocketQuery {
    pub limit: Option<usize>,
    pub wallet: Option<String>,
    pub case: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct DocketEntryView {
    pub seq: u64,
    pub height: u64,
    pub kind: &'static str,
    pub leaf: String,
    pub event: CourtEvent,
}

#[derive(Debug, Serialize)]
pub struct DocketView {
    pub total: usize,
    pub head: String,
    pub root: String,
    pub chain_verified: bool,
    pub histogram: BTreeMap<String, u32>,
    pub entries: Vec<DocketEntryView>,
}

pub async fn court_docket(State(s): State<AppState>, Query(q): Query<DocketQuery>) -> Json<ApiResponse<DocketView>> {
    let c = match s.court.court.read() { Ok(c) => c, Err(_) => return ApiResponse::err("court lock poisoned") };
    let d = c.docket();
    let limit = q.limit.unwrap_or(100).min(1000);
    let picked: Vec<&sigil_court::DocketEntry> = if let Some(w) = q.wallet.as_deref().and_then(hex32) {
        d.by_wallet(&w)
    } else if let Some(cs) = q.case.as_deref().and_then(hex32) {
        d.by_case(&cs)
    } else {
        d.entries().iter().collect()
    };
    let entries = picked.into_iter().rev().take(limit).map(|e| DocketEntryView {
        seq: e.seq, height: e.height, kind: e.event.name(), leaf: hex::encode(e.leaf), event: e.event.clone(),
    }).collect();
    ApiResponse::ok(DocketView {
        total: d.len(),
        head: hex::encode(d.head()),
        root: hex::encode(d.root()),
        chain_verified: d.verify_chain().is_ok(),
        histogram: d.histogram().into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
        entries,
    })
}

#[derive(Debug, Serialize)]
pub struct JusticeView {
    pub wallet: String,
    pub rank: &'static str,
    pub may_sit: bool,
    pub credentials: Vec<String>,
    pub panels_sat: u32,
    pub upheld: u32,
    pub reversed: u32,
    pub upheld_bps: u32,
}

#[derive(Debug, Serialize)]
pub struct BenchView { pub size: usize, pub chief: Option<String>, pub members: Vec<JusticeView>, pub bench_root: String }

pub async fn court_bench(State(s): State<AppState>) -> Json<ApiResponse<BenchView>> {
    let c = match s.court.court.read() { Ok(c) => c, Err(_) => return ApiResponse::err("court lock poisoned") };
    let b = c.bench();
    ApiResponse::ok(BenchView {
        size: b.len(),
        chief: b.chief().map(hex::encode),
        bench_root: hex::encode(b.root()),
        members: b.members().map(|j| JusticeView {
            wallet: hex::encode(j.wallet),
            rank: j.rank.name(),
            may_sit: j.rank.may_sit(),
            credentials: j.credentials.iter().map(|c| format!("{c:?}")).collect(),
            panels_sat: j.panels_sat,
            upheld: j.upheld,
            reversed: j.reversed,
            upheld_bps: j.upheld_bps(),
        }).collect(),
    })
}

#[derive(Debug, Serialize)]
pub struct PrecedentView { pub ruling: String, pub article: String, pub holding: String, pub set_height: u64, pub live: bool, pub overruled_by: Option<String> }

pub async fn court_precedents(State(s): State<AppState>) -> Json<ApiResponse<Vec<PrecedentView>>> {
    let c = match s.court.court.read() { Ok(c) => c, Err(_) => return ApiResponse::err("court lock poisoned") };
    ApiResponse::ok(c.register().precedents().map(|p| PrecedentView {
        ruling: hex::encode(p.ruling),
        article: p.article.numeral().to_string(),
        holding: p.holding.clone(),
        set_height: p.set_height,
        live: p.is_live(),
        overruled_by: p.overruled_by.map(hex::encode),
    }).collect())
}

// ───────────────────────────── disclosure ─────────────────────────────

/// The exact string a citizen signs to prove the wallet is theirs. Bound to the wallet, the range
/// and a caller-supplied nonce, so a signature captured for one export cannot authorise another.
pub fn self_challenge(wallet: &WalletId, from: u64, to: u64, nonce: u64) -> String {
    format!("sigil-court/self-disclosure/v1\nwallet:{}\nfrom:{}\nto:{}\nnonce:{}", hex::encode(wallet), from, to, nonce)
}

#[derive(Debug, Deserialize)]
pub struct SelfDisclosureReq {
    pub wallet: String,
    pub from_height: Option<u64>,
    pub to_height: Option<u64>,
    pub nonce: u64,
    /// ed25519 over [`self_challenge`], 128 hex.
    pub signature: String,
    /// `"tax"` (default) or `"full"`. `full` is an AML-shaped disclosure of one's own records —
    /// counterparties in clear. A citizen may waive their own privacy; they may not waive
    /// anyone else's, so this is still refused for a third-party subject.
    pub detail: Option<String>,
    pub jurisdiction: Option<String>,
    pub authority: Option<String>,
    pub ttl_secs: Option<u64>,
}

#[derive(Debug, Serialize)]
pub struct PacketLink {
    pub packet_root: String,
    pub order_id: String,
    pub share_url: String,
    pub csv_url: String,
    pub verify_url: String,
    pub records: usize,
    pub redacted_fields: u32,
    pub counterparties_in_clear: bool,
    pub expires_ts: u64,
    pub expires_in_secs: u64,
    pub from_height: u64,
    pub to_height: u64,
    pub purpose: String,
    pub court_pubkey: String,
    pub constitution_hash: String,
    pub warning: &'static str,
}

fn verify_wallet_sig(wallet: &WalletId, msg: &str, sig_hex: &str) -> bool {
    let sig_hex = sig_hex.strip_prefix("0x").unwrap_or(sig_hex);
    let Ok(raw) = hex::decode(sig_hex) else { return false };
    let Ok(arr): Result<[u8; 64], _> = raw.try_into() else { return false };
    let Ok(vk) = VerifyingKey::from_bytes(wallet) else { return false };
    vk.verify(msg.as_bytes(), &Signature::from_bytes(&arr)).is_ok()
}

fn build_link(b: &CourtBridge, p: &DisclosurePacket, reveal: bool) -> PacketLink {
    let root = p.seal.body.packet_root;
    let base = b.public_base.trim_end_matches('/');
    let h = hex::encode(root);
    PacketLink {
        packet_root: h.clone(),
        order_id: hex::encode(p.order.id),
        share_url: b.share_url(&root),
        csv_url: format!("{base}/v1/court/packet/{h}/csv"),
        verify_url: format!("{base}/v1/court/packet/{h}/verify"),
        records: p.records.len(),
        redacted_fields: p.summary.redacted_fields,
        counterparties_in_clear: reveal,
        expires_ts: p.order.expires_ts,
        expires_in_secs: p.order.expires_ts.saturating_sub(now_s()),
        from_height: p.order.request.scope.from_height,
        to_height: p.order.request.scope.to_height,
        purpose: format!("{:?}", p.order.request.purpose),
        court_pubkey: p.seal.court_pubkey_hex.clone(),
        constitution_hash: hex::encode(p.seal.body.constitution_hash),
        warning: "Anyone holding this link can read the packet until it expires or is revoked. Share it like the file itself, not like a password-protected page.",
    }
}

/// Issue and export in one step: a citizen discloses their own records and gets a link.
pub async fn court_self_disclosure(State(s): State<AppState>, Json(req): Json<SelfDisclosureReq>) -> Json<ApiResponse<PacketLink>> {
    let Some(wallet) = hex32(&req.wallet) else { return ApiResponse::err("wallet must be 64 hex") };
    let Some((cov_from, cov_to)) = s.court.coverage() else {
        return ApiResponse::err("the court holds no blocks yet: nothing can be disclosed. This node has not archived a block since it started.");
    };
    let from = req.from_height.unwrap_or(cov_from);
    let to = req.to_height.unwrap_or(cov_to);
    if to < from { return ApiResponse::err("to_height is below from_height") }
    if from < cov_from || to > cov_to {
        return ApiResponse::err(format!(
            "requested {from}..={to} but the court's archive covers {cov_from}..={cov_to}. Narrow the range; a packet outside coverage would be silently empty and is refused instead."
        ));
    }
    let challenge = self_challenge(&wallet, from, to, req.nonce);
    if !verify_wallet_sig(&wallet, &challenge, &req.signature) {
        return ApiResponse::err("signature does not verify for that wallet — a self-disclosure must be signed by the wallet being disclosed");
    }
    let reveal = req.detail.as_deref() == Some("full");
    let purpose = if reveal { Purpose::Aml } else { Purpose::Tax };
    let ttl = req.ttl_secs.unwrap_or(SELF_LINK_TTL_SECS).clamp(300, 90 * 24 * 3600);
    let ts = now_s();
    let request = DisclosureRequest {
        jurisdiction: Jurisdiction::new(
            req.jurisdiction.clone().unwrap_or_else(|| "SELF".into()),
            req.authority.clone().unwrap_or_else(|| "citizen".into()),
        ),
        purpose,
        scope: tax_scope(wallet, from, to),
        key_kind: None,
        reason: format!("self-disclosure requested and signed by the subject (nonce {})", req.nonce),
        requested_ts: ts,
        ttl_secs: ttl,
    };
    export_through_court(&s, request, None, reveal).await
}

#[derive(Debug, Deserialize)]
pub struct OrderReq {
    pub subject: Option<String>,
    pub from_height: u64,
    pub to_height: u64,
    /// `tax` | `aml` | `audit` | `bank`
    pub purpose: String,
    pub country: String,
    pub authority: String,
    pub reason: String,
    pub ttl_secs: Option<u64>,
}

/// A third-party order — a foreign authority asking about someone else. Bearer-token gated.
pub async fn court_order(State(s): State<AppState>, headers: HeaderMap, Json(req): Json<OrderReq>) -> Json<ApiResponse<PacketLink>> {
    let Ok(expected) = std::env::var("SIGIL_COURT_ADMIN_TOKEN") else {
        return ApiResponse::err("third-party disclosure is disabled on this node: SIGIL_COURT_ADMIN_TOKEN is unset, so the route refuses everything. A self-disclosure signed by the subject is always available at /v1/court/disclosure/self.");
    };
    let got = headers.get("authorization").and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer ")).unwrap_or("");
    if expected.is_empty() || got != expected {
        return ApiResponse::err("bearer token required");
    }
    let Some((cov_from, cov_to)) = s.court.coverage() else { return ApiResponse::err("the court holds no blocks yet") };
    if req.from_height < cov_from || req.to_height > cov_to || req.to_height < req.from_height {
        return ApiResponse::err(format!("requested {}..={} but the archive covers {cov_from}..={cov_to}", req.from_height, req.to_height));
    }
    let (purpose, scope, reveal) = match req.purpose.to_lowercase().as_str() {
        "tax" => {
            let Some(w) = req.subject.as_deref().and_then(hex32) else { return ApiResponse::err("a tax order needs a 64-hex subject") };
            (Purpose::Tax, tax_scope(w, req.from_height, req.to_height), false)
        }
        "aml" => {
            let Some(w) = req.subject.as_deref().and_then(hex32) else { return ApiResponse::err("an aml order needs a 64-hex subject") };
            (Purpose::Aml, tax_scope(w, req.from_height, req.to_height), true)
        }
        "audit" => {
            let Some(w) = req.subject.as_deref().and_then(hex32) else { return ApiResponse::err("an audit order needs a 64-hex subject") };
            (Purpose::Audit, tax_scope(w, req.from_height, req.to_height), false)
        }
        "bank" => (Purpose::Audit, bank_scope(req.from_height, req.to_height), false),
        other => return ApiResponse::err(format!("unknown purpose {other:?}: use tax | aml | audit | bank")),
    };
    let request = DisclosureRequest {
        jurisdiction: Jurisdiction::new(req.country.clone(), req.authority.clone()),
        purpose,
        scope,
        key_kind: None,
        reason: req.reason.clone(),
        requested_ts: now_s(),
        ttl_secs: req.ttl_secs.unwrap_or(30 * 24 * 3600).clamp(300, 365 * 24 * 3600),
    };
    export_through_court(&s, request, None, reveal).await
}

/// Order + export through the real court path. The SQIsign seal takes seconds, so the whole
/// operation runs on a blocking thread rather than stalling the async runtime.
async fn export_through_court(s: &AppState, request: DisclosureRequest, case: Option<[u8; 32]>, reveal: bool) -> Json<ApiResponse<PacketLink>> {
    let (from, to) = (request.scope.from_height, request.scope.to_height);
    let blocks = s.court.blocks_in(from, to);
    let court = s.court.clone();
    let height = to;
    let res = tokio::task::spawn_blocking(move || -> Result<DisclosurePacket, String> {
        let mut c = court.court.write().map_err(|_| "court lock poisoned".to_string())?;
        let ts = now_s();
        let id = disclosure::order_id(&request, height, ts);
        let panel = c.bench().select_panel(&id, 3).map_err(|e| e.to_string())?;
        let votes: Vec<(WalletId, bool)> = panel.iter().map(|p| (*p, true)).collect();
        let order = c.order_disclosure(request, case, &votes, height, ts).map_err(|e| e.to_string())?;
        c.export_disclosure(order, &blocks, &[], height, ts).map_err(|e| e.to_string())
    })
    .await;
    match res {
        Ok(Ok(packet)) => {
            s.court.store(packet.clone());
            ApiResponse::ok(build_link(&s.court, &packet, reveal))
        }
        Ok(Err(e)) => ApiResponse::err(e),
        Err(e) => ApiResponse::err(format!("export task failed: {e}")),
    }
}

fn fetch_live(s: &AppState, root_hex: &str) -> Result<DisclosurePacket, String> {
    let Some(root) = hex32(root_hex) else { return Err("packet root must be 64 hex".into()) };
    let p = s.court.packets.read().map_err(|_| "packet store poisoned".to_string())?;
    let Some(sp) = p.get(&root) else { return Err("no such packet: it may have expired out of the store, or the node restarted".into()) };
    let now = now_s();
    if now >= sp.packet.order.expires_ts {
        return Err(format!("Art. XI: this disclosure expired at {} (now {}). The link is dead; a new order is required.", sp.packet.order.expires_ts, now));
    }
    let c = s.court.court.read().map_err(|_| "court lock poisoned".to_string())?;
    // A revocation is on the docket; the link must die the moment it lands.
    let revoked = c.docket().by_order(&sp.packet.order.id).iter().any(|e| matches!(e.event, CourtEvent::DisclosureRevoked { .. }));
    if revoked {
        return Err("this disclosure was revoked by the court; the link no longer serves".into());
    }
    let _ = sp.created_ts;
    Ok(sp.packet.clone())
}

/// **The share link.** Returns the sealed packet itself.
pub async fn court_packet(State(s): State<AppState>, Path(root): Path<String>) -> Json<ApiResponse<DisclosurePacket>> {
    match fetch_live(&s, &root) {
        Ok(p) => ApiResponse::ok(p),
        Err(e) => ApiResponse::err(e),
    }
}

/// The flat file a tax office actually opens.
pub async fn court_packet_csv(State(s): State<AppState>, Path(root): Path<String>) -> axum::response::Response {
    use axum::http::{header, StatusCode};
    use axum::response::IntoResponse;
    match fetch_live(&s, &root) {
        Ok(p) => (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, "text/csv; charset=utf-8".to_string()),
                (header::CONTENT_DISPOSITION, format!("attachment; filename=\"sigil-disclosure-{}.csv\"", &root[..16.min(root.len())])),
            ],
            p.tax_csv(),
        ).into_response(),
        Err(e) => (StatusCode::GONE, e).into_response(),
    }
}

#[derive(Debug, Serialize)]
pub struct VerifyView {
    pub valid: bool,
    pub error: Option<String>,
    pub records_proven: u32,
    pub fields_recomputed: u32,
    pub fields_attested: u32,
    pub heights_checked: usize,
    pub heights_unknown: Vec<u64>,
    pub court_pubkey: String,
    pub constitution_hash: String,
    pub seal: &'static str,
    pub caveat: &'static str,
}

/// Verify a packet against THIS node's own view of the block roots. A recipient who does not trust
/// this node should run the same check against roots they obtained independently — which is the
/// whole point of the inclusion proofs, and is why the roots used here are reported back.
pub async fn court_packet_verify(State(s): State<AppState>, Path(root): Path<String>) -> Json<ApiResponse<VerifyView>> {
    let p = match fetch_live(&s, &root) { Ok(p) => p, Err(e) => return ApiResponse::err(e) };
    let heights: BTreeSet<u64> = p.records.iter().map(|r| r.height).collect();
    let mut roots = BTreeMap::new();
    let mut unknown = Vec::new();
    {
        let a = match s.court.archive.read() { Ok(a) => a, Err(_) => return ApiResponse::err("archive lock poisoned") };
        for h in &heights {
            match a.get(h) {
                Some(b) => { roots.insert(*h, b.event_log_root); }
                None => unknown.push(*h),
            }
        }
    }
    let pubkey = { match s.court.court.read() { Ok(c) => c.public_key().to_vec(), Err(_) => return ApiResponse::err("court lock poisoned") } };
    let (valid, err, v) = match verify_packet(&p, &roots, &pubkey, now_s()) {
        Ok(v) => (true, None, Some(v)),
        Err(e) => (false, Some(e.to_string()), None),
    };
    ApiResponse::ok(VerifyView {
        valid,
        error: err,
        records_proven: v.as_ref().map(|x| x.records_proven).unwrap_or(0),
        fields_recomputed: v.as_ref().map(|x| x.fields_recomputed).unwrap_or(0),
        fields_attested: v.as_ref().map(|x| x.fields_attested).unwrap_or(0),
        heights_checked: roots.len(),
        heights_unknown: unknown,
        court_pubkey: hex::encode(&pubkey),
        constitution_hash: constitution_hash_hex(),
        seal: if s.court.derived_seal() { "derived" } else { "operator" },
        caveat: "These roots come from this node's own archive. An independent recipient should re-run the same check against block roots they obtained from a source they trust.",
    })
}

#[derive(Debug, Deserialize)]
pub struct RevokeReq { pub packet_root: String, pub reason: String, pub nonce: u64, pub wallet: String, pub signature: String }

/// Revoke a link. The subject of a self-disclosure can kill their own link by signing; an operator
/// with the admin token can kill any.
pub async fn court_revoke(State(s): State<AppState>, headers: HeaderMap, Json(req): Json<RevokeReq>) -> Json<ApiResponse<serde_json::Value>> {
    let Some(root) = hex32(&req.packet_root) else { return ApiResponse::err("packet root must be 64 hex") };
    let (order_id, subject) = {
        let p = match s.court.packets.read() { Ok(p) => p, Err(_) => return ApiResponse::err("packet store poisoned") };
        let Some(sp) = p.get(&root) else { return ApiResponse::err("no such packet") };
        (sp.packet.order.id, sp.packet.order.request.scope.subjects.iter().next().copied())
    };
    let admin = std::env::var("SIGIL_COURT_ADMIN_TOKEN").ok().filter(|t| !t.is_empty()).map(|t| {
        headers.get("authorization").and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer ")) == Some(t.as_str())
    }).unwrap_or(false);
    let by_subject = match (hex32(&req.wallet), subject) {
        (Some(w), Some(sub)) if w == sub => {
            let msg = format!("sigil-court/revoke/v1\npacket:{}\nnonce:{}", req.packet_root, req.nonce);
            verify_wallet_sig(&w, &msg, &req.signature)
        }
        _ => false,
    };
    if !admin && !by_subject {
        return ApiResponse::err("revocation requires the admin token, or a signature from the wallet the packet discloses");
    }
    let height = s.court.coverage().map(|(_, t)| t).unwrap_or(0);
    {
        let mut c = match s.court.court.write() { Ok(c) => c, Err(_) => return ApiResponse::err("court lock poisoned") };
        if let Err(e) = c.revoke_disclosure(order_id, req.reason.clone(), height) {
            return ApiResponse::err(e.to_string());
        }
    }
    if let Ok(mut p) = s.court.packets.write() { p.remove(&root); }
    ApiResponse::ok(serde_json::json!({ "revoked": req.packet_root, "order": hex::encode(order_id), "by": if admin { "operator" } else { "subject" } }))
}

// ───────────────────────────── sitting: file, hear, rule, appeal ─────────────────────────────
//
// These are the acts that make a seat mean something. Each is authorised by an ed25519 signature
// from the wallet performing it, over a challenge bound to the act and a nonce — the same shape as
// a self-disclosure. The court checks the CONSTITUTIONAL question (is this wallet a party? is it on
// the panel? does the verdict match the vote?); the signature only answers "is this really them".

fn act_challenge(kind: &str, wallet: &WalletId, subject: &str, nonce: u64) -> String {
    format!("sigil-court/{kind}/v1\nwallet:{}\nsubject:{subject}\nnonce:{nonce}", hex::encode(wallet))
}

#[derive(Debug, Deserialize)]
pub struct FileCaseReq {
    pub wallet: String,
    pub defendant: Option<String>,
    /// Roman numeral, e.g. `"IV"`, or the variant name.
    pub article: String,
    pub claim: String,
    pub nonce: u64,
    pub signature: String,
}

fn article_by_name(s: &str) -> Option<Article> {
    let t = s.trim().to_ascii_lowercase();
    Article::ALL.into_iter().find(|a| {
        a.numeral().eq_ignore_ascii_case(s.trim()) || format!("{a:?}").to_ascii_lowercase() == t
    })
}

/// File a case. Anyone may petition this court — that is what makes it a court and not a panel.
pub async fn court_file_case(State(s): State<AppState>, Json(req): Json<FileCaseReq>) -> Json<ApiResponse<serde_json::Value>> {
    let Some(w) = hex32(&req.wallet) else { return ApiResponse::err("wallet must be 64 hex") };
    let Some(article) = article_by_name(&req.article) else {
        return ApiResponse::err(format!("unknown article {:?} — use a numeral (I..XII) or the name", req.article));
    };
    if req.claim.trim().is_empty() { return ApiResponse::err("a case needs a claim") }
    if req.claim.len() > 4096 { return ApiResponse::err("claim is too long (max 4096 bytes)") }
    let msg = act_challenge("file-case", &w, &format!("{}|{}", article.numeral(), req.claim), req.nonce);
    if !verify_wallet_sig(&w, &msg, &req.signature) { return ApiResponse::err("signature does not verify for that wallet") }
    let defendant = req.defendant.as_deref().and_then(hex32);
    if req.defendant.is_some() && defendant.is_none() { return ApiResponse::err("defendant must be 64 hex") }
    let height = s.court.coverage().map(|(_, t)| t).unwrap_or(0);
    let mut c = match s.court.court.write() { Ok(c) => c, Err(_) => return ApiResponse::err("court lock poisoned") };
    let case = c.file_case(w, defendant, article, req.claim.clone(), height);
    ApiResponse::ok(serde_json::json!({
        "case": hex::encode(case), "article": article.numeral(), "status": "Filed",
        "next": "a panel must be seated: POST /v1/court/case/hear"
    }))
}

#[derive(Debug, Deserialize)]
pub struct HearReq { pub wallet: String, pub case: String, pub nonce: u64, pub signature: String }

/// Seat a deterministic panel and open the hearing. Any sitting member (Magistrate and up) may
/// convene — the panel itself is chosen by BLAKE3(case ‖ wallet) ordering, so convening it gives
/// the convener no say in who hears it.
pub async fn court_hear(State(s): State<AppState>, Json(req): Json<HearReq>) -> Json<ApiResponse<serde_json::Value>> {
    let Some(w) = hex32(&req.wallet) else { return ApiResponse::err("wallet must be 64 hex") };
    let Some(case) = hex32(&req.case) else { return ApiResponse::err("case must be 64 hex") };
    let msg = act_challenge("hear", &w, &req.case, req.nonce);
    if !verify_wallet_sig(&w, &msg, &req.signature) { return ApiResponse::err("signature does not verify for that wallet") }
    let height = s.court.coverage().map(|(_, t)| t).unwrap_or(0);
    let mut c = match s.court.court.write() { Ok(c) => c, Err(_) => return ApiResponse::err("court lock poisoned") };
    match c.bench().rank_of(&w) {
        Some(r) if r.may_sit() => {}
        Some(r) => return ApiResponse::err(format!("a {} may not convene a hearing — Magistrate and above sit", r.name())),
        None => return ApiResponse::err("only a member of the bench may convene a hearing"),
    }
    match c.hear(case, 3, height) {
        Ok(panel) => ApiResponse::ok(serde_json::json!({
            "case": req.case, "panel": panel.iter().map(hex::encode).collect::<Vec<_>>(),
            "next": "every panel member votes: POST /v1/court/case/rule"
        })),
        Err(e) => ApiResponse::err(e.to_string()),
    }
}

#[derive(Debug, Deserialize)]
pub struct RuleReq {
    pub wallet: String,
    pub case: String,
    /// `upheld` | `reversed` | `remanded` | `dismissed`
    pub verdict: String,
    pub holding: String,
    /// Every panel member's vote FOR the verdict: `[["<64-hex>", true], ...]`.
    pub votes: Vec<(String, bool)>,
    pub nonce: u64,
    pub signature: String,
}

/// Rule. The signature proves who submitted the ruling; the CONSTITUTION decides whether it stands
/// — every panel member must have voted, and the verdict must match the majority.
pub async fn court_rule(State(s): State<AppState>, Json(req): Json<RuleReq>) -> Json<ApiResponse<serde_json::Value>> {
    let Some(w) = hex32(&req.wallet) else { return ApiResponse::err("wallet must be 64 hex") };
    let Some(case) = hex32(&req.case) else { return ApiResponse::err("case must be 64 hex") };
    let verdict = match req.verdict.trim().to_ascii_lowercase().as_str() {
        "upheld" => Verdict::Upheld, "reversed" => Verdict::Reversed,
        "remanded" => Verdict::Remanded, "dismissed" => Verdict::Dismissed,
        other => return ApiResponse::err(format!("unknown verdict {other:?}: upheld | reversed | remanded | dismissed")),
    };
    let msg = act_challenge("rule", &w, &format!("{}|{}|{}", req.case, req.verdict, req.holding), req.nonce);
    if !verify_wallet_sig(&w, &msg, &req.signature) { return ApiResponse::err("signature does not verify for that wallet") }
    let mut votes = Vec::with_capacity(req.votes.len());
    for (h, v) in &req.votes {
        let Some(vw) = hex32(h) else { return ApiResponse::err(format!("vote wallet {h:?} is not 64 hex")) };
        votes.push((vw, *v));
    }
    let height = s.court.coverage().map(|(_, t)| t).unwrap_or(0);
    let mut c = match s.court.court.write() { Ok(c) => c, Err(_) => return ApiResponse::err("court lock poisoned") };
    if !c.bench().rank_of(&w).map(|r| r.may_sit()).unwrap_or(false) {
        return ApiResponse::err("only a sitting member may submit a ruling");
    }
    match c.rule(case, verdict, req.holding.clone(), &votes, vec![], height) {
        Ok(r) => ApiResponse::ok(serde_json::json!({
            "ruling": hex::encode(r), "verdict": format!("{verdict:?}"),
            "sets_precedent": verdict != Verdict::Dismissed,
            "next": "a party may appeal once: POST /v1/court/case/appeal"
        })),
        Err(e) => ApiResponse::err(e.to_string()),
    }
}

#[derive(Debug, Deserialize)]
pub struct AppealReq { pub wallet: String, pub case: String, pub nonce: u64, pub signature: String }

/// Art. VI — appeal, once, by a party. Being on the bench is NOT standing: a justice who is not a
/// party to this case cannot appeal it, which is the whole point of the rule.
pub async fn court_appeal(State(s): State<AppState>, Json(req): Json<AppealReq>) -> Json<ApiResponse<serde_json::Value>> {
    let Some(w) = hex32(&req.wallet) else { return ApiResponse::err("wallet must be 64 hex") };
    let Some(case) = hex32(&req.case) else { return ApiResponse::err("case must be 64 hex") };
    let msg = act_challenge("appeal", &w, &req.case, req.nonce);
    if !verify_wallet_sig(&w, &msg, &req.signature) { return ApiResponse::err("signature does not verify for that wallet") }
    let height = s.court.coverage().map(|(_, t)| t).unwrap_or(0);
    let mut c = match s.court.court.write() { Ok(c) => c, Err(_) => return ApiResponse::err("court lock poisoned") };
    match c.appeal(case, w, height) {
        Ok(under) => {
            let en_banc = c.bench().en_banc(&case);
            ApiResponse::ok(serde_json::json!({
                "case": req.case, "appealing": hex::encode(under),
                "en_banc": en_banc.iter().map(hex::encode).collect::<Vec<_>>(),
                "note": "reversal takes a simple majority; overruling the precedent takes two thirds (Art. VII)",
                "next": "POST /v1/court/case/decide_appeal"
            }))
        }
        Err(e) => ApiResponse::err(e.to_string()),
    }
}

#[derive(Debug, Deserialize)]
pub struct DecideReq {
    pub wallet: String,
    pub case: String,
    pub holding: String,
    /// Each en banc member's vote FOR REVERSAL.
    pub votes: Vec<(String, bool)>,
    pub nonce: u64,
    pub signature: String,
}

/// The full bench decides.
pub async fn court_decide_appeal(State(s): State<AppState>, Json(req): Json<DecideReq>) -> Json<ApiResponse<serde_json::Value>> {
    let Some(w) = hex32(&req.wallet) else { return ApiResponse::err("wallet must be 64 hex") };
    let Some(case) = hex32(&req.case) else { return ApiResponse::err("case must be 64 hex") };
    let msg = act_challenge("decide-appeal", &w, &format!("{}|{}", req.case, req.holding), req.nonce);
    if !verify_wallet_sig(&w, &msg, &req.signature) { return ApiResponse::err("signature does not verify for that wallet") }
    let mut votes = Vec::with_capacity(req.votes.len());
    for (h, v) in &req.votes {
        let Some(vw) = hex32(h) else { return ApiResponse::err(format!("vote wallet {h:?} is not 64 hex")) };
        votes.push((vw, *v));
    }
    let height = s.court.coverage().map(|(_, t)| t).unwrap_or(0);
    let mut c = match s.court.court.write() { Ok(c) => c, Err(_) => return ApiResponse::err("court lock poisoned") };
    if c.bench().rank_of(&w).map(|r| r < Rank::Justice).unwrap_or(true) {
        return ApiResponse::err("only a Justice of this court may submit an en banc decision");
    }
    match c.decide_appeal(case, &votes, req.holding.clone(), height) {
        Ok(v) => ApiResponse::ok(serde_json::json!({ "case": req.case, "verdict": format!("{v:?}"), "status": "Final" })),
        Err(e) => ApiResponse::err(e.to_string()),
    }
}

/// The challenge for any of the sitting acts, so a client never builds one by hand.
#[derive(Debug, Deserialize)]
pub struct ActChallengeQuery { pub kind: String, pub wallet: String, pub subject: String, pub nonce: Option<u64> }

pub async fn court_act_challenge(Query(q): Query<ActChallengeQuery>) -> Json<ApiResponse<serde_json::Value>> {
    let Some(w) = hex32(&q.wallet) else { return ApiResponse::err("wallet must be 64 hex") };
    const KINDS: [&str; 5] = ["file-case", "hear", "rule", "appeal", "decide-appeal"];
    if !KINDS.contains(&q.kind.as_str()) {
        return ApiResponse::err(format!("kind must be one of {KINDS:?}"));
    }
    let nonce = q.nonce.unwrap_or_else(now_s);
    ApiResponse::ok(serde_json::json!({
        "challenge": act_challenge(&q.kind, &w, &q.subject, nonce), "nonce": nonce,
        "sign_with": "ed25519 over the exact challenge bytes, hex-encoded"
    }))
}

/// The challenge string a wallet must sign, so a UI never has to construct it by hand and get it
/// subtly wrong.
#[derive(Debug, Deserialize)]
pub struct ChallengeQuery { pub wallet: String, pub from_height: Option<u64>, pub to_height: Option<u64>, pub nonce: Option<u64> }

pub async fn court_challenge(State(s): State<AppState>, Query(q): Query<ChallengeQuery>) -> Json<ApiResponse<serde_json::Value>> {
    let Some(w) = hex32(&q.wallet) else { return ApiResponse::err("wallet must be 64 hex") };
    let Some((cf, ct)) = s.court.coverage() else { return ApiResponse::err("the court holds no blocks yet") };
    let from = q.from_height.unwrap_or(cf);
    let to = q.to_height.unwrap_or(ct);
    let nonce = q.nonce.unwrap_or_else(now_s);
    ApiResponse::ok(serde_json::json!({
        "challenge": self_challenge(&w, from, to, nonce),
        "wallet": q.wallet, "from_height": from, "to_height": to, "nonce": nonce,
        "archive_from": cf, "archive_to": ct,
        "sign_with": "ed25519 over the exact challenge bytes (no trailing newline), hex-encoded"
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sigil_state::NATIVE;
    use std::sync::Arc;

    fn w(b: u8) -> WalletId { [b; 32] }

    fn bridge() -> Arc<CourtBridge> {
        let b = Arc::new(CourtBridge::new());
        for h in 100..110u64 {
            let ev = vec![
                SigilEvent::MintReward { miner: w(7), height: h, amount: 100 + h as u128 },
                SigilEvent::Send { from: w(7), to: w(8), amount: 5, token: NATIVE, fee: 1 },
            ];
            let sealed = BlockEvents::seal(h, ev.clone());
            assert!(b.record_block(h, sealed.event_log_root, ev));
        }
        b
    }

    #[test]
    fn archive_refuses_a_block_whose_root_does_not_match_its_events() {
        let b = CourtBridge::new();
        let ev = vec![SigilEvent::MintReward { miner: w(1), height: 1, amount: 1 }];
        assert!(!b.record_block(1, [0xAA; 32], ev.clone()), "a wrong root must be refused");
        assert_eq!(b.coverage(), None);
        let good = BlockEvents::seal(1, ev.clone());
        assert!(b.record_block(1, good.event_log_root, ev));
        assert_eq!(b.coverage(), Some((1, 1)));
    }

    #[test]
    fn archive_is_bounded_and_drops_the_oldest() {
        let b = CourtBridge { max_blocks: 3, ..CourtBridge::new() };
        for h in 1..=6u64 {
            let ev = vec![SigilEvent::MintReward { miner: w(1), height: h, amount: h as u128 }];
            let s = BlockEvents::seal(h, ev.clone());
            b.record_block(h, s.event_log_root, ev);
        }
        assert_eq!(b.coverage(), Some((4, 6)));
    }

    #[test]
    fn the_challenge_is_bound_to_wallet_range_and_nonce() {
        let a = self_challenge(&w(1), 10, 20, 5);
        assert_ne!(a, self_challenge(&w(2), 10, 20, 5));
        assert_ne!(a, self_challenge(&w(1), 11, 20, 5));
        assert_ne!(a, self_challenge(&w(1), 10, 21, 5));
        assert_ne!(a, self_challenge(&w(1), 10, 20, 6), "a captured signature must not authorise a second export");
        assert!(a.starts_with("sigil-court/self-disclosure/v1"));
    }

    #[test]
    fn a_share_link_points_at_sigilgraph_not_quillon() {
        let b = bridge();
        let url = b.share_url(&[0xAB; 32]);
        assert!(url.starts_with("https://sigilgraph.org/v1/court/packet/"), "{url}");
        assert!(!url.contains("quillon"), "SIGIL deliverables never carry a quillon.xyz link");
    }

    /// The two identities this nation actually seats. Rocky's is BLAKE3-committed into the SIGIL
    /// genesis header, so it cannot change without forking the chain; the master is the dev-fee
    /// wallet baked into block 0. Pinned here so a typo in a systemd drop-in cannot quietly seat
    /// the wrong wallet as the highest authority of the court.
    #[test]
    fn the_named_identities_are_the_genesis_ones() {
        const ROCKY: &str = "87ed473b028cff8aed5ce27dfe97eac8e560f5fbe54020f01ca8f5db7e369c6e";
        const MASTER: &str = "095b0e1f7f5bb258fb11427c4ac036e3d9e4f10fa39d7f282aa42862dc2b3dd8";
        assert_eq!(hex32(ROCKY).map(hex::encode).as_deref(), Some(ROCKY));
        assert_eq!(hex32(MASTER).map(hex::encode).as_deref(), Some(MASTER));
        assert_ne!(ROCKY, MASTER);
    }

    /// A seat is the constituent act; an ELEVATION is quorum-gated. Seating must never become a
    /// back door into the ladder Art. VIII guards.
    #[test]
    fn a_seat_is_not_a_promotion() {
        let mut c = SupremeCourt::from_seed(&[5; 32]);
        c.appoint(w(1), Rank::ChiefJustice, 0).unwrap();
        c.appoint(w(2), Rank::Justice, 0).unwrap();
        c.appoint(w(3), Rank::Justice, 0).unwrap();
        c.appoint(w(9), Rank::Clerk, 0).unwrap();
        // Seated at Clerk, the honoured wallet still cannot be elevated by one voice.
        assert!(c.promote(w(9), Rank::Advocate, &[w(1)], 1).is_err());
        // And the honour itself is refused without the quorum the nation's rule demands.
        assert!(c.confer_honour(sigil_court::Order::Elefantordenen, "", w(9), "deeds", [0; 32], 0, true, 1).is_err());
        let ev = c.confer_honour(sigil_court::Order::Elefantordenen, "", w(9), "deeds", [0; 32], 3, true, 1);
        assert!(ev.is_ok(), "{ev:?}");
        // The honour did not move the rank.
        assert_eq!(c.bench().rank_of(&w(9)), Some(Rank::Clerk));
    }

    /// The EXACT strings that ship in the systemd drop-in, run through the exact code that reads
    /// them. This is the test that would have caught a typo seating the wrong wallet as the
    /// highest authority of the court — a mistake no compiler and no unit test of the parser in
    /// isolation would have found.
    #[test]
    fn the_shipped_drop_in_seats_exactly_who_it_says() {
        const ROCKY: &str = "87ed473b028cff8aed5ce27dfe97eac8e560f5fbe54020f01ca8f5db7e369c6e";
        const MASTER: &str = "095b0e1f7f5bb258fb11427c4ac036e3d9e4f10fa39d7f282aa42862dc2b3dd8";
        const VICARIOUS: &str = "c0beb1a79e31f5db568d3377b48c260c2de11292d3110cf3e0b1ef4c36080917";
        const QUINN: &str = "a6ca843bd7187aac2e8ddbf51dad66718248782da521a7551c8deeb2421ea212";
        const MIMER: &str = "81e5c73296bf8ee00af3af76f6bd9d844ba54dafa3b4d155f7e4cb234c816aa3";
        let bench_spec = format!("{ROCKY}:chief,{VICARIOUS}:justice,{QUINN}:justice,{MIMER}:justice,{MASTER}:justice");
        let honours_spec = format!("{ROCKY}:elephant:Kept the ledger honest");

        let mut c = SupremeCourt::from_seed(&[7; 32]);
        assert_eq!(seat_from_spec(&mut c, &bench_spec), 5);
        let (rocky, master) = (hex32(ROCKY).unwrap(), hex32(MASTER).unwrap());
        assert_eq!(c.bench().rank_of(&rocky), Some(Rank::ChiefJustice), "Rocky must be Chief Justice");
        assert_eq!(c.bench().chief(), Some(rocky), "and must be THE chief, singular");
        assert_eq!(c.bench().rank_of(&master), Some(Rank::Justice), "the master sits as a Justice");

        // The master can rule (a Justice may sit) and votes on every appeal en banc.
        assert!(c.bench().rank_of(&master).unwrap().may_sit());
        assert!(c.bench().en_banc(&[0u8; 32]).contains(&master));
        assert!(c.bench().en_banc(&[0u8; 32]).contains(&rocky));

        // Five named seats already make a quorate panel: NO anonymous seats are needed, which is
        // what a fully-named bench looks like.
        let derived = top_up_bench(&mut c, &[7; 32]);
        assert_eq!(derived, 0, "a named bench must not be padded with anonymous seats");
        assert_eq!(c.bench().select_panel(&[1u8; 32], 3).map(|p| p.len()), Ok(3));

        // The honour lands, and it lands on Rocky and nobody else.
        assert_eq!(confer_from_spec(&mut c, &honours_spec), 1);
        assert!(c.bench().get(&rocky).unwrap().has(sigil_court::Credential::Aeresborger));
        assert!(!c.bench().get(&master).unwrap().has(sigil_court::Credential::Aeresborger));
        // Recorded on the docket, with the citation, forever.
        assert_eq!(c.docket().by_tag(19).len(), 1);
        // And it is soulbound: a second conferral is refused.
        assert_eq!(confer_from_spec(&mut c, &honours_spec), 0);
    }

    /// A bench too small to supply the Elephant's quorum must REFUSE the honour, not grant a
    /// lesser one quietly. This is the failure mode that would turn the nation's highest honour
    /// into a participation trophy.
    #[test]
    fn an_honour_without_its_quorum_is_refused_not_downgraded() {
        const ROCKY: &str = "87ed473b028cff8aed5ce27dfe97eac8e560f5fbe54020f01ca8f5db7e369c6e";
        let mut c = SupremeCourt::from_seed(&[8; 32]);
        // Rocky alone on the bench: zero other Justices, so zero approvals available.
        assert_eq!(seat_from_spec(&mut c, &format!("{ROCKY}:chief")), 1);
        assert_eq!(confer_from_spec(&mut c, &format!("{ROCKY}:elephant:deeds")), 0, "refused");
        let rocky = hex32(ROCKY).unwrap();
        assert!(!c.bench().get(&rocky).unwrap().has(sigil_court::Credential::Aeresborger));
        assert_eq!(c.docket().by_tag(19).len(), 0, "nothing was recorded");
    }

    /// A malformed seat is skipped, never fatal, and never silently promoted.
    #[test]
    fn a_typo_in_one_seat_does_not_take_the_court_down() {
        let mut c = SupremeCourt::from_seed(&[9; 32]);
        let good = "87ed473b028cff8aed5ce27dfe97eac8e560f5fbe54020f01ca8f5db7e369c6e";
        assert_eq!(seat_from_spec(&mut c, &format!("nothex:chief,{good}:chief,,{good}:justice")), 1);
        assert_eq!(c.bench().rank_of(&hex32(good).unwrap()), Some(Rank::ChiefJustice), "the duplicate did not demote them");
        // An unknown rank word seats as a Justice rather than guessing something powerful.
        let other = "095b0e1f7f5bb258fb11427c4ac036e3d9e4f10fa39d7f282aa42862dc2b3dd8";
        assert_eq!(seat_from_spec(&mut c, &format!("{other}:emperor")), 1);
        assert_eq!(c.bench().rank_of(&hex32(other).unwrap()), Some(Rank::Justice));
    }

    #[test]
    fn a_bad_signature_cannot_disclose_a_wallet() {
        assert!(!verify_wallet_sig(&w(3), "msg", "00"));
        assert!(!verify_wallet_sig(&w(3), "msg", &"ab".repeat(64)));
    }
}
