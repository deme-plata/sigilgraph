//! # sigil-knot — what the knot of the SIGIL DAG says (QTFT-2.1)
//!
//! Each producer is a strand. Every time one producer builds on another's
//! block (spine parent or merge parent) the two strands get one full twist
//! (see [`sigil_dagknight::present::ReferenceBraid`]). Close the braid and you
//! get a link with one loop per producer. Its invariants answer a plain
//! question — *do the producers actually see each other?*
//!
//! | what the link is | what it means on the chain |
//! |---|---|
//! | one loop (unknot) | one producer in the window — nothing to braid (g2 today) |
//! | split (loops pull apart, Δ = 0) | groups of producers that never built on each other |
//! | linked, one-way | A builds on B, B never builds on A — a one-way gossip break |
//! | linked, mutual | every pair that links references each other both ways |
//!
//! Proven, not plausible: `lk(s,t)` of the closure equals the number of cross
//! references between `s` and `t` (tested), a split link has Δ = 0 (a theorem),
//! so Δ ≠ 0 certifies the producers cannot be pulled apart. Direction is NOT a
//! topological fact here (a full twist is symmetric); it is a plain count.
//!
//! Analysis only. The header's `topology_commitment` still uses the V1
//! convention, which is blind to two producers; this crate reports both.

#![forbid(unsafe_code)]

use flux_topology::{alexander_poly, linking_matrix, writhe, BraidWord, LaurentPoly};
use serde::Serialize;
use serde_json::Value;
use sigil_dagknight::present::ReferenceBraid;
use sigil_dagknight::{BlockHash, BlockView, Braid, BraidConfig};

/// `sigil-node::dag::TOPOLOGY_COMMITMENT_WINDOW` — the window the header
/// commits over. Mirrored (not imported) to keep this crate off sigil-node.
pub const COMMITMENT_WINDOW: u64 = 32;

/// `flux-topology`'s exact Alexander path is sized for small strand counts.
pub const MAX_EXACT_STRANDS: u32 = 8;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Verdict {
    Empty,
    Unknot,
    Split { groups: Vec<Vec<usize>> },
    OneWay { pairs: Vec<(usize, usize)> },
    Braided,
}

#[derive(Debug, Clone, Serialize)]
pub struct KnotReport {
    pub from_height: u64,
    pub to_height: u64,
    pub heights_resident: usize,
    pub strands: u32,
    /// First 8 bytes of each producer id, hex — strand index order.
    pub producers: Vec<String>,
    /// Cross references in the window (= full twists).
    pub references: u32,
    pub crossings: usize,
    pub writhe: i64,
    pub linking: Vec<Vec<i64>>,
    /// `directed[s][t]` = s built on t.
    pub directed: Vec<Vec<u32>>,
    /// Canonical Δ(t), or why it was not computed.
    pub alexander: Result<String, String>,
    /// Connected groups of the "lk ≠ 0" graph.
    pub groups: Vec<Vec<usize>>,
    /// `(s, t)`: s built on t, t never built on s.
    pub one_way: Vec<(usize, usize)>,
    /// Length of the V1 word over the same window (the committed convention).
    pub v1_word_len: usize,
    pub verdict: Verdict,
}

/// Δ(t) with the checked i128 arithmetic's overflow panic turned into an
/// honest refusal instead of a crash.
pub fn alexander_checked(w: &BraidWord) -> Result<LaurentPoly, String> {
    if w.strands > MAX_EXACT_STRANDS {
        return Err(format!("not computed: {} strands > {MAX_EXACT_STRANDS} (v0 exact limit)", w.strands));
    }
    std::panic::catch_unwind(|| alexander_poly(w)).map_err(|e| {
        let msg = e
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| e.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "panic".into());
        format!("not computed: {msg}")
    })
}

fn groups_of(lk: &[Vec<i64>]) -> Vec<Vec<usize>> {
    let n = lk.len();
    let mut seen = vec![false; n];
    let mut out = Vec::new();
    for start in 0..n {
        if seen[start] {
            continue;
        }
        let mut group = Vec::new();
        let mut stack = vec![start];
        seen[start] = true;
        while let Some(s) = stack.pop() {
            group.push(s);
            for t in 0..n {
                if !seen[t] && lk[s][t] != 0 {
                    seen[t] = true;
                    stack.push(t);
                }
            }
        }
        group.sort_unstable();
        out.push(group);
    }
    out
}

