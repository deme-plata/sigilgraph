// sigil-top/src/shield_scan.rs — the private-balance scanner, incremental (2026-10-17, 10.4.38).
//
// WHAT IT REPLACES, MEASURED ON LIVE g2 (2026-10-17). Up to 10.4.37 `scan_shielded` re-downloaded
// EVERY pool generation in full every 45 s: 40.3 + 43.4 + 28.6 MB of notes plus 1.3 MB of spent
// markers ≈ 113 MB per cycle, uncompressed (the client never asked for compression), for as long
// as a sigil-top holding a wallet mines. It also kept all generations in ONE note store and
// compared RAW spent markers, while the chain files every spend after the first rotation under a
// generation-scoped key (`sigil_state::shielded::scoped_nullifier`): a spent note in generation 1
// or 2 never read as spent (balance too high), and a note whose position number was already taken
// by our note in an earlier generation was never located (balance too low).
//
// NOW:
//  * Per generation: how far it was scanned and what was found there. Positions are append-only,
//    so each cycle asks only for `from=<scanned>`; a SEALED generation, once read to its end, is
//    never fetched again.
//  * Spent markers are compared under the chain's own scoping rule, and re-fetched only when the
//    chain's count moved and we hold notes at all.
//  * Every request asks for zstd (the node already serves it: 1.3 MB → 417 KB for the spent set).
//  * VIEW TAGS: when the node serves `/v1/shielded/tags`, the scanner downloads 34 bytes per new
//    note and fetches whole envelopes only around notes that could be ours — untagged ones (sealed
//    before 2026-10-17, so old history costs what it always did) and tagged ones whose one-byte tag
//    matches our key (1 in 256 for a stranger's). A node without the route is used as before.
//
// PRIVACY TRADE, said plainly: fetching envelopes only near candidates tells the node which
// 16-position blocks interested us (our notes plus ~1/256 of everyone else's, rounded to blocks).
// The full download told it nothing. That is the price of not downloading everything; it applies
// only to TAGGED notes, and only on a node that serves tags.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use sigil_shield::note_cipher::{enc_identity_from_seed, may_be_ours, NoteCiphertext, TagView};
use sigil_shield::note_v1::to_wire;
use sigil_shield::wallet::{NoteStore, ShieldedAccount};
use sigil_shield::wallet_v7::{scoped_nullifier_v7, AccountV7, NoteStoreV7};

use crate::shield_setup::ShieldedSnapshot;

/// One HTTP answer, already decompressed. `wire` = the bytes that crossed the network.
pub(crate) struct Fetched {
    pub status: u16,
    pub body: Vec<u8>,
    pub wire: u64,
}

/// How the scanner fetches: reqwest in production, an in-memory node in the tests.
pub(crate) type Getter<'a> = dyn FnMut(&str) -> Option<Fetched> + 'a;

/// Largest decompressed reply accepted — a zstd-bomb guard. A whole sealed generation is ~45 MB.
const MAX_REPLY: u64 = 256 * 1024 * 1024;
/// Tag records asked for per request (34 B each → ~280 KB).
const TAG_CHUNK: u64 = 8192;
/// Envelopes are fetched in aligned blocks of this many positions around a candidate, so the node
/// sees a block rather than one position, and a payment's two outputs usually share a block.
const BLOCK: u64 = 16;
/// Most leaves asked for in one ranged request.
const LEAF_CHUNK: u64 = 4096;
/// After a node said it has no tag route, ask again this often (it may be upgraded meanwhile).
const TAGS_REPROBE: Duration = Duration::from_secs(10 * 60);

/// GET with `Accept-Encoding: zstd`, decoded with the pure-Rust decoder this crate already uses.
pub(crate) fn http_getter(client: reqwest::blocking::Client) -> impl FnMut(&str) -> Option<Fetched> {
    move |url| {
        let resp = client
            .get(url)
            .header(reqwest::header::ACCEPT_ENCODING, "zstd")
            .send()
            .ok()?;
        let status = resp.status().as_u16();
        let zstd = resp
            .headers()
            .get(reqwest::header::CONTENT_ENCODING)
            .is_some_and(|v| v.as_bytes().eq_ignore_ascii_case(b"zstd"));
        let raw = resp.bytes().ok()?;
        let wire = raw.len() as u64;
        let body = if zstd { inflate(&raw)? } else { raw.to_vec() };
        Some(Fetched { status, body, wire })
    }
}

fn inflate(z: &[u8]) -> Option<Vec<u8>> {
    use std::io::Read;
    let dec = ruzstd::StreamingDecoder::new(z).ok()?;
    let mut out = Vec::new();
    dec.take(MAX_REPLY + 1).read_to_end(&mut out).ok()?;
    (out.len() as u64 <= MAX_REPLY).then_some(out)
}

fn json(f: &Fetched) -> Option<serde_json::Value> {
    if f.status != 200 {
        return None;
    }
    serde_json::from_slice(&f.body).ok()
}

fn wire32(h: &str) -> Option<[u8; 32]> {
    hex::decode(h).ok()?.try_into().ok()
}

/// One `/v1/shielded/tags` record: `kind` (0 none · 1 untagged · 2 tagged · 3 unreadable), tag,
/// one-time key. Layout pinned by `sigil_api::shielded_tags_handler`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TagRecord {
    pub kind: u8,
    pub vt: u8,
    pub eph_pk: [u8; 32],
}

/// A parsed `/v1/shielded/tags` reply.
#[derive(Debug)]
pub(crate) struct TagFrame {
    pub epoch: u32,
    pub from: u64,
    pub total: u64,
    pub records: Vec<TagRecord>,
}

