//! flux-secrets CLI — vault smoke/self-test + build stamp.
//!   flux-secrets version    print the genesis-stamped build line
//!   flux-secrets selftest    seal/unseal + capability-gating roundtrip, prove the vault works here
use std::env;
use flux_secrets::Vault;

fn main() {
    let args: Vec<String> = env::args().collect();
    let cmd = args.get(1).map(|s| s.as_str()).unwrap_or("version");
    match cmd {
        "version" | "--version" | "-V" => println!("{}", flux_secrets::stamp().line()),
        "selftest" => {
            let mut v = Vault::create("self-test-passphrase");
            v.grant("app", "secret/*");
            v.put("app", "secret/seed", b"deadbeefcafe").expect("put");
            let got = v.get("app", "secret/seed").expect("get");
            let denied = v.get("intruder", "secret/seed").is_err();
            let json = v.to_json().expect("json");
            let mut v2 = Vault::open(&json, "self-test-passphrase").expect("reopen");
            let persisted = v2.get("app", "secret/seed").unwrap_or_default();
            let wrong = Vault::open(&json, "wrong").is_err();
            let ok = got == b"deadbeefcafe" && denied && persisted == b"deadbeefcafe" && wrong;
            println!("{}", flux_secrets::stamp().line());
            println!("seal/unseal: {}  · capability-deny: {}  · persistence: {}  · wrong-pass-rejected: {}",
                got == b"deadbeefcafe", denied, persisted == b"deadbeefcafe", wrong);
            println!("{}", if ok { "✅ flux-secrets SELFTEST PASSED — vault works on this host" } else { "❌ SELFTEST FAILED" });
            if !ok { std::process::exit(1); }
        }
        _ => println!("usage: flux-secrets version | flux-secrets selftest"),
    }
}
