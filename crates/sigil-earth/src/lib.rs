//! sigil-earth — the Kristensen Earth Rotation Gauge K⊕ as a service.
//!
//! One binary, four jobs:
//!   fetch   IERS finals2000A (+ daily rapid) and GFZ ESMGFZ angular-momentum analyses and
//!           forecasts → the de-wobbled gauge, the 10-day forecast, alerts, a signed attest row,
//!           all written atomically into the sigilgraph.org web root.
//!   serve   /v1/earth/* JSON + a Server-Sent-Events stream, read from those files.
//!   alert   evaluate (or inject) the alert rule and dispatch to webhooks / Buzz.
//!   attest  anchor the newest attest row on the SIGIL chain (shielded memo self-send).
//!
//! Every number the pages show is computed here, once, and published with its inputs and a
//! provenance tag (measured / predicted / model / derived / assumed). Nothing is computed in
//! a browser that is not also here.

pub mod alert;
pub mod attest;
pub mod buzz;
pub mod eam;
pub mod eop;
pub mod fetch;
pub mod forecast;
pub mod model;
pub mod out;
pub mod serve;
pub mod time;
pub mod tips;

pub const UA: &str = "sigil-earth/1.0 (sigilgraph.org Kristensen Earth gauge)";
pub const VERSION: &str = "sigil-earth/1.0";

/// The K family ladder: < 1 stable · 1–3 elevated · ≥ 3 critical.
pub fn regime(k: f64) -> &'static str {
    if !k.is_finite() {
        "unknown"
    } else if k < 1.0 {
        "stable"
    } else if k < 3.0 {
        "elevated"
    } else {
        "critical"
    }
}