/// `"SGT1" ‖ epoch u32 ‖ from u64 ‖ total u64 ‖ n u32 ‖ n × { kind u8 ‖ vt u8 ‖ eph_pk [32] }`,
/// little-endian. `None` unless the length is exactly what the header promises.
pub(crate) fn parse_tag_frame(b: &[u8]) -> Option<TagFrame> {
    if b.len() < 28 || &b[..4] != b"SGT1" {
        return None;
    }
    let epoch = u32::from_le_bytes(b[4..8].try_into().ok()?);
    let from = u64::from_le_bytes(b[8..16].try_into().ok()?);
    let total = u64::from_le_bytes(b[16..24].try_into().ok()?);
    let n = u32::from_le_bytes(b[24..28].try_into().ok()?) as usize;
    if b.len() != 28 + n.checked_mul(34)? || from.checked_add(n as u64)? > total {
        return None;
    }
    let records = b[28..]
        .chunks_exact(34)
        .map(|r| TagRecord { kind: r[0], vt: r[1], eph_pk: r[2..34].try_into().unwrap() })
        .collect();
    Some(TagFrame { epoch, from, total, records })
}

#[derive(Debug, Default)]
struct Generation {
    /// Positions `[0, scanned)` have been looked at.
    scanned: u64,
    /// Sealed and read to its end: it can never change again.
    complete: bool,
    /// The permanent anchor of a sealed generation, to notice a different chain.
    anchor: Option<String>,
    /// position → value of every note here that we can open and have located.
    notes: BTreeMap<u64, u64>,
    /// The commitment at position `scanned - 1`, checked before reading on: if it moved, the
    /// URL now serves a different chain and everything starts over.
    last_leaf: Option<[u8; 32]>,
    /// sigil-g3: a 256-bit (v7) generation — its own opener and spent-key family.
    v7: bool,
}

#[derive(Debug)]
enum Tags {
    Unknown,
    Yes,
    No(Instant),
}

enum TagsAnswer {
    Frame(TagFrame),
    Unsupported,
}

/// The scanner's memory between cycles. Lives in the scanner thread; nothing is written to disk.
pub(crate) struct ShieldedScanner {
    base: String,
    seed: [u8; 32],
    account: ShieldedAccount,
    account7: AccountV7,
    gens: Vec<Generation>,
    spent: BTreeSet<[u8; 32]>,
    spent_count: Option<u64>,
    tags: Tags,
    /// Notes we could open but whose commitment was not in the page they arrived in (only a
    /// sender that attached an envelope to the wrong output produces one). Counted, not shown.
    pub(crate) unlocated: usize,
}

impl ShieldedScanner {
    /// 2026-10-23: switch the node this scanner reads from (your own node once it is caught up, the network
    /// otherwise). Safe between cycles: both serve the same chain, and a node a few blocks behind just answers a
    /// little less until the next cycle.
    pub(crate) fn set_base(&mut self, node_url: &str) -> bool {
        let b = node_url.trim_end_matches('/').to_string();
        if b == self.base {
            return false;
        }
        self.base = b;
        true
    }

    pub(crate) fn new(node_url: &str, seed: [u8; 32]) -> Self {
        Self {
            base: node_url.trim_end_matches('/').to_string(),
            seed,
            account: ShieldedAccount::from_seed(seed),
            account7: AccountV7::from_seed(seed),
            gens: Vec::new(),
            spent: BTreeSet::new(),
            spent_count: None,
            tags: Tags::Unknown,
            unlocated: 0,
        }
    }

    fn reset(&mut self) {
        self.gens.clear();
        self.spent.clear();
        self.spent_count = None;
    }

    /// One scan: only what is new since the last one. `None` = no reading this cycle (a fetch
    /// failed or the node answered something unusable); what was already scanned is kept.
    pub(crate) fn cycle(&mut self, get: &mut Getter<'_>) -> Option<ShieldedSnapshot> {
        let mut wire = 0u64;
        let snap = {
            let mut counted = |url: &str| {
                let f = get(url)?;
                wire += f.wire;
                Some(f)
            };
            self.cycle_inner(&mut counted)
        };
        snap.map(|mut s| {
            s.fetched_bytes = wire;
            s
        })
    }

