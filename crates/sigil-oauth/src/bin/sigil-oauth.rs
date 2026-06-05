//! sigil-oauth CLI — demo + a persistent Authorization Server's key utilities.
//!
//!   sigil-oauth                       narrated end-to-end demo (ephemeral key)
//!   sigil-oauth anchor   [opts]       print the stable anchor TXT to publish in DNS
//!   sigil-oauth mint     [opts]       mint a real token (stable key) + print anchor
//!   sigil-oauth verify <token> <txt>  offline-verify a token against an anchor TXT
//!
//! Options:  --issuer <domain>  --seed <path>  --scope <s>  --epoch <n>
//!
//! The stable key lives in a 32-byte hex seed file (default
//! /home/orobit/sigil-data/sigil-oauth-as.seed), created on first use. The anchor
//! it prints is the exact `_sigil-oauth.<issuer>` TXT record an operator publishes;
//! tokens minted with the same seed then verify against it (offline, via DNS).

use sigil_oauth::*;

fn line() {
    println!("  {}", "─".repeat(70));
}
fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn arg(args: &[String], flag: &str, default: &str) -> String {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .cloned()
        .unwrap_or_else(|| default.to_string())
}

const DEFAULT_SEED: &str = "/home/orobit/sigil-data/sigil-oauth-as.seed";

/// Load a 32-byte hex seed, or create + persist one (0600).
fn load_or_create_seed(path: &str) -> [u8; 32] {
    if let Ok(s) = std::fs::read_to_string(path) {
        if let Ok(b) = hex::decode(s.trim()) {
            if b.len() == 32 {
                let mut k = [0u8; 32];
                k.copy_from_slice(&b);
                return k;
            }
        }
    }
    let mut seed = [0u8; 32];
    {
        use rand::Rng;
        rand::thread_rng().fill(&mut seed[..]);
    }
    if let Err(e) = std::fs::write(path, hex::encode(seed)) {
        eprintln!("warning: could not persist seed to {path}: {e} (key is ephemeral this run)");
    } else {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        }
    }
    seed
}

fn cmd_anchor(args: &[String]) {
    let issuer = arg(args, "--issuer", "sigilgraph.quillon.xyz");
    let epoch: u64 = arg(args, "--epoch", "0").parse().unwrap_or(0);
    let alg = arg(args, "--alg", "ed25519");
    let anchor = if alg == "sqisign5" {
        // SQIsign keygen is NOT seed-deterministic → persist the generated pk+sk and reuse.
        let key_path = arg(args, "--sqkey", "/home/orobit/.flux-secrets/fluxapp-oauth.sqisign");
        let signer = load_or_create_sqisign(&key_path);
        DnsAnchor::for_issuer(&issuer, &signer.pubkey(), signer.alg(), epoch)
    } else {
        let seed = load_or_create_seed(&arg(args, "--seed", DEFAULT_SEED));
        let kp = Keypair::from_seed(&seed);
        DnsAnchor::for_issuer(&issuer, &kp.pubkey(), "ed25519", epoch)
    };
    eprintln!("# publish this as a TXT record at  _sigil-oauth.{issuer}");
    eprintln!("# key_id={}  alg={}  epoch={}  pubkey_bytes={}", anchor.key_id, anchor.alg, epoch, anchor.pubkey.len());
    println!("{}", anchor.to_txt());
}

/// Load a persisted SQIsign issuer keypair (pk\nsk, hex) or generate + persist one (0600).
fn load_or_create_sqisign(path: &str) -> sigil_oauth::IssuerSigner {
    use sigil_oauth::IssuerSigner;
    if let Ok(s) = std::fs::read_to_string(path) {
        let mut it = s.lines();
        if let (Some(pk), Some(sk)) = (it.next(), it.next()) {
            if let (Ok(pk), Ok(sk)) = (hex::decode(pk.trim()), hex::decode(sk.trim())) {
                return IssuerSigner::from_sqisign5(pk, sk);
            }
        }
    }
    let signer = IssuerSigner::sqisign5();
    if let IssuerSigner::Sqisign5 { pk, sk } = &signer {
        if let Err(e) = std::fs::write(path, format!("{}\n{}\n", hex::encode(pk), hex::encode(sk))) {
            eprintln!("warning: could not persist SQIsign key to {path}: {e} (ephemeral this run)");
        } else {
            #[cfg(unix)]
            { use std::os::unix::fs::PermissionsExt; let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)); }
        }
    }
    signer
}