/// Analyze one window: the reference braid plus the V1 word length for comparison.
pub fn analyze(rb: &ReferenceBraid, v1_word_len: usize, from_height: u64, to_height: u64) -> KnotReport {
    let w = BraidWord::new(rb.strands.max(1), rb.word.clone());
    let n = rb.strands as usize;
    let linking = if n == 0 { Vec::new() } else { linking_matrix(&w) };
    let groups = groups_of(&linking);
    let mut one_way = Vec::new();
    for s in 0..n {
        for t in 0..n {
            if rb.directed[s][t] > 0 && rb.directed[t][s] == 0 {
                one_way.push((s, t));
            }
        }
    }
    let alexander = match n {
        0 => Err("empty window".into()),
        _ => alexander_checked(&w).map(|d| d.to_string()),
    };
    let verdict = match n {
        0 => Verdict::Empty,
        1 => Verdict::Unknot,
        _ if groups.len() > 1 => Verdict::Split { groups: groups.clone() },
        _ if !one_way.is_empty() => Verdict::OneWay { pairs: one_way.clone() },
        _ => Verdict::Braided,
    };
    KnotReport {
        from_height,
        to_height,
        heights_resident: rb.heights_resident,
        strands: rb.strands,
        producers: rb.producers.iter().map(|p| p[..8].iter().map(|b| format!("{b:02x}")).collect()).collect(),
        references: rb.directed.iter().flatten().sum(),
        crossings: rb.word.len(),
        writhe: if n == 0 { 0 } else { writhe(&w) },
        linking,
        directed: rb.directed.clone(),
        alexander,
        groups,
        one_way,
        v1_word_len,
        verdict,
    }
}

/// Analyze `[from, to]` of a live braid.
pub fn analyze_braid(b: &Braid, from_height: u64, to_height: u64) -> KnotReport {
    let rb = b.reference_braid(from_height, to_height);
    let v1 = b.braid_word(from_height, to_height).word.len();
    analyze(&rb, v1, from_height, to_height)
}

fn hash32(v: &Value) -> Result<BlockHash, String> {
    match v {
        Value::Array(a) if a.len() == 32 => {
            let mut out = [0u8; 32];
            for (i, x) in a.iter().enumerate() {
                out[i] = x.as_u64().filter(|b| *b <= 255).ok_or("hash byte out of range")? as u8;
            }
            Ok(out)
        }
        Value::String(s) if s.len() == 64 => {
            let mut out = [0u8; 32];
            for i in 0..32 {
                out[i] = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).map_err(|e| e.to_string())?;
            }
            Ok(out)
        }
        _ => Err(format!("expected a 32-byte array or 64-hex string, got {v}")),
    }
}

/// Parse `GET /v1/dagknight/recent` (`{ok, data: {blocks: [...]}}`), or a bare
/// `blocks` array, into views sorted by height.
pub fn views_from_snapshot(json: &Value) -> Result<Vec<BlockView>, String> {
    let blocks = json
        .pointer("/data/blocks")
        .or_else(|| json.get("blocks"))
        .unwrap_or(json)
        .as_array()
        .ok_or("no blocks array (expected /v1/dagknight/recent JSON)")?;
    let mut views = Vec::with_capacity(blocks.len());
    for b in blocks {
        let merge_parents = match b.get("merge_parents") {
            Some(Value::Array(a)) => a.iter().map(hash32).collect::<Result<Vec<_>, _>>()?,
            _ => Vec::new(),
        };
        views.push(BlockView {
            hash: hash32(b.get("hash").ok_or("block without hash")?)?,
            parent: hash32(b.get("parent").ok_or("block without parent")?)?,
            merge_parents,
            height: b.get("height").and_then(Value::as_u64).ok_or("block without height")?,
            producer: hash32(b.get("producer").ok_or("block without producer")?)?,
            difficulty: 0,
        });
    }
    views.sort_by_key(|v| (v.height, v.producer, v.hash));
    Ok(views)
}

