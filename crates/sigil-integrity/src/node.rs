//! One watched node: what it says (via its routes and its event stream) and how it has behaved.
use crate::ledger::Roots;
use serde::Serialize;
use std::io::{BufRead, BufReader};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

pub fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

#[derive(Clone, Debug)]
pub struct NodeSpec {
    pub name: String,
    pub url: String,
}

/// What `/v1/integrity` says, reduced to what two nodes must agree on.
#[derive(Clone, Debug, Serialize)]
pub struct IntegritySample {
    pub applied_height: u64,
    pub finalized_height: Option<u64>,
    pub roots: Roots,
    pub ts_ms: u64,
}

pub fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new().timeout_connect(Duration::from_secs(4)).timeout_read(Duration::from_secs(6)).build()
}

pub fn fetch_integrity(agent: &ureq::Agent, base: &str) -> anyhow::Result<IntegritySample> {
    let v: serde_json::Value = agent.get(&format!("{base}/v1/integrity")).call()?.into_json()?;
    let applied = v.get("applied_height").and_then(|x| x.as_u64()).ok_or_else(|| anyhow::anyhow!("no applied_height"))?;
    let r = v.get("roots").ok_or_else(|| anyhow::anyhow!("no roots"))?;
    let s = |k: &str| r.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
    let sh = v.get("shielded").cloned().unwrap_or(serde_json::Value::Null);
    Ok(IntegritySample {
        applied_height: applied,
        finalized_height: v.get("finalized_height").and_then(|x| x.as_u64()),
        roots: Roots {
            wallet: s("wallet_state_root"),
            contract: s("contract_state_root"),
            dex: s("dex_state_root"),
            event_log: s("event_log_root"),
            native_supply: v.get("native_supply").and_then(|x| x.as_str()).unwrap_or("").to_string(),
            anchor: sh.get("anchor").and_then(|x| x.as_str()).unwrap_or("").to_string(),
            notes: sh.get("notes").and_then(|x| x.as_u64()).unwrap_or(0),
            nullifiers: sh.get("nullifiers").and_then(|x| x.as_u64()).unwrap_or(0),
        },
        ts_ms: now_ms(),
    })
}

/// `/v1/dagknight/recent` → (height, block hash hex) for the blocks this node holds. The route returns the
/// hash as a byte array; a follower that lags the producer by hundreds of blocks is compared through these.
pub fn fetch_recent_blocks(agent: &ureq::Agent, base: &str) -> anyhow::Result<Vec<(u64, String)>> {
    let v: serde_json::Value = agent.get(&format!("{base}/v1/dagknight/recent")).call()?.into_json()?;
    let d = v.get("data").unwrap_or(&v);
    let blocks = d.get("blocks").and_then(|b| b.as_array()).ok_or_else(|| anyhow::anyhow!("no blocks"))?;
    let mut out = Vec::with_capacity(blocks.len());
    for b in blocks {
        let h = b.get("height").and_then(|x| x.as_u64());
        let hash = b.get("hash").and_then(|x| x.as_array()).map(|a| a.iter().filter_map(|n| n.as_u64()).map(|n| n as u8).collect::<Vec<u8>>());
        if let (Some(h), Some(bytes)) = (h, hash) {
            if bytes.len() == 32 {
                out.push((h, hex::encode(bytes)));
            }
        }
    }
    Ok(out)
}

/// A certificate seen on the node's own event stream (SSE) or delivered by its webhook.
#[derive(Clone, Debug)]
#[allow(dead_code)] // votes/ts_ms are kept for the status line and future vote-count alarms
pub struct CertEvent {
    pub height: u64,
    pub spine_block_hash: String,
    pub votes: u64,
    pub ts_ms: u64,
}

pub fn parse_event_json(s: &str) -> Option<CertEvent> {
    let v: serde_json::Value = serde_json::from_str(s).ok()?;
    if v.get("kind").and_then(|k| k.as_str()) != Some("certificate") {
        return None;
    }
    Some(CertEvent {
        height: v.get("height")?.as_u64()?,
        spine_block_hash: v.get("spine_block_hash")?.as_str()?.to_string(),
        votes: v.get("votes").and_then(|x| x.as_u64()).unwrap_or(0),
        ts_ms: v.get("ts_ms").and_then(|x| x.as_u64()).unwrap_or_else(now_ms),
    })
}

/// Follow `GET /v1/events?kinds=certificate` and hand every certificate to `on_cert`. Returns when the
/// stream ends or errors; the caller loops with a back-off. The node keeps the stream alive with a
/// 15 s comment, so a read timeout of 30 s distinguishes "quiet" from "gone".
pub fn follow_sse(base: &str, on_cert: &mut dyn FnMut(CertEvent), last_seen_ms: &Arc<AtomicU64>) -> anyhow::Result<()> {
    let agent = ureq::AgentBuilder::new().timeout_connect(Duration::from_secs(5)).timeout_read(Duration::from_secs(30)).build();
    let resp = agent.get(&format!("{base}/v1/events?kinds=certificate")).call()?;
    let reader = BufReader::new(resp.into_reader());
    for line in reader.lines() {
        let line = line?;
        last_seen_ms.store(now_ms(), Ordering::Relaxed);
        if let Some(data) = line.strip_prefix("data:") {
            if let Some(c) = parse_event_json(data.trim()) {
                on_cert(c);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_certificate_line_and_ignores_tips() {
        let c = parse_event_json(r#"{"seq":1,"kind":"certificate","height":20040791,"spine_block_hash":"e1c2","votes":2,"committee_size":2,"quorum":2,"ts_ms":5}"#).unwrap();
        assert_eq!((c.height, c.spine_block_hash.as_str(), c.votes, c.ts_ms), (20040791, "e1c2", 2, 5));
        assert!(parse_event_json(r#"{"seq":2,"kind":"tip","height":7,"ts_ms":1}"#).is_none());
    }
}
