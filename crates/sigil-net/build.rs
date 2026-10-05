// Which SIGIL network this binary belongs to, fixed at BUILD time.
//
// `SIGIL_NETWORK=sigil-m1 fluxc build ...` produces a mainnet binary; unset gives the
// live testnet, `sigil-g2`, byte-identical to before this file existed. Baking it in
// (rather than reading it at runtime) means a binary can never be pointed at the wrong
// chain by a stray environment variable on a production box.
//
// The same script lives in crates/sigil-header/build.rs; a const assertion in this crate
// fails the build if the two ever disagree.
fn main() {
    println!("cargo:rerun-if-env-changed=SIGIL_NETWORK");
    let id = std::env::var("SIGIL_NETWORK").unwrap_or_else(|_| "sigil-g2".to_string());
    let ok = id.len() == 8
        && id.starts_with("sigil-")
        && id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    if !ok {
        panic!("SIGIL_NETWORK must be exactly 8 characters, start with \"sigil-\" and use [a-z0-9-]; got {id:?}");
    }
    println!("cargo:rustc-env=SIGIL_NET_ID={id}");
    println!("cargo:rustc-env=SIGIL_NET_SHORT={}", &id["sigil-".len()..]);
}
