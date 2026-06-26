//! Integration: the ACK-paced multi-substream planner converges (delivers every
//! page) both lossless and under ~2% loss via retransmit, using only the public
//! API, and respects per-substream credit windows throughout.

use sigil_serve::{ChunkSink, PageId, ServeStreamPlanner, StreamConfig, StreamError};

struct Link {
    loss_every: u64,
    sent_ok: Vec<(usize, PageId)>,
}
impl Link {
    fn new(loss_every: u64) -> Self {
        Self { loss_every, sent_ok: Vec::new() }
    }
    fn drops(&self, sub: usize, page: PageId) -> bool {
        self.loss_every != 0 && (page.wrapping_mul(31).wrapping_add(sub as u64)) % self.loss_every == 0
    }
}
impl ChunkSink for Link {
    fn send_chunk(&mut self, sub: usize, page_id: PageId) -> Result<(), StreamError> {
        self.sent_ok.push((sub, page_id));
        Ok(())
    }
}

fn assert_windows_respected(p: &ServeStreamPlanner, cfg: &StreamConfig) {
    for sub in 0..cfg.substreams {
        let cap = p.cwnd(sub).floor().max(1.0) as usize;
        assert!(p.inflight(sub) <= cap, "sub {sub}: {} > window {cap}", p.inflight(sub));
    }
}

#[test]
fn converges_lossless() {
    let cfg = StreamConfig::default();
    let mut p = ServeStreamPlanner::new(300, cfg);
    let mut link = Link::new(0);
    let mut guard = 0;
    while !p.is_complete() {
        let sends = p.next_sends();
        assert_windows_respected(&p, &cfg);
        for s in sends {
            link.send_chunk(s.sub, s.page_id).unwrap();
            p.on_ack(s.sub, s.page_id);
        }
        guard += 1;
        assert!(guard < 50_000, "no convergence");
    }
    assert_eq!(p.delivered(), 300);
}

#[test]
fn converges_under_loss_with_retransmit() {
    let cfg = StreamConfig::default();
    let mut p = ServeStreamPlanner::new(400, cfg);
    let mut link = Link::new(50); // ~2%
    let mut guard = 0;
    while !p.is_complete() {
        let sends = p.next_sends();
        assert_windows_respected(&p, &cfg);
        if sends.is_empty() {
            for sub in 0..cfg.substreams {
                if p.inflight(sub) > 0 {
                    p.on_stall(sub);
                }
            }
        }
        for s in sends {
            if link.drops(s.sub, s.page_id) {
                p.on_stall(s.sub);
            } else {
                link.send_chunk(s.sub, s.page_id).unwrap();
                p.on_ack(s.sub, s.page_id);
            }
        }
        guard += 1;
        assert!(guard < 1_000_000, "no convergence under loss");
    }
    assert_eq!(p.delivered(), 400, "all pages delivered despite 2% loss");
}

#[test]
fn single_substream_also_converges() {
    let cfg = StreamConfig { substreams: 1, ..StreamConfig::default() };
    let mut p = ServeStreamPlanner::new(120, cfg);
    let mut link = Link::new(0);
    let mut guard = 0;
    while !p.is_complete() {
        for s in p.next_sends() {
            link.send_chunk(s.sub, s.page_id).unwrap();
            p.on_ack(s.sub, s.page_id);
        }
        guard += 1;
        assert!(guard < 50_000);
    }
    assert_eq!(p.delivered(), 120);
}
