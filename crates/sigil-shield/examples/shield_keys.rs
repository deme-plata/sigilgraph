// Derive the two shield keys a wallet must publish via POST /v1/shielded/register,
// from the SAME 32-byte seed its Ed25519 signing key uses. Read-only: prints public
// values only, never the seed or the spend key.
//
//   fluxc run -p sigil-shield --example shield_keys -- <64-hex-seed>
use sigil_shield::note_cipher::enc_identity_from_seed;
use sigil_shield::note_v1::to_wire;
use sigil_shield::wallet::ShieldedAccount;

fn main() {
    let arg = std::env::args().nth(1).expect("usage: shield_keys <64-hex-seed>");
    let seed: [u8; 32] = hex::decode(arg.trim()).expect("hex").try_into().expect("32 bytes");
    let acct = ShieldedAccount::from_seed(seed);
    println!("pk_shield={}", hex::encode(to_wire(acct.public_key())));
    println!("pk_encrypt={}", enc_identity_from_seed(&seed).public_hex());
}