/// Rebuild a braid from a snapshot window. Parents outside the snapshot are
/// anchored as trusted pre-window history (they never enter the order and
/// contribute no twists). Returns the braid and the snapshot's height range.
pub fn braid_from_views(views: &[BlockView]) -> Result<(Braid, u64, u64), String> {
    let first = views.first().ok_or("empty snapshot")?;
    let lo = first.height;
    let hi = views.last().map(|v| v.height).unwrap_or(lo);
    let known: std::collections::HashSet<BlockHash> = views.iter().map(|v| v.hash).collect();
    let mut b = Braid::new_with_base(BraidConfig::default(), first.parent, lo.saturating_sub(1));
    for v in views {
        for p in std::iter::once(&v.parent).chain(v.merge_parents.iter()) {
            if !known.contains(p) {
                b.anchor_trusted(*p, v.height.saturating_sub(1));
            }
        }
    }
    for v in views {
        b.insert(v.clone());
    }
    Ok((b, lo, hi))
}

fn short(p: &str) -> &str {
    &p[..p.len().min(8)]
}

/// Plain-language rendering for the terminal.
pub fn render(r: &KnotReport, expect: Option<u32>) -> String {
    let mut o = String::new();
    let name = |i: usize| format!("{}·{}", (b'A' + i as u8) as char, short(&r.producers[i]));
    o += &format!(
        "window   heights {}..={} ({} resident)\nstrands  {} producer(s){}\n",
        r.from_height,
        r.to_height,
        r.heights_resident,
        r.strands,
        if r.strands > 0 {
            format!(": {}", (0..r.strands as usize).map(name).collect::<Vec<_>>().join("  "))
        } else {
            String::new()
        }
    );
    o += &format!(
        "braid    {} cross reference(s) → {} crossings, writhe {}\n",
        r.references, r.crossings, r.writhe
    );
    match &r.alexander {
        Ok(d) => o += &format!("Δ(t)     {d}\n"),
        Err(e) => o += &format!("Δ(t)     {e}\n"),
    }
    let n = r.strands as usize;
    if n >= 2 {
        o += "linking  ";
        for s in 0..n {
            for t in s + 1..n {
                o += &format!(
                    "lk({},{})={} [{}→{} {}, {}→{} {}]  ",
                    (b'A' + s as u8) as char,
                    (b'A' + t as u8) as char,
                    r.linking[s][t],
                    (b'A' + s as u8) as char,
                    (b'A' + t as u8) as char,
                    r.directed[s][t],
                    (b'A' + t as u8) as char,
                    (b'A' + s as u8) as char,
                    r.directed[t][s]
                );
            }
        }
        o += "\n";
    }
    o += &format!("v1 word  {} generator(s) (the convention the header commits today)\n", r.v1_word_len);
    let letter = |i: usize| ((b'A' + i as u8) as char).to_string();
    let verdict = match &r.verdict {
        Verdict::Empty => "EMPTY — no blocks in the window.".to_string(),
        Verdict::Unknot => "UNKNOT — one producer, one strand. Nothing to braid; every knot invariant is trivial.".to_string(),
        Verdict::Split { groups } => format!(
            "SPLIT — {} groups never built on each other here: {}. The loops pull apart (Δ = 0).",
            groups.len(),
            groups.iter().map(|g| g.iter().map(|&i| letter(i)).collect::<String>()).collect::<Vec<_>>().join(" | ")
        ),
        Verdict::OneWay { pairs } => format!(
            "ONE-WAY — linked, but {}. One side is not receiving the other's blocks.",
            pairs
                .iter()
                .map(|(s, t)| format!("{} builds on {}, {} never on {}", letter(*s), letter(*t), letter(*t), letter(*s)))
                .collect::<Vec<_>>()
                .join("; ")
        ),
        Verdict::Braided => "BRAIDED — every producer is linked to the rest, and every link goes both ways.".to_string(),
    };
    o += &format!("verdict  {verdict}\n");
    if let Some(e) = expect {
        if r.strands < e {
            o += &format!(
                "⚠ expected {e} producers, only {} strand(s) in the finalized order: the others' blocks were never merged in this window (partitioned, stopped, or behind).\n",
                r.strands
            );
        }
    }
    o
}