    fn cycle_inner(&mut self, get: &mut Getter<'_>) -> Option<ShieldedSnapshot> {
        let a = json(&get(&format!("{}/v1/shielded/anchor", self.base))?)?;
        if a.get("ok").and_then(|v| v.as_bool()) == Some(false) {
            return None;
        }
        let num = |k: &str| a.get(k).and_then(|v| v.as_u64()).unwrap_or(0);
        let live = num("epoch") as usize;
        let sealed: BTreeMap<usize, String> = a
            .get("sealed_epochs")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|s| {
                        Some((s.get("epoch")?.as_u64()? as usize, s.get("anchor")?.as_str()?.to_string()))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let mut snap = ShieldedSnapshot {
            pool_notes: num("notes") as usize,
            pool_capacity: num("capacity") as usize,
            pool_locked: a
                .get("value_locked")
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse::<u128>().ok())
                .unwrap_or(0),
            epoch: live as u32,
            sealed_epochs: sealed.len(),
            registered: num("registered") as usize,
            nullifiers: num("nullifiers") as usize,
            ..Default::default()
        };

        // sigil-g3: the v7 generation, when the node has opened one — numbered ABOVE the v1
        // epochs (on g3: v1 epoch 0 sealed empty, v7 epoch 1), so the scan reaches up to it.
        let v7_epoch = a.get("v7").and_then(|v| v.get("epoch")).and_then(|v| v.as_u64()).map(|x| x as usize);
        let top = live.max(v7_epoch.unwrap_or(0));
        snap.v7_epoch = v7_epoch.map(|x| x as u32);
        snap.v7_notes = a.get("v7").and_then(|v| v.get("notes")).and_then(|v| v.as_u64()).unwrap_or(0) as usize;

        // A different chain (re-genesis, another network behind the same URL): start over. Fewer
        // generations than we already hold, or a sealed generation whose permanent anchor
        // changed, can only mean that.
        let foreign = self.gens.len() > top + 1
            || self.gens.iter().enumerate().any(|(e, g)| {
                g.anchor.as_ref().is_some_and(|mine| sealed.get(&e) != Some(mine))
            });
        if foreign {
            self.reset();
        }
        while self.gens.len() <= top {
            self.gens.push(Generation::default());
        }
        if let Some(v) = v7_epoch {
            self.gens[v].v7 = true;
        }

        for e in 0..=top {
            if self.gens[e].complete {
                continue;
            }
            self.scan_generation(e as u32, get)?;
            // Sealed = listed as sealed by the node (was `e < live`, which is the same thing
            // on a v1-only chain; with v7 above the v1 number the live v7 epoch is not sealed).
            if let Some(anchor) = sealed.get(&e) {
                let g = &mut self.gens[e];
                g.complete = true;
                g.anchor = Some(anchor.clone());
            }
        }

        // Spent markers: the whole set is public by design (it is the double-spend guard), so
        // fetching it reveals nothing. Fetch only when it moved, and only if we own notes.
        let owned = self.gens.iter().map(|g| g.notes.len()).sum::<usize>();
        let chain_count = num("nullifiers");
        if owned > 0 && self.spent_count != Some(chain_count) {
            let n = json(&get(&format!("{}/v1/shielded/nullifiers", self.base))?)?;
            let list = n.get("nullifiers").and_then(|v| v.as_array())?;
            self.spent = list.iter().filter_map(|v| v.as_str().and_then(wire32)).collect();
            self.spent_count = Some(n.get("count").and_then(|v| v.as_u64()).unwrap_or(chain_count));
        }

        for (e, g) in self.gens.iter().enumerate() {
            for (&pos, &value) in &g.notes {
                snap.owned += 1;
                let key = if g.v7 {
                    scoped_nullifier_v7(e as u32, self.account7.nullifier_at(pos).to_wire())
                } else {
                    sigil_state::shielded::scoped_nullifier(e as u32, to_wire(self.account.nullifier_at(pos)))
                };
                if self.spent.contains(&key) {
                    snap.spent += 1;
                } else {
                    snap.balance += value as u128;
                    if g.v7 {
                        snap.v7_balance += value as u128;
                    }
                }
            }
        }
        snap.view_tags = matches!(self.tags, Tags::Yes);
        Some(snap)
    }

    /// Read generation `e` from where we stopped to its current end.
    fn scan_generation(&mut self, e: u32, get: &mut Getter<'_>) -> Option<()> {
        let i = e as usize;
        // Same chain? The commitment we saw last must still be where we saw it (one leaf, no
        // envelope). A node holding fewer notes than we scanned fails this the same way.
        if self.gens[i].scanned > 0 {
            let p = self.gens[i].scanned - 1;
            if self.leaf_at(e, p, get)? != self.gens[i].last_leaf {
                self.reset();
                return None;
            }
        }
        loop {
            let from = self.gens[i].scanned;
            let use_tags = match self.tags {
                Tags::Yes | Tags::Unknown => true,
                Tags::No(at) => at.elapsed() >= TAGS_REPROBE,
            };
            if use_tags {
                match self.fetch_tags(e, from, get)? {
                    TagsAnswer::Frame(f) => {
                        self.tags = Tags::Yes;
                        if f.from != from {
                            return None;
                        }
                        self.take_tag_frame(e, &f, get)?;
                        if self.gens[i].scanned >= f.total {
                            return Some(());
                        }
                        continue;
                    }
                    TagsAnswer::Unsupported => self.tags = Tags::No(Instant::now()),
                }
            }
            // No tags: the whole tail from `from`, as one page.
            let url = format!("{}/v1/shielded/leaves?epoch={e}&from={from}", self.base);
            let page = json(&get(&url)?)?;
            let (got_from, total, n, last) = self.take_page(e, &page)?;
            if got_from != from {
                return None;
            }
            let g = &mut self.gens[i];
            g.scanned = from + n;
            if last.is_some() {
                g.last_leaf = last;
            }
            if g.scanned >= total {
                return Some(());
            }
        }
    }

    /// The commitment at position `p` of generation `e`: outer `None` = no answer, inner `None`
    /// = the node has no such position.
    fn leaf_at(&self, e: u32, p: u64, get: &mut Getter<'_>) -> Option<Option<[u8; 32]>> {
        let url = format!("{}/v1/shielded/leaves?epoch={e}&from={p}&count=1&ciphertexts=0", self.base);
        let page = json(&get(&url)?)?;
        if page.get("ok").and_then(|v| v.as_bool()) != Some(true) {
            return Some(None);
        }
        if page.get("from").and_then(|v| v.as_u64()) != Some(p) {
            return Some(None);
        }
        Some(page.get("leaves")?.as_array()?.first().and_then(|v| v.as_str()).and_then(wire32))
    }

