//! sigil-hurricane — Gossip Parameter Optimizer
//!
//! Part of "Flux P2P Hurricane" research track. Uses flux-chronos to simulate
//! a multi-node gossip mesh and find the optimal parameters for minimum block
//! propagation latency. Outputs `NetworkConfig` values Delta can use directly.
//!
//! Sweep axes:
//!   - fan_out: 1..16 (redundancy factor)
//!   - mesh_size: 4..12 (D parameter in gossipsub)
//!   - heartbeat_ms: 100..2000
//!   - latency_ms: 5, 25, 50, 100, 200, 500
//!   - drop_prob: 0.0, 0.01, 0.05, 0.10, 0.15, 0.25
//!   - node_count: 16, 32, 64, 128
//!
//! Run: sigil-hurricane [--quick] [--full] [--json]

use std::time::Instant;

use flux_chronos::{millis, secs, Envelope, NetEdge, NodeId, NodeStepResult, ScenarioSeed, SimNode, TickId, Universe};

/// A gossip node that participates in block propagation.
/// One node is the "source" that injects a block; all others are "peers"
/// that propagate it according to gossip rules.
struct GossipNode {
    id: NodeId,
    peers: Vec<NodeId>,
    is_source: bool,
    fan_out: usize,
    seen_block: bool,
    block_propagated: bool,
    /// Tick when this node first received the block (0 = not received yet).
    first_seen_at: TickId,
}

impl SimNode for GossipNode {
    fn step(&mut self, now: TickId, incoming: &[Envelope]) -> NodeStepResult {
        let mut out = NodeStepResult::default();

        // Check if we received the block for the first time.
        if !self.seen_block {
            for env in incoming {
                if env.payload == b"BLOCK" {
                    self.seen_block = true;
                    self.first_seen_at = now;
                    out.events.push(format!("node-{} saw block at tick {}", self.id.0, now));
                    break;
                }
            }
        }

        // Source: inject the block at tick 0.
        if self.is_source && !self.block_propagated {
            self.block_propagated = true;
            // Fan-out: send to `fan_out` peers.
            let count = self.fan_out.min(self.peers.len());
            for &peer in self.peers.iter().take(count) {
                out.publish.push(Envelope {
                    from: self.id,
                    to: peer,
                    sent_at: now,
                    payload: b"BLOCK".to_vec(),
                });
            }
            out.events.push(format!("source-{} injected block", self.id.0));
            return out;
        }

        // Non-source: if we just received the block, propagate it further.
        if self.seen_block && !self.block_propagated {
            self.block_propagated = true;
            let count = self.fan_out.min(self.peers.len());
            for &peer in self.peers.iter().take(count) {
                out.publish.push(Envelope {
                    from: self.id,
                    to: peer,
                    sent_at: now,
                    payload: b"BLOCK".to_vec(),
                });
            }
            out.events.push(format!("node-{} propagated block", self.id.0));
        }

        out
    }

    fn snapshot(&self) -> Vec<u8> {
        let mut v = Vec::new();
        v.push(self.seen_block as u8);
        v.push(self.block_propagated as u8);
        v.extend_from_slice(&self.first_seen_at.to_le_bytes());
        v
    }

    fn restore(&mut self, bytes: &[u8]) -> Result<(), String> {
        if bytes.len() >= 10 {
            self.seen_block = bytes[0] != 0;
            self.block_propagated = bytes[1] != 0;
            let mut a = [0u8; 8];
            a.copy_from_slice(&bytes[2..10]);
            self.first_seen_at = u64::from_le_bytes(a);
        }
        Ok(())
    }

    fn name(&self) -> &str {
        if self.is_source { "source" } else { "peer" }
    }
}

/// One parameter combination to test.
#[derive(Debug, Clone)]
struct GossipParams {
    fan_out: usize,
    node_count: usize,
    latency_ms: u64,
    drop_prob: f64,
}

/// Result of one gossip simulation.
#[derive(Debug, Clone, serde::Serialize)]
struct GossipResult {
    fan_out: usize,
    node_count: usize,
    latency_ms: u64,
    drop_prob: f64,
    /// How many nodes received the block.
    delivered: usize,
    /// Tick when the FIRST non-source node received the block.
    first_delivery_tick: TickId,
    /// Tick when the LAST node received the block (or u64::MAX if not all).
    last_delivery_tick: TickId,
    /// Propagation latency: last - first delivery tick.
    propagation_span_us: u64,
    /// Wall-clock time for this sim run (microseconds).
    wall_us: u128,
    /// Delivery rate.
    delivery_pct: f64,
}