fn cmd_mint(args: &[String]) {
    let issuer = arg(args, "--issuer", "sigilgraph.quillon.xyz");
    let scope = arg(args, "--scope", "dex:trade send:read profile:read");
    let client = arg(args, "--client", "wallet.sigilgraph.quillon.xyz");
    let redirect = arg(args, "--redirect", "https://wallet.sigilgraph.quillon.xyz/cb");
    let alg = arg(args, "--alg", "ed25519");
    let mut iss = if alg == "sqisign5" {
        let key_path = arg(args, "--sqkey", "/home/orobit/.flux-secrets/fluxapp-oauth.sqisign");
        Issuer::new_sqisign(&issuer, load_or_create_sqisign(&key_path))
    } else {
        let seed = load_or_create_seed(&arg(args, "--seed", DEFAULT_SEED));
        Issuer::new(&issuer, Keypair::from_seed(&seed))
    };
    iss.register_client(&client, vec![redirect.clone()]);
    // a wallet "logs in" by signing the request (here: a fresh demo wallet)
    let wallet = Keypair::generate();
    let (verifier, challenge) = pkce_pair();
    let req = AuthRequest {
        client_id: client.clone(),
        redirect_uri: redirect,
        scope: scope.clone(),
        code_challenge: challenge,
        code_challenge_method: "S256".into(),
        state: "cli".into(),
        nonce: hex_short(),
    };
    let code = iss
        .authorize(&req, &WalletAssertion::sign(&wallet, &req))
        .expect("authorize");
    let resp = iss
        .token(&TokenRequest { code, code_verifier: verifier, client_id: client, holder_pubkey: None })
        .expect("token");

    let anchor = iss.anchor();
    println!("ANCHOR_TXT  {}", anchor.to_txt());
    println!("WALLET      {}", wallet_id(&wallet.pubkey()));
    println!("SCOPE       {scope}");
    println!("ACCESS      {}", resp.access_token);
    println!("REFRESH     {}", resp.refresh_token);
    // self-check: prove the token verifies offline against the anchor we printed
    match verify_token(&resp.access_token, &anchor, now()) {
        Ok(c) => println!("VERIFY      ✓ offline-valid (sub={}, exp in {}s)", &c.sub[..14], c.exp - now()),
        Err(e) => println!("VERIFY      ✗ {e:?}"),
    }
}

fn cmd_verify(args: &[String]) {
    // positional: verify <token> <anchor-txt...>
    let rest: Vec<&String> = args.iter().filter(|a| !a.starts_with("--")).collect();
    if rest.len() < 2 {
        eprintln!("usage: sigil-oauth verify <token> <anchor TXT string>");
        std::process::exit(2);
    }
    let token = rest[0];
    let anchor_txt = rest[1..].iter().map(|s| s.as_str()).collect::<Vec<_>>().join(" ");
    let anchor = match DnsAnchor::from_txt(&anchor_txt) {
        Ok(a) => a,
        Err(e) => {
            println!("INVALID anchor TXT: {e:?}");
            std::process::exit(1);
        }
    };
    match verify_token(token, &anchor, now()) {
        Ok(c) => println!("VALID  sub={} aud={} scope=\"{}\" iss={} epoch={}", c.sub, c.aud, c.scope, c.iss, c.epoch),
        Err(e) => {
            println!("INVALID  {e:?}");
            std::process::exit(1);
        }
    }
}

fn hex_short() -> String {
    use rand::Rng;
    let mut b = [0u8; 8];
    rand::thread_rng().fill(&mut b[..]);
    hex::encode(b)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(|s| s.as_str()) {
        Some("anchor") => cmd_anchor(&args[1..]),
        Some("mint") => cmd_mint(&args[1..]),
        Some("verify") => cmd_verify(&args[1..]),
        Some("demo") | None => demo(),
        Some(other) => {
            eprintln!("unknown subcommand '{other}'. try: anchor | mint | verify | demo");
            std::process::exit(2);
        }
    }
}