    fn fetch_tags(&mut self, e: u32, from: u64, get: &mut Getter<'_>) -> Option<TagsAnswer> {
        let url = format!("{}/v1/shielded/tags?epoch={e}&from={from}&count={TAG_CHUNK}", self.base);
        let f = get(&url)?;
        if f.status == 200 && f.body.starts_with(b"SGT1") {
            let frame = parse_tag_frame(&f.body)?;
            return (frame.epoch == e).then_some(TagsAnswer::Frame(frame));
        }
        // A 200 JSON refusal ("no such epoch") is the node speaking: no reading this cycle.
        if f.status == 200 {
            if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&f.body) {
                if v.get("ok").and_then(|o| o.as_bool()) == Some(false) {
                    return None;
                }
            }
        }
        // 404 / 405 / an HTML fallback page: this node has no tag route.
        Some(TagsAnswer::Unsupported)
    }

    /// Decide from the tags which envelopes to fetch, fetch them in aligned blocks, open them.
    fn take_tag_frame(&mut self, e: u32, f: &TagFrame, get: &mut Getter<'_>) -> Option<()> {
        let id = enc_identity_from_seed(&self.seed);
        let mut blocks = BTreeSet::new();
        for (i, r) in f.records.iter().enumerate() {
            let candidate = match r.kind {
                0 => false, // no envelope at this position: nothing to find
                2 => may_be_ours(&TagView { eph_pk: r.eph_pk, vt: Some(r.vt) }, &id),
                _ => true, // untagged (sealed before tags) or unreadable: only opening it can tell
            };
            if candidate {
                blocks.insert((f.from + i as u64) / BLOCK);
            }
        }
        // Consecutive blocks become one ranged request (capped at LEAF_CHUNK positions).
        let mut ranges: Vec<(u64, u64)> = Vec::new();
        for b in blocks {
            let (start, end) = (b * BLOCK, ((b + 1) * BLOCK).min(f.total));
            match ranges.last_mut() {
                Some(last) if last.1 == start && end - last.0 <= LEAF_CHUNK => last.1 = end,
                _ => ranges.push((start, end)),
            }
        }
        for (start, end) in ranges {
            let url = format!(
                "{}/v1/shielded/leaves?epoch={e}&from={start}&count={}",
                self.base,
                end - start
            );
            let page = json(&get(&url)?)?;
            let (got_from, _, _, _) = self.take_page(e, &page)?;
            if got_from != start {
                return None;
            }
        }
        let n = f.records.len() as u64;
        if n > 0 {
            let last = self.leaf_at(e, f.from + n - 1, get)??;
            self.gens[e as usize].last_leaf = Some(last);
        }
        self.gens[e as usize].scanned = f.from + n;
        Some(())
    }

    /// Open one `/v1/shielded/leaves` page and record what is ours. Returns
    /// `(from, total, n, last commitment)`.
    fn take_page(&mut self, e: u32, page: &serde_json::Value) -> Option<(u64, u64, u64, Option<[u8; 32]>)> {
        if page.get("ok").and_then(|v| v.as_bool()) != Some(true)
            || page.get("epoch").and_then(|v| v.as_u64()) != Some(e as u64)
        {
            return None;
        }
        let from = page.get("from").and_then(|v| v.as_u64())?;
        let total = page.get("total").and_then(|v| v.as_u64())?;
        let leaves: Vec<[u8; 32]> = page
            .get("leaves")?
            .as_array()?
            .iter()
            .map(|v| v.as_str().and_then(wire32))
            .collect::<Option<_>>()?;
        let cts: Vec<NoteCiphertext> = page
            .get("ciphertexts")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|v| v.as_str()).map(|s| NoteCiphertext(s.to_string())).collect())
            .unwrap_or_default();
        // Trial decryption IS the ownership proof; then locate each note by its commitment within
        // the page it arrived in (a payment's outputs are adjacent), never across generations.
        // sigil-g3: a v7 page opens with the v7 account (the delivery key is the same X25519 one).
        if page.get("generation").and_then(|v| v.as_str()) == Some("v7") {
            self.gens[e as usize].v7 = true;
        }
        let id = enc_identity_from_seed(&self.seed);
        let found: Vec<(Option<u64>, u64)> = if self.gens[e as usize].v7 {
            let mut store = NoteStoreV7::new();
            store.scan_ciphertexts(&self.account7.enc_identity(), &cts);
            store.scan_owned(&self.account7, &leaves);
            store.notes.iter().map(|n| (n.position, n.value)).collect()
        } else {
            let mut store = NoteStore::new();
            store.scan_ciphertexts(&id, &cts);
            store.scan_owned(&self.account, &leaves);
            store.notes.iter().map(|n| (n.position, n.value)).collect()
        };
        let g = &mut self.gens[e as usize];
        for (pos, value) in found {
            match pos {
                Some(p) => {
                    g.notes.insert(from + p, value);
                }
                None => self.unlocated += 1,
            }
        }
        Some((from, total, leaves.len() as u64, leaves.last().copied()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sigil_shield::note_cipher::{seal_note, tag_view, NotePlaintext};
    use sigil_state::shielded::scoped_nullifier;

    /// An in-memory node that answers exactly the shapes `sigil-api` does.
    #[derive(Default)]
    struct FakeNode {
        gens: Vec<(Vec<[u8; 32]>, Vec<Option<String>>)>,
        anchors: Vec<String>,
        nullifiers: Vec<[u8; 32]>,
        tags: bool,
        log: Vec<String>,
        /// sigil-g3 shape: this generation is v7 (and the last); every v1 one before it is sealed.
        v7_gen: Option<usize>,
    }

    struct Who {
        seed: [u8; 32],
        account: ShieldedAccount,
    }

    fn who(b: u8) -> Who {
        Who { seed: [b; 32], account: ShieldedAccount::from_seed([b; 32]) }
    }

    impl FakeNode {
        fn new(generations: usize) -> Self {
            Self {
                gens: (0..generations).map(|_| (Vec::new(), Vec::new())).collect(),
                anchors: (0..generations).map(|i| format!("{:064x}", i + 1)).collect(),
                ..Default::default()
            }
        }

        /// Append a note for `to` at the end of generation `e`; returns its position.
        fn pay(&mut self, e: usize, to: &Who, index: u64, value: u64, tagged: bool) -> u64 {
            let cm = to_wire(to.account.note(index, value).unwrap().commitment());
            let pt = NotePlaintext::new(value, to.account.blinding(index));
            let ct = seal_note(&pt, &to.account.address(&to.seed)).unwrap().0;
            let ct = if tagged {
                ct
            } else {
                // exactly the pre-tag envelope: seal_note appends `,"vt":"xx"` before the `}`
                let old = format!("{}}}", &ct[..ct.len() - 11]);
                assert_eq!(tag_view(&old).unwrap().vt, None);
                old
            };
            let g = &mut self.gens[e];
            g.0.push(cm);
            g.1.push(Some(ct));
            (g.0.len() - 1) as u64
        }

        fn spend(&mut self, e: usize, owner: &Who, pos: u64) {
            let raw = to_wire(owner.account.nullifier_at(pos));
            self.nullifiers.push(scoped_nullifier(e as u32, raw));
        }

        /// A v7 note for `to` (a tagged v7 envelope, as every g3 note carries) at the end of `e`.
        fn pay7(&mut self, e: usize, to: &Who, index: u64, value: u64) -> u64 {
            let a = AccountV7::from_seed(to.seed);
            let cm = a.note(index, value).unwrap().commitment().to_wire();
            let ct = sigil_shield::wallet_v7::seal_output_v7(value, a.blinding(index), &a.pk_enc_hex(), "").unwrap().0;
            let g = &mut self.gens[e];
            g.0.push(cm);
            g.1.push(Some(ct));
            (g.0.len() - 1) as u64
        }

        fn spend7(&mut self, e: usize, owner: &Who, pos: u64) {
            let a = AccountV7::from_seed(owner.seed);
            self.nullifiers.push(scoped_nullifier_v7(e as u32, a.nullifier_at(pos).to_wire()));
        }

        fn get(&mut self, url: &str) -> Option<Fetched> {
            self.log.push(url.to_string());
            let (path, query) = url.split_once('?').unwrap_or((url, ""));
            let q = |k: &str| {
                query
                    .split('&')
                    .find_map(|kv| kv.strip_prefix(&format!("{k}=")))
                    .and_then(|v| v.parse::<u64>().ok())
            };
            let live = self.gens.len() - 1;
            let body = match path.strip_prefix("http://fake").unwrap() {
                // sigil-g3 shape: the v1 number is one below the v7 epoch and v1 is closed
                "/v1/shielded/anchor" => serde_json::json!({
                    "ok": true, "epoch": self.v7_gen.map_or(live, |v| v.saturating_sub(1)),
                    "notes": if self.v7_gen.is_some() { 0 } else { self.gens[live].0.len() }, "capacity": 32768,
                    "nullifiers": self.nullifiers.len(), "registered": 2, "value_locked": "0",
                    "sealed_epochs": (0..live).map(|i| serde_json::json!({
                        "epoch": i, "notes": self.gens[i].0.len(), "anchor": self.anchors[i]
                    })).collect::<Vec<_>>(),
                    "v1_live": self.v7_gen.is_none(),
                    "v7": self.v7_gen.map(|v| serde_json::json!({ "epoch": v, "notes": self.gens[v].0.len() })),
                })
                .to_string()
                .into_bytes(),
                "/v1/shielded/nullifiers" => serde_json::json!({
                    "ok": true, "count": self.nullifiers.len(),
                    "nullifiers": self.nullifiers.iter().map(hex::encode).collect::<Vec<_>>(),
                })
                .to_string()
                .into_bytes(),
                "/v1/shielded/leaves" => {
                    let e = q("epoch").unwrap_or(live as u64) as usize;
                    let Some((leaves, cts)) = self.gens.get(e) else {
                        return Some(Fetched { status: 200, wire: 0, body: br#"{"ok":false}"#.to_vec() });
                    };
                    let total = leaves.len() as u64;
                    let from = q("from").unwrap_or(0).min(total);
                    let to = q("count").map_or(total, |c| (from + c).min(total));
                    let cts: &[Option<String>] =
                        if q("ciphertexts") == Some(0) { &[] } else { &cts[from as usize..to as usize] };
                    serde_json::json!({
                        "ok": true, "epoch": e, "live_epoch": live, "from": from, "total": total,
                        "anchor": "00", "sealed": e != live,
                        "generation": if Some(e) == self.v7_gen { "v7" } else { "v1" },
                        "leaves": leaves[from as usize..to as usize].iter().map(hex::encode).collect::<Vec<_>>(),
                        "ciphertexts": cts,
                    })
                    .to_string()
                    .into_bytes()
                }
                "/v1/shielded/tags" if self.tags => {
                    let e = q("epoch").unwrap() as usize;
                    let cts = &self.gens[e].1;
                    let total = cts.len() as u64;
                    let from = q("from").unwrap_or(0).min(total);
                    let to = q("count").map_or(total, |c| (from + c).min(total));
                    let mut out = b"SGT1".to_vec();
                    out.extend_from_slice(&(e as u32).to_le_bytes());
                    out.extend_from_slice(&from.to_le_bytes());
                    out.extend_from_slice(&total.to_le_bytes());
                    out.extend_from_slice(&((to - from) as u32).to_le_bytes());
                    for ct in &cts[from as usize..to as usize] {
                        let mut r = [0u8; 34];
                        if let Some(ct) = ct {
                            match tag_view(ct) {
                                Some(v) => {
                                    r[0] = if v.vt.is_some() { 2 } else { 1 };
                                    r[1] = v.vt.unwrap_or(0);
                                    r[2..].copy_from_slice(&v.eph_pk);
                                }
                                None => r[0] = 3,
                            }
                        }
                        out.extend_from_slice(&r);
                    }
                    out
                }
                _ => return Some(Fetched { status: 404, wire: 0, body: Vec::new() }),
            };
            Some(Fetched { status: 200, wire: body.len() as u64, body })
        }

        fn requests(&mut self, prefix: &str) -> usize {
            self.log.iter().filter(|u| u.starts_with(&format!("http://fake{prefix}"))).count()
        }

        /// Envelope-carrying leaf requests for generation `e` (not the one-leaf identity checks).
        fn reads(&self, e: usize) -> Vec<String> {
            let pre = format!("http://fake/v1/shielded/leaves?epoch={e}&");
            self.log.iter().filter(|u| u.starts_with(&pre) && !u.ends_with("ciphertexts=0")).cloned().collect()
        }
    }

    fn cycle(s: &mut ShieldedScanner, node: &mut FakeNode) -> ShieldedSnapshot {
        let mut g = |u: &str| node.get(u);
        s.cycle(&mut g).expect("a reading")
    }

    /// 10.4.37's algorithm, kept verbatim as the control: one store for every generation, raw
    /// spent markers.
    fn old_balance(node: &FakeNode, w: &Who) -> u128 {
        let id = enc_identity_from_seed(&w.seed);
        let mut store = NoteStore::new();
        for (leaves, cts) in &node.gens {
            let cts: Vec<NoteCiphertext> = cts.iter().flatten().map(|c| NoteCiphertext(c.clone())).collect();
            store.scan_ciphertexts(&id, &cts);
            store.scan_owned(&w.account, leaves);
        }
        store.mark_spent(&w.account, &node.nullifiers.iter().copied().collect());
        store.balance()
    }

    /// Three generations (0 and 1 sealed, 2 live), Alice's notes among Bob's, some spent.
    fn rotated_chain(tag_live: bool) -> (FakeNode, Who, u128) {
        let (alice, bob) = (who(1), who(2));
        let mut node = FakeNode::new(3);
        let mut idx = 0u64;
        let mut filler = |node: &mut FakeNode, e: usize, n: usize, tagged: bool| {
            for _ in 0..n {
                idx += 1;
                node.pay(e, &bob, idx, 3, tagged);
            }
        };
        // generation 0 (pre-tag): Alice 100 at 0, Alice 7 at 1 (spent)
        let a0 = node.pay(0, &alice, 1, 100, false);
        let a1 = node.pay(0, &alice, 2, 7, false);
        filler(&mut node, 0, 40, false);
        // generation 1 (pre-tag): Alice 20 at 0 — the SAME position number as her generation-0
        // note of 100 — and Alice 50 at 6 (spent)
        let b0 = node.pay(1, &alice, 4, 20, false);
        filler(&mut node, 1, 5, false);
        let b1 = node.pay(1, &alice, 3, 50, false);
        filler(&mut node, 1, 35, false);
        // generation 2 (live): strangers first, then Alice 9 and Alice 4 (spent)
        filler(&mut node, 2, 300, tag_live);
        let c0 = node.pay(2, &alice, 5, 9, tag_live);
        filler(&mut node, 2, 300, tag_live);
        let c1 = node.pay(2, &alice, 6, 4, tag_live);
        assert_eq!((a0, a1, b0, b1), (0, 1, 0, 6));
        node.spend(0, &alice, a1);
        node.spend(1, &alice, b1);
        node.spend(2, &alice, c1);
        let _ = c0;
        (node, alice, 100 + 20 + 9)
    }

    /// sigil-g3 (2026-10-19): v1 epoch 0 sealed EMPTY at genesis, v7 = epoch 1 with tagged
    /// envelopes; Alice's v7 notes among a stranger's, one spent. The 10.4.38 scanner (v1 only)
    /// read 0 here; it must read exactly the unspent v7 value, fetching envelopes by tag.
    #[test]
    fn g3_shape_the_v7_generation_is_scanned_and_netted() {
        let (alice, bob) = (who(0xA1), who(0xB2));
        let mut node = FakeNode::new(2);
        node.v7_gen = Some(1);
        node.tags = true;
        let p1 = node.pay7(1, &alice, 0, 500);
        for i in 0..20 {
            node.pay7(1, &bob, i, 9_999);
        }
        node.pay7(1, &alice, 1, 70);
        node.spend7(1, &alice, p1);
        let mut s = ShieldedScanner::new("http://fake", alice.seed);
        let snap = cycle(&mut s, &mut node);
        assert_eq!(snap.v7_epoch, Some(1));
        assert_eq!((snap.owned, snap.spent), (2, 1), "both of Alice's v7 notes found, one netted as spent");
        assert_eq!(snap.balance, 70);
        assert_eq!(snap.v7_balance, 70);
        assert_eq!(s.unlocated, 0);
        assert!(snap.view_tags, "envelopes fetched by view tag");
        // second cycle: nothing new, nothing re-read
        let before = node.reads(1).len();
        let again = cycle(&mut s, &mut node);
        assert_eq!(again.balance, 70);
        assert_eq!(node.reads(1).len(), before, "a quiet second cycle fetches no envelopes");
    }

    #[test]
    fn the_balance_is_right_across_rotated_generations_and_the_old_scan_was_not() {
        let (mut node, alice, want) = rotated_chain(false);
        // control: 10.4.37 counts the generation-1 and -2 spends as unspent and cannot place the
        // generation-1 note at a position number generation 0 already used
        let old = old_balance(&node, &alice);
        assert_ne!(old, want, "the control must show the old bug");
        assert_eq!(old, 100 + 50 + 9 + 4, "old: 20 unplaced, 50 and 4 read as unspent");
        let mut s = ShieldedScanner::new("http://fake", alice.seed);
        let snap = cycle(&mut s, &mut node);
        assert_eq!(snap.balance, want);
        assert_eq!((snap.owned, snap.spent), (6, 3));
        assert_eq!(snap.epoch, 2);
        assert_eq!(snap.sealed_epochs, 2);
        assert_eq!(s.unlocated, 0);
    }

    #[test]
    fn a_second_cycle_downloads_only_what_is_new_and_never_a_sealed_generation_again() {
        let (mut node, alice, want) = rotated_chain(false);
        let bob = who(2);
        let mut s = ShieldedScanner::new("http://fake", alice.seed);
        let first = cycle(&mut s, &mut node);
        node.log.clear();
        let quiet = cycle(&mut s, &mut node);
        assert_eq!(quiet.balance, want);
        assert_eq!(node.requests("/v1/shielded/leaves?epoch=0"), 0);
        assert_eq!(node.requests("/v1/shielded/leaves?epoch=1"), 0);
        assert_eq!(node.requests("/v1/shielded/nullifiers"), 0, "count unchanged → not refetched");
        assert!(quiet.fetched_bytes * 50 < first.fetched_bytes, "{} vs {}", quiet.fetched_bytes, first.fetched_bytes);
        // new notes: one for Alice, one for Bob; Bob spends something (the count moves)
        let p = node.pay(2, &alice, 77, 11, false);
        node.pay(2, &bob, 78, 3, false);
        node.spend(2, &bob, 5);
        node.log.clear();
        let next = cycle(&mut s, &mut node);
        assert_eq!(next.balance, want + 11);
        let tail = node.reads(2);
        assert_eq!(tail.len(), 1);
        assert!(tail[0].ends_with(&format!("from={p}")), "asked only for the tail: {tail:?}");
        assert_eq!(node.requests("/v1/shielded/nullifiers"), 1);
    }

    #[test]
    fn view_tags_find_the_same_notes_and_skip_strangers_envelopes() {
        let (mut plain, alice, want) = rotated_chain(true);
        let mut s0 = ShieldedScanner::new("http://fake", alice.seed);
        let a = cycle(&mut s0, &mut plain); // tags off: the node answers 404
        assert!(!a.view_tags);

        let (mut tagged, alice2, _) = rotated_chain(true);
        tagged.tags = true;
        let mut s1 = ShieldedScanner::new("http://fake", alice2.seed);
        let b = cycle(&mut s1, &mut tagged);
        assert!(b.view_tags);
        assert_eq!((a.balance, b.balance), (want, want));
        assert_eq!((a.owned, a.spent), (b.owned, b.spent));
        // Generation 2 (602 notes, all tagged): Alice's two notes plus the strangers that match by
        // chance (600/256 ≈ 2.3 expected) → a handful of 16-blocks, far below the whole
        // generation. 12 blocks would need ≥ 10 chance matches: p ≈ 1e-4.
        let fetched: u64 = tagged
            .reads(2)
            .iter()
            .map(|u| u.split("count=").nth(1).unwrap().parse::<u64>().unwrap())
            .sum();
        assert!(fetched <= 12 * BLOCK, "fetched {fetched} positions of 602");
        assert!(b.fetched_bytes * 2 < a.fetched_bytes, "{} vs {}", b.fetched_bytes, a.fetched_bytes);
        // Untagged generations 0 and 1 are fetched whole either way — nothing can be skipped there.
        assert!(tagged.log.iter().any(|u| u.contains("leaves?epoch=0&from=0&count=")));
    }

    #[test]
    fn a_rotation_between_cycles_carries_on_where_it_stopped() {
        let (mut node, alice, want) = rotated_chain(false);
        let mut s = ShieldedScanner::new("http://fake", alice.seed);
        cycle(&mut s, &mut node);
        // more for Alice in generation 2, then generation 2 seals and 3 opens with another note
        node.pay(2, &alice, 90, 13, false);
        node.gens.push((Vec::new(), Vec::new()));
        node.anchors.push(format!("{:064x}", 4));
        let p = node.pay(3, &alice, 91, 2, false);
        assert_eq!(p, 0);
        node.log.clear();
        let snap = cycle(&mut s, &mut node);
        assert_eq!(snap.balance, want + 13 + 2);
        assert_eq!(snap.epoch, 3);
        assert_eq!(node.requests("/v1/shielded/leaves?epoch=0"), 0);
        assert_eq!(node.reads(2), vec!["http://fake/v1/shielded/leaves?epoch=2&from=602".to_string()]);
        node.log.clear();
        cycle(&mut s, &mut node);
        assert_eq!(node.requests("/v1/shielded/leaves?epoch=2"), 0, "generation 2 is sealed and read");
    }

    #[test]
    fn a_different_chain_behind_the_same_url_starts_over() {
        let (mut node, alice, want) = rotated_chain(false);
        let mut s = ShieldedScanner::new("http://fake", alice.seed);
        assert_eq!(cycle(&mut s, &mut node).balance, want);
        // re-genesis: one generation, one note
        let mut fresh = FakeNode::new(1);
        fresh.pay(0, &alice, 1, 5, false);
        assert_eq!(cycle(&mut s, &mut fresh).balance, 5);
        // back to a rotated chain: its live-generation commitment differs from the one we saw
        // last → this cycle gives no reading, the next starts over
        let (mut other, _, _) = rotated_chain(false);
        other.anchors[0] = format!("{:064x}", 99);
        {
            let mut g = |u: &str| other.get(u);
            assert!(s.cycle(&mut g).is_none());
        }
        assert_eq!(cycle(&mut s, &mut other).balance, want);
        // same notes, but sealed generation 0 now has another permanent anchor → start over
        let (mut third, _, _) = rotated_chain(false);
        third.anchors[0] = format!("{:064x}", 77);
        third.log.clear();
        assert_eq!(cycle(&mut s, &mut third).balance, want);
        assert!(!third.reads(0).is_empty(), "generation 0 was read again");
    }

    #[test]
    fn the_tag_frame_parser_refuses_anything_but_the_exact_layout() {
        let mut f = b"SGT1".to_vec();
        f.extend_from_slice(&2u32.to_le_bytes());
        f.extend_from_slice(&5u64.to_le_bytes());
        f.extend_from_slice(&7u64.to_le_bytes());
        f.extend_from_slice(&2u32.to_le_bytes());
        f.extend_from_slice(&[2u8; 34]);
        f.extend_from_slice(&[1u8; 34]);
        let p = parse_tag_frame(&f).unwrap();
        assert_eq!((p.epoch, p.from, p.total, p.records.len()), (2, 5, 7, 2));
        assert_eq!(p.records[0], TagRecord { kind: 2, vt: 2, eph_pk: [2; 32] });
        assert!(parse_tag_frame(&f[..f.len() - 1]).is_none(), "short");
        let mut long = f.clone();
        long.push(0);
        assert!(parse_tag_frame(&long).is_none(), "long");
        let mut past = f.clone();
        past[16..24].copy_from_slice(&6u64.to_le_bytes());
        assert!(parse_tag_frame(&past).is_none(), "records past the total");
        assert!(parse_tag_frame(b"{\"ok\":true}").is_none());
    }

    /// Live measurement (opt-in): `SIGIL_SCAN_LIVE=http://127.0.0.1:18181`. A throwaway seed with
    /// no notes — it downloads exactly what any wallet downloads, and owns nothing.
    #[test]
    #[ignore]
    fn live_bytes_per_cycle() {
        let url = std::env::var("SIGIL_SCAN_LIVE").expect("SIGIL_SCAN_LIVE");
        let client = reqwest::blocking::Client::builder().timeout(Duration::from_secs(120)).build().unwrap();
        let mut get = http_getter(client);
        let mut s = ShieldedScanner::new(&url, [0xa5; 32]);
        for i in 0..3 {
            let t = Instant::now();
            let snap = s.cycle(&mut get).expect("a reading");
            println!(
                "cycle {i}: {} bytes on the wire, {:.1} s, epoch {}, view tags {}, owned {}",
                snap.fetched_bytes,
                t.elapsed().as_secs_f64(),
                snap.epoch,
                snap.view_tags,
                snap.owned
            );
            if i < 2 {
                std::thread::sleep(Duration::from_secs(45));
            }
        }
    }
}

/// The scanner against the node's REAL routes (`sigil_api::serve`: handlers, compression layer,
/// tag frames, `count`, `ciphertexts=0`), over real HTTP — not the in-memory stand-in above.
#[cfg(all(test, feature = "producer"))]
mod against_the_real_node {
    use super::*;
    use sigil_shield::note_cipher::{seal_note, NotePlaintext};
    use sigil_state::shielded::DENOMINATIONS;
    use sigil_state::{commit_state_transition, SigilState, StateMutation, StateTransition, NATIVE};
    use std::sync::{Arc, RwLock};

    const W: [u8; 32] = [0x77; 32];

    /// Deposits `range` into the pool: position i is Alice's when `i % 397 == 5`, a stranger's
    /// otherwise; every 50th carries no envelope, every 700th a pre-tag (untagged) one.
    fn deposit(state: &RwLock<SigilState>, range: std::ops::Range<u64>, height: u64, first: bool) -> u128 {
        let (alice, bob) = (ShieldedAccount::from_seed([1; 32]), ShieldedAccount::from_seed([2; 32]));
        let d = DENOMINATIONS[0];
        let mut muts = Vec::new();
        if first {
            muts.push(StateMutation::SetMasterWallet { wallet: [0x11; 32] });
        }
        muts.push(StateMutation::SetBalance { wallet: W, token: NATIVE, amount: d * 100_000 });
        let mut alices = 0u128;
        for i in range {
            let (acct, seed) = if i % 397 == 5 { (&alice, [1u8; 32]) } else { (&bob, [2u8; 32]) };
            let value = i + 1;
            let cm = to_wire(acct.note(i, value).unwrap().commitment());
            let ct = seal_note(&NotePlaintext::new(value, acct.blinding(i)), &acct.address(&seed)).unwrap().0;
            let ct = if i % 50 == 49 {
                None
            } else if i % 700 == 699 {
                Some(format!("{}}}", &ct[..ct.len() - 11]))
            } else {
                Some(ct)
            };
            if i % 397 == 5 && ct.is_some() {
                alices += value as u128;
            }
            muts.push(StateMutation::Shield { from: W, amount: d, cm, note_ciphertext: ct });
        }
        commit_state_transition(&mut state.write().unwrap(), &StateTransition { at_height: height, mutations: muts }, height)
            .unwrap();
        alices
    }

    #[test]
    fn the_scanner_reads_the_real_node_routes_with_and_without_view_tags() {
        // This drives the real routes with v1 (64-bit) deposits; the sigil-g3 family has no v1
        // pool (closed at genesis) — its v7 path is `g3_shape_the_v7_generation_is_scanned_and_netted`.
        if sigil_header::IS_G3 {
            return;
        }
        let state = Arc::new(RwLock::new(SigilState::new()));
        let mut want = deposit(&state, 0..2000, 1, true);
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let served = state.clone();
        std::thread::spawn(move || {
            tokio::runtime::Runtime::new().unwrap().block_on(async move {
                let st = sigil_api::AppState::new(Arc::new(sigil_narwhal_mempool::MempoolBackend::legacy()), served);
                sigil_api::serve(&format!("127.0.0.1:{port}"), st).await.unwrap();
            })
        });
        let url = format!("http://127.0.0.1:{port}");
        let client = || reqwest::blocking::Client::builder().timeout(Duration::from_secs(30)).build().unwrap();
        for _ in 0..100 {
            if client().get(format!("{url}/v1/health")).send().is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let mut get = http_getter(client());
        let mut tagged = ShieldedScanner::new(&url, [1; 32]);
        let mut plain = ShieldedScanner::new(&url, [1; 32]);
        plain.tags = Tags::No(Instant::now());

        let a = tagged.cycle(&mut get).expect("tags reading");
        let b = plain.cycle(&mut get).expect("plain reading");
        assert!(a.view_tags && !b.view_tags);
        assert_eq!((a.balance, b.balance), (want, want));
        assert_eq!(a.owned, b.owned);
        assert!(a.owned >= 4, "Alice owns {} notes", a.owned);
        eprintln!("2,000 notes: view tags {} B on the wire, full {} B", a.fetched_bytes, b.fetched_bytes);
        assert!(a.fetched_bytes * 3 < b.fetched_bytes, "{} vs {}", a.fetched_bytes, b.fetched_bytes);

        // New deposits while the node runs (one of them Alice's): both find them, reading only the tail.
        want += deposit(&state, 2000..2400, 2, false);
        let a2 = tagged.cycle(&mut get).expect("tags reading 2");
        let b2 = plain.cycle(&mut get).expect("plain reading 2");
        assert_eq!((a2.balance, b2.balance), (want, want));
        assert!(a2.owned > a.owned);
        assert!(a2.fetched_bytes * 2 < a.fetched_bytes && b2.fetched_bytes * 3 < b.fetched_bytes);
        assert_eq!(tagged.unlocated + plain.unlocated, 0);
    }
}