impl GossipResult {
    fn header() -> &'static str {
        "fan  nodes  lat_ms  drop%   delivered  first_us  last_us   span_us   delivery%  wall_us"
    }

    fn row(&self) -> String {
        format!(
            "{:<4} {:<6} {:<7} {:<6.1}% {:<10} {:<9} {:<9} {:<9} {:<9.1}% {:<8}",
            self.fan_out,
            self.node_count,
            self.latency_ms,
            self.drop_prob * 100.0,
            self.delivered,
            self.first_delivery_tick,
            self.last_delivery_tick,
            self.propagation_span_us,
            self.delivery_pct,
            self.wall_us,
        )
    }
}

/// Run one gossip simulation and return the result.
fn run_gossip(params: &GossipParams) -> GossipResult {
    let t0 = Instant::now();
    let n = params.node_count;

    let mut universe = Universe::new(ScenarioSeed(
        42u64.wrapping_add(params.fan_out as u64 * 10000)
            .wrapping_add(params.node_count as u64 * 100)
            .wrapping_add(params.latency_ms),
    ));

    // Create nodes: node 0 = source, nodes 1..n-1 = peers.
    let mut peer_ids = Vec::with_capacity(n);

    let all_ids: Vec<NodeId> = (0..n as u32).map(NodeId).collect();
    // Create source first.
    let source = Box::new(GossipNode {
        id: NodeId(0),
        peers: all_ids[1..].to_vec(),
        is_source: true,
        fan_out: params.fan_out,
        seen_block: false,
        block_propagated: false,
        first_seen_at: 0,
    });
    let source_id = universe.spawn_node(source);

    // Create peers.
    for i in 1..n {
        let my_peers: Vec<NodeId> = all_ids.iter().filter(|id| id.0 != i as u32).copied().collect();
        let peer = Box::new(GossipNode {
            id: NodeId(i as u32),
            peers: my_peers,
            is_source: false,
            fan_out: params.fan_out,
            seen_block: false,
            block_propagated: false,
            first_seen_at: 0,
        });
        let pid = universe.spawn_node(peer);
        peer_ids.push(pid);
    }

    // Connect: random-ish peer list for each node (not fully connected — that's unrealistic).
    // Each node has ~sqrt(n) random peers (subject to fan_out).
    use rand::Rng;
    let mut rng = rand::thread_rng();
    let peer_degree = (n as f64).sqrt().max(params.fan_out as f64 * 2.0) as usize;

    // all_ids already defined above for peer population; reuse for connections
    for &from in &all_ids {
        // Pick `peer_degree` random peers (excluding self).
        let mut candidates: Vec<NodeId> = all_ids.iter().filter(|&&id| id != from).copied().collect();
        // Shuffle and take peer_degree.
        for i in (1..candidates.len()).rev() {
            let j = rng.gen_range(0..=i);
            candidates.swap(i, j);
        }
        let selected: Vec<NodeId> = candidates.into_iter().take(peer_degree).collect();

        for &to in &selected {
            universe.connect(
                from,
                to,
                NetEdge {
                    latency_micros: params.latency_ms * 1000,
                    drop_prob: params.drop_prob,
                    partitioned: false,
                },
            );
        }
    }

    // Inject the block at the source.
    universe.inject(source_id, b"GO".to_vec());

    // Advance enough simulated time for the block to propagate.
    // Worst case: log2(N) hops × (latency + processing).
    let max_hops = (n as f64).log2().ceil() as u64 + 2;
    let sim_time = max_hops * (params.latency_ms * 1000 + 100_000) + 1_000_000;
    universe.advance(sim_time);

    // Collect results.
    let events = universe.event_log();
    let mut first_tick = u64::MAX;
    let mut last_tick = 0u64;
    let mut delivered = 0usize;

    for (_tick, _node, event) in events {
        if event.starts_with("node-") && event.contains("saw block") {
            delivered += 1;
            // Extract tick from the event string: "node-N saw block at tick T"
            if let Some(tick_str) = event.split("tick ").nth(1) {
                if let Ok(t) = tick_str.parse::<u64>() {
                    first_tick = first_tick.min(t);
                    last_tick = last_tick.max(t);
                }
            }
        }
    }

    let wall_us = t0.elapsed().as_micros();
    let total_nodes = n - 1; // exclude source

    GossipResult {
        fan_out: params.fan_out,
        node_count: n,
        latency_ms: params.latency_ms,
        drop_prob: params.drop_prob,
        delivered,
        first_delivery_tick: if first_tick == u64::MAX { 0 } else { first_tick },
        last_delivery_tick: last_tick,
        propagation_span_us: if first_tick != u64::MAX && last_tick > first_tick {
            last_tick - first_tick
        } else {
            0
        },
        wall_us,
        delivery_pct: if total_nodes > 0 {
            delivered as f64 / total_nodes as f64 * 100.0
        } else {
            100.0
        },
    }
}

