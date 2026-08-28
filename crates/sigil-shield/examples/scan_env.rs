// scan_env.rs — read a wallet's shielded balance from the chain's own note
// ciphertexts, with the seed supplied via an ENV VAR (never argv, so it cannot
// leak into a process list or a shell history).
//
//   SIGIL_SCAN_SEED=<64-hex>  fluxc run -p sigil-shield --example scan_env -- <ciphertexts-file>
//
// The file is one base-JSON ciphertext per line, as served by
// GET /v1/shielded/leaves. A successful AEAD open IS the ownership proof —
// nothing marks a ciphertext as ours, which is exactly why an observer cannot
// tell who was paid. Prints public aggregates only: never the seed, never the
// blinding factors.
use sigil_shield::note_cipher::{enc_identity_from_seed, try_open_note, NoteCiphertext};

fn main() {
    let seed_hex = std::env::var("SIGIL_SCAN_SEED")
        .expect("set SIGIL_SCAN_SEED=<64-hex> (env only — never pass the seed as an argument)");
    let seed: [u8; 32] = hex::decode(seed_hex.trim())
        .expect("SIGIL_SCAN_SEED must be hex")
        .try_into()
        .expect("SIGIL_SCAN_SEED must be 32 bytes");
    let path = std::env::args().nth(1).expect("usage: scan_env <ciphertexts-file>");
    let body = std::fs::read_to_string(&path).expect("read ciphertexts file");

    let id = enc_identity_from_seed(&seed);
    let (mut scanned, mut mine, mut total) = (0u64, 0u64, 0u128);
    let mut values: Vec<u64> = Vec::new();
    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() { continue; }
        scanned += 1;
        if let Ok(pt) = try_open_note(&NoteCiphertext(line.to_string()), &id) {
            mine += 1;
            total += pt.value as u128;
            values.push(pt.value);
        }
    }
    values.sort_unstable();
    println!("scanned_ciphertexts={scanned}");
    println!("notes_owned={mine}");
    println!("balance_base_units={total}");
    println!("balance_sigil={}.{:08}", total / 100_000_000, total % 100_000_000);
    println!("note_values={values:?}");
}