fn demo() {
    println!("\n  sigil-oauth — OAuth2 with a DNS-anchored, post-quantum trust root\n");

    let mut issuer = Issuer::new("auth.sigilgraph.com", Keypair::generate());
    let anchor = issuer.anchor();
    println!("  1. AS publishes its signing key to DNS — NO CA, NO well-known doc:");
    println!("     _sigil-oauth.auth.sigilgraph.com  TXT");
    println!("       {}", anchor.to_txt());
    println!("     key_id={}  alg={}  (swap alg → sqisign5/dilithium5 ⇒ PQ)", anchor.key_id, anchor.alg);
    line();

    issuer.register_client("wallet.sigilgraph.com", vec!["https://wallet.sigilgraph.com/cb".into()]);
    let wallet = Keypair::generate();
    println!("  2. User = a SIGIL wallet (no password store):");
    println!("     wallet  {}", wallet_id(&wallet.pubkey()));
    line();

    let (verifier, challenge) = pkce_pair();
    let req = AuthRequest {
        client_id: "wallet.sigilgraph.com".into(),
        redirect_uri: "https://wallet.sigilgraph.com/cb".into(),
        scope: "dex:trade send:read".into(),
        code_challenge: challenge,
        code_challenge_method: "S256".into(),
        state: "opaque-csrf".into(),
        nonce: "n-12345".into(),
    };
    let assertion = WalletAssertion::sign(&wallet, &req);
    let code = issuer.authorize(&req, &assertion).expect("authorize");
    println!("  3. /authorize — wallet SIGNS the request (proves ownership):");
    println!("     scope = {}", req.scope);
    println!("     → single-use, PKCE-bound code  {}…", &code[..16]);
    line();

    let holder = Keypair::generate();
    let resp = issuer
        .token(&TokenRequest {
            code,
            code_verifier: verifier,
            client_id: "wallet.sigilgraph.com".into(),
            holder_pubkey: Some(holder.pubkey()),
        })
        .expect("token");
    let token = resp.access_token.clone();
    println!("  4. /token — PKCE verified, AS mints a SIGNED token (DPoP-bound):");
    println!("     access  {}…{}  ({} B)", &token[..20], &token[token.len() - 10..], token.len());
    println!("     refresh {}…  ({} B, rotates on use)", &resp.refresh_token[..20], resp.refresh_token.len());
    line();

    let resolver = StaticResolver::default().with(anchor.clone());
    let now = now();
    let claims = verify_token_via_dns(&token, &resolver, now).expect("offline verify");
    println!("  5. Resource server verifies OFFLINE (zero AS round-trips):");
    println!("     sub   = {}", claims.sub);
    println!("     aud   = {}   scope = {}", claims.aud, claims.scope);
    println!("     iss   = {} (matched against DNS anchor ✓)", claims.iss);

    let ts = now;
    let proof = make_dpop(&holder, "POST", "https://api.sigilgraph.com/send", &token, ts);
    let real = verify_dpop(&claims, &holder.pubkey(), "POST", "https://api.sigilgraph.com/send", &token, ts, ts, &proof).is_ok();
    let thief = Keypair::generate();
    let stolen = verify_dpop(&claims, &thief.pubkey(), "POST", "https://api.sigilgraph.com/send", &token, ts, ts, &proof);
    println!("     DPoP  holder proves possession: {}", if real { "✓" } else { "✗" });
    println!("           stolen token (no key):    {} ({:?})", if stolen.is_err() { "rejected ✓" } else { "ACCEPTED ✗" }, stolen.err());
    line();

    let rogue = DnsAnchor::for_issuer("auth.sigilgraph.com", &Keypair::generate().pubkey(), "ed25519", 0);
    let forged = verify_token(&token, &rogue, now);
    println!("  6. Rogue key publishing the SAME issuer name → token rejected: {:?}", forged.err());
    line();

    let r2 = issuer.refresh(&resp.refresh_token, now).expect("refresh");
    let replay = issuer.refresh(&resp.refresh_token, now);
    println!("  7. /refresh — token ROTATES (old one dies):");
    println!("     rotated access verifies: {}", verify_token(&r2.access_token, &anchor, now).is_ok());
    println!("     replay spent refresh:    {:?}  (theft ⇒ family revoked)", replay.err());
    line();

    issuer.revoke_all();
    let after = issuer.anchor();
    println!("  8. revoke_all() — AS publishes a new epoch to DNS (the kill switch):");
    println!("     anchor epoch {} → {}  (republished TXT)", anchor.epoch, after.epoch);
    println!("     old access token now: {:?}  (no introspection call)", verify_token(&token, &after, now).err());
    line();

    println!("\n  what sigil-oauth replaces vs classic OAuth2");
    line();
    println!("  {:<22}{:<26}{}", "concern", "classic OAuth2", "sigil-oauth");
    line();
    let rows = [
        ("trust root", "TLS cert + well-known", "DNS TXT anchor (DoH)"),
        ("issuer key crypto", "RSA/ECDSA (HMAC JWT)", "Ed25519 → SQIsign/Dilith."),
        ("user credential", "password / federated", "wallet signature"),
        ("token verify", "introspection round-trip", "offline (anchored key)"),
        ("revocation", "blocklist / introspect", "DNS epoch bump"),
        ("token theft", "bearer = full access", "DPoP cnf binding"),
        ("post-quantum", "no", "alg= swap, no flow change"),
    ];
    for (c, a, b) in rows {
        println!("  {c:<22}{a:<26}{b}");
    }
    line();
    println!("\n  the whole flow above used ZERO calls back to the AS to verify —");
    println!("  the DNS anchor is the only trust input. That's the win.\n");
}