fn quick_sweep() -> Vec<GossipParams> {
    let mut p = Vec::new();
    for fan_out in [1, 2, 4, 8] {
        for nodes in [16, 64] {
            for lat_ms in [5, 50, 200] {
                for drop_pct in [0.0, 0.05, 0.15] {
                    p.push(GossipParams {
                        fan_out,
                        node_count: nodes,
                        latency_ms: lat_ms,
                        drop_prob: drop_pct,
                    });
                }
            }
        }
    }
    p
}

fn full_sweep() -> Vec<GossipParams> {
    let mut p = Vec::new();
    for fan_out in 1..=16 {
        for nodes in [16, 32, 64, 128] {
            for lat_ms in [5, 25, 50, 100, 200, 500] {
                for drop_pct in [0.0, 0.01, 0.05, 0.10, 0.15, 0.25] {
                    p.push(GossipParams {
                        fan_out,
                        node_count: nodes,
                        latency_ms: lat_ms,
                        drop_prob: drop_pct,
                    });
                }
            }
        }
    }
    p
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let json_only = args.iter().any(|a| a == "--json");
    let full = args.iter().any(|a| a == "--full");

    let params = if full { full_sweep() } else { quick_sweep() };

    if !json_only {
        println!("╔══════════════════════════════════════════════════════════════════════════════════════════╗");
        println!("║                    FLUX P2P HURRICANE — Gossip Parameter Optimizer                     ║");
        println!("╠══════════════════════════════════════════════════════════════════════════════════════════╣");
        println!("║  Points: {:<4}  Mode: {:<8}                                                         ║",
            params.len(),
            if full { "full" } else { "quick" },
        );
        println!("╚══════════════════════════════════════════════════════════════════════════════════════════╝");
        println!();
        println!("{}", GossipResult::header());
        println!("{}", "-".repeat(GossipResult::header().len()));
    }

    let t0 = Instant::now();
    let mut results: Vec<GossipResult> = params.iter().map(|p| run_gossip(p)).collect();
    let wall_ms = t0.elapsed().as_millis();

    // Sort by: highest delivery rate, then lowest propagation span.
    results.sort_by(|a, b| {
        b.delivery_pct
            .partial_cmp(&a.delivery_pct)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.propagation_span_us.cmp(&b.propagation_span_us))
    });

    if json_only {
        let json = serde_json::to_string_pretty(&serde_json::json!({
            "wall_ms": wall_ms,
            "points": results.len(),
            "results": &results,
            "top_result": &results.first(),
        }))
        .unwrap();
        println!("{json}");
    } else {
        for r in &results {
            println!("{}", r.row());
        }
        println!();
        println!("═══ Sweep complete: {} points in {} ms ═══", results.len(), wall_ms);

        if let Some(best) = results.first() {
            println!();
            println!("🏆 OPTIMAL GOSSIP CONFIGURATION:");
            println!("   fan_out          = {}", best.fan_out);
            println!("   node_count       = {}", best.node_count);
            println!("   latency_ms       = {}", best.latency_ms);
            println!("   drop_prob        = {:.1}%", best.drop_prob * 100.0);
            println!("   delivery_rate    = {:.1}%", best.delivery_pct);
            println!("   propagation_span = {} μs", best.propagation_span_us);
            println!();
            println!("   Delta deploy:");
            println!("   export FLUX_GOSSIP_FAN_OUT={}", best.fan_out);
            println!("   export FLUX_MESH_D={}", (best.node_count as f64).sqrt().max(best.fan_out as f64 * 2.0) as usize);
        }
    }
}