/// Synthetic DAGs for the four shapes, so the verdicts can be seen without a
/// multi-producer chain. Returns `(name, braid, from, to)`.
pub fn demo_scenarios() -> Vec<(&'static str, Braid, u64, u64)> {
    let pa = [0xA0u8; 32];
    let pb = [0xB0u8; 32];
    let pc = [0xC0u8; 32];
    let h = |n: u8| -> BlockHash { [n.wrapping_add(1); 32] };
    let v = |hash, parent, merge_parents, height, producer| BlockView {
        hash,
        parent,
        merge_parents,
        height,
        producer,
        difficulty: 0,
    };
    let feed = |views: Vec<BlockView>| {
        let mut b = Braid::new(BraidConfig::default());
        for x in views {
            b.insert(x);
        }
        b
    };
    let mut out = Vec::new();

    // 1. g2 today: one producer.
    out.push((
        "one producer (g2 today)",
        feed((0..12u8).map(|k| v(h(k), if k == 0 { [0; 32] } else { h(k - 1) }, vec![], k as u64, pa)).collect()),
        0,
        11,
    ));

    // 2. Two producers alternating on one spine, plus a parallel pair merged back.
    let mut healthy: Vec<BlockView> = (0..10u8)
        .map(|k| v(h(k), if k == 0 { [0; 32] } else { h(k - 1) }, vec![], k as u64, if k % 2 == 0 { pa } else { pb }))
        .collect();
    healthy.push(v(h(10), h(9), vec![], 10, pa));
    healthy.push(v(h(11), h(9), vec![], 10, pb));
    healthy.push(v(h(12), h(11), vec![h(10)], 11, pa));
    out.push(("two producers, healthy", feed(healthy), 0, 11));

    // 3. One-way: A merges B's blocks, B only ever builds on itself.
    let mut oneway = vec![v(h(0), [0; 32], vec![], 0, pa)];
    for k in 1..=6u8 {
        let (a_prev, b_prev) = if k == 1 { (h(0), h(0)) } else { (h(2 * k - 3), h(2 * k - 2)) };
        oneway.push(v(h(2 * k - 1), a_prev, if k == 1 { vec![] } else { vec![b_prev] }, k as u64, pa));
        oneway.push(v(h(2 * k), b_prev, vec![], k as u64, pb));
    }
    oneway.push(v(h(13), h(11), vec![h(12)], 7, pa));
    out.push(("two producers, one-way gossip", feed(oneway), 1, 7));

    // 4. Three producers, C cut off after the shared start: A and B braid, C
    //    keeps a merged-but-never-merging relationship only through history
    //    outside the window → the window splits {A,B} | {C}.
    let mut split = vec![v(h(0), [0; 32], vec![], 0, pc)];
    for k in 1..=8u8 {
        split.push(v(h(k), h(k - 1), vec![], k as u64, if k % 2 == 1 { pa } else { pb }));
    }
    for k in 0..4u8 {
        let parent = if k == 0 { h(0) } else { h(20 + k - 1) };
        split.push(v(h(20 + k), parent, vec![], (k + 1) as u64, pc));
    }
    split.push(v(h(30), h(8), vec![h(23)], 9, pa));
    out.push(("three producers, C partitioned", feed(split), 2, 8));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linking_equals_cross_reference_counts_on_random_dags() {
        // The proven claim this crate leans on: lk(s,t) of the reference
        // braid's closure = directed[s][t] + directed[t][s].
        let mut seed = 0x5161_6C17u64;
        let mut rnd = |m: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % m
        };
        for _ in 0..40 {
            let producers = [[0x10u8; 32], [0x20u8; 32], [0x30u8; 32], [0x40u8; 32]];
            let np = 2 + rnd(3) as usize;
            let mut views: Vec<BlockView> = vec![BlockView {
                hash: [1; 32],
                parent: [0; 32],
                merge_parents: vec![],
                height: 0,
                producer: producers[0],
                difficulty: 0,
            }];
            let mut by_height: Vec<Vec<BlockHash>> = vec![vec![[1; 32]]];
            let mut id = 2u32;
            for height in 1..14u64 {
                let width = 1 + rnd(2) as usize;
                let prev = by_height[height as usize - 1].clone();
                let mut row = Vec::new();
                for k in 0..width {
                    let mut hash = [0u8; 32];
                    hash[..4].copy_from_slice(&id.to_le_bytes());
                    hash[31] = 0xEE;
                    id += 1;
                    let parent = prev[k % prev.len()];
                    let merge_parents: Vec<BlockHash> = prev.iter().copied().filter(|p| *p != parent).collect();
                    views.push(BlockView {
                        hash,
                        parent,
                        merge_parents,
                        height,
                        producer: producers[rnd(np as u64) as usize],
                        difficulty: 0,
                    });
                    row.push(hash);
                }
                by_height.push(row);
            }
            let mut b = Braid::new(BraidConfig::default());
            for x in &views {
                b.insert(x.clone());
            }
            let rb = b.reference_braid(2, 13);
            if rb.strands < 2 {
                continue;
            }
            let lk = linking_matrix(&BraidWord::new(rb.strands, rb.word.clone()));
            for s in 0..rb.strands as usize {
                for t in 0..rb.strands as usize {
                    if s != t {
                        assert_eq!(lk[s][t], (rb.directed[s][t] + rb.directed[t][s]) as i64);
                    }
                }
            }
            assert_eq!(
                flux_topology::permutation(&BraidWord::new(rb.strands, rb.word.clone())),
                (0..rb.strands as usize).collect::<Vec<_>>(),
                "a reference braid is pure"
            );
        }
    }

    #[test]
    fn demo_verdicts() {
        let got: Vec<(String, Verdict)> = demo_scenarios()
            .into_iter()
            .map(|(name, b, from, to)| (name.to_string(), analyze_braid(&b, from, to).verdict))
            .collect();
        assert_eq!(got[0].1, Verdict::Unknot);
        assert_eq!(got[1].1, Verdict::Braided);
        assert!(matches!(&got[2].1, Verdict::OneWay { pairs } if pairs.len() == 1), "{:?}", got[2]);
        assert!(matches!(&got[3].1, Verdict::Split { groups } if groups.len() == 2), "{:?}", got[3]);
    }

    #[test]
    fn healthy_two_producers_is_a_torus_link_and_v1_is_blind() {
        let (_, b, from, to) = demo_scenarios().swap_remove(1);
        let r = analyze_braid(&b, from, to);
        assert_eq!(r.v1_word_len, 0, "V1 sees nothing with two producers");
        assert_eq!(r.strands, 2);
        assert!(r.linking[0][1] > 0);
        // σ1^(2k) closes to the torus link T(2,2k); its Δ is nonzero.
        let d = r.alexander.clone().unwrap();
        assert_ne!(d, "0", "linked → Δ ≠ 0 here");
    }

    #[test]
    fn split_link_alexander_vanishes() {
        // The theorem the SPLIT verdict cites: an unlinked pair has Δ = 0.
        assert!(alexander_checked(&BraidWord::new(2, vec![])).unwrap().is_zero());
        let (_, b, from, to) = demo_scenarios().swap_remove(3);
        assert_eq!(analyze_braid(&b, from, to).alexander, Ok("0".into()));
    }

    #[test]
    fn parses_the_live_snapshot_shape() {
        let a: Vec<u8> = vec![7; 32];
        let json = serde_json::json!({"ok": true, "data": {"blocks": [
            {"hash": vec![2u8; 32], "parent": a, "merge_parents": [], "height": 5, "producer": vec![9u8; 32], "blue_score": 5, "is_blue": true},
            {"hash": vec![3u8; 32], "parent": vec![2u8; 32], "merge_parents": [], "height": 6, "producer": vec![9u8; 32]}
        ]}, "k": null});
        let views = views_from_snapshot(&json).unwrap();
        assert_eq!(views.len(), 2);
        let (b, lo, hi) = braid_from_views(&views).unwrap();
        assert_eq!((lo, hi), (5, 6));
        assert_eq!(analyze_braid(&b, lo, hi).verdict, Verdict::Unknot);
        assert_eq!(analyze_braid(&b, lo, hi).heights_resident, 2);
    }
}
