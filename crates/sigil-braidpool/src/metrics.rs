//! metrics.rs — the §23 Prometheus-style counters, for real: "every
//! performance claim should be reproducible from exported counters." Until
//! now every phase's own benchmark numbers (Phase B's tx/s table, Phase E's
//! bandwidth/CPU table, Phase G's win-rate spread) were one-off `println!`s
//! from a bench binary, not something a running node exposes. `store::memory`'s
//! `BatchStoreMetrics` is the one exception — plain atomics, explicitly
//! documented there as "not wired to a real metrics exporter yet." This
//! module is that exporter's real shape: the same §23 names, atomic
//! counters, and a text-exposition renderer any HTTP handler can serve
//! as-is (no `prometheus` crate dependency — the exposition format for
//! plain counters/gauges is a handful of lines per metric, not worth a new
//! dependency for).
//!
//! Standalone; nothing in this crate or `sigil-node`/`sigil-api` increments
//! these yet — a real integration means threading a `&MetricsRegistry`
//! through the actual ingest/seal/certify/repair call sites, which is
//! separate follow-up wiring work, not done here.

use std::sync::atomic::{AtomicU64, Ordering};

macro_rules! counters {
    ($($field:ident: $name:literal => $help:literal),+ $(,)?) => {
        /// The full §23 counter set. Every field name matches its
        /// Prometheus metric name 1:1 (`sigil_braidpool_<field>`).
        #[derive(Default)]
        pub struct MetricsRegistry {
            $(pub $field: AtomicU64,)+
        }

        impl MetricsRegistry {
            pub fn new() -> Self {
                Self::default()
            }

            /// Render every counter as Prometheus text exposition format
            /// (HELP + TYPE + one sample line per metric) — valid to serve
            /// directly from an HTTP handler with
            /// `Content-Type: text/plain; version=0.0.4`.
            pub fn render_prometheus_text(&self) -> String {
                let mut out = String::new();
                $(
                    out.push_str(&format!("# HELP sigil_braidpool_{} {}\n", $name, $help));
                    out.push_str(&format!("# TYPE sigil_braidpool_{} counter\n", $name));
                    out.push_str(&format!("sigil_braidpool_{} {}\n", $name, self.$field.load(Ordering::Relaxed)));
                )+
                out
            }
        }
    };
}

counters! {
    ingest_total: "ingest_total" => "Transactions accepted into a worker's ingest queue",
    ingest_rejected_total: "ingest_rejected_total" => "Transactions rejected at ingest (capacity, dedup, invalid)",
    verified_total: "verified_total" => "Transactions that passed signature verification",
    queue_bytes: "queue_bytes" => "Current bytes queued across all workers (gauge, exposed as counter type for simplicity)",
    worker_depth: "worker_depth" => "Current queued transaction count across all workers",
    batches_sealed_total: "batches_sealed_total" => "Batches sealed by BatchSealer",
    batch_bytes: "batch_bytes" => "Cumulative bytes across all sealed batches",
    cert_latency_seconds: "cert_latency_seconds" => "Cumulative seconds spent assembling availability certificates (x1000, integer)",
    cert_failures_total: "cert_failures_total" => "Certificate assembly attempts that failed to reach quorum",
    shards_sent_total: "shards_sent_total" => "Erasure-coded shards sent to peers",
    shards_received_total: "shards_received_total" => "Erasure-coded shards received from peers",
    reconstruct_total: "reconstruct_total" => "Successful batch reconstructions from shards",
    reconstruct_failures_total: "reconstruct_failures_total" => "Failed batch reconstruction attempts",
    repair_requests_total: "repair_requests_total" => "Repair requests issued for missing shards",
    gc_batches_total: "gc_batches_total" => "Batches removed by garbage collection",
    committed_txs_total: "committed_txs_total" => "Transactions committed to the chain from this mempool",
    committed_bytes_total: "committed_bytes_total" => "Bytes committed to the chain from this mempool",
}

impl MetricsRegistry {
    pub fn incr(counter: &AtomicU64) {
        counter.fetch_add(1, Ordering::Relaxed);
    }

    pub fn add(counter: &AtomicU64, n: u64) {
        counter.fetch_add(n, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_registry_is_all_zero() {
        let m = MetricsRegistry::new();
        assert_eq!(m.ingest_total.load(Ordering::Relaxed), 0);
        assert_eq!(m.committed_bytes_total.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn increments_are_visible_in_rendered_text() {
        let m = MetricsRegistry::new();
        m.ingest_total.fetch_add(42, Ordering::Relaxed);
        MetricsRegistry::incr(&m.batches_sealed_total);
        MetricsRegistry::add(&m.committed_txs_total, 7);

        let text = m.render_prometheus_text();
        assert!(text.contains("sigil_braidpool_ingest_total 42"), "text was:\n{text}");
        assert!(text.contains("sigil_braidpool_batches_sealed_total 1"));
        assert!(text.contains("sigil_braidpool_committed_txs_total 7"));
    }

    #[test]
    fn every_counter_has_a_help_and_type_line() {
        let m = MetricsRegistry::new();
        let text = m.render_prometheus_text();
        // 17 counters * 3 lines (HELP/TYPE/sample) each.
        assert_eq!(text.lines().count(), 17 * 3);
        assert!(text.contains("# HELP sigil_braidpool_reconstruct_failures_total"));
        assert!(text.contains("# TYPE sigil_braidpool_gc_batches_total counter"));
    }
}
