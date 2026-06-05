//! Dumps the demo multiverse run as JSON — the data feed for the CHRONOS-G
//! browser viz (`sigil/gui/multiverse.html`). Deterministic, so the embedded
//! snapshot in the HTML can be regenerated reproducibly:
//!
//! ```text
//! sigil-multiverse-dump > multiverse-state.json
//! ```

fn main() {
    println!("{}", sigil_chronos::multiverse::demo_multiverse().to_json());
}
