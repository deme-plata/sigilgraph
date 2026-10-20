//! sigil-node — the SIGIL node program. Since 2026-10-20 the node lives in the library
//! ([`sigil_node::node_main`]), so `sigil-top` runs this exact code instead of a port of it.
fn main() -> std::process::ExitCode {
    sigil_node::node_main(std::env::args().collect())
}
