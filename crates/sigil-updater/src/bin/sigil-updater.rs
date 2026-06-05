//! sigil-updater CLI — bare-bones wrapper around the library's three APIs.
//!
//! Subcommands:
//!   keygen   --out-prefix NAME            → write NAME.sk.hex + NAME.pk.hex
//!   publish  --binary B --proof P --version V --sk-hex SK --pk-hex PK
//!            --activation-height H [--url U] [--min-consensus N] [--note S]
//!            [--out A.json]                → emit signed ReleaseAnnouncement
//!   verify   --announcement A [--binary B] → exit 0 if sig + (optional) hash check pass
//!   apply    --announcement A --binary B --target T → atomic .new→target→.bak swap
//!
//! No clap dependency — sigil-node uses the same bare-bones parsing, and
//! Phase 0 wants the dep graph as tight as possible.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;

use sigil_updater::{
    apply_to_target, verify_announcement, verify_binary_bytes,
    ReleaseAnnouncement, UpdaterError,
};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let sub = args.get(1).map(String::as_str).unwrap_or("");
    let rest = parse_flags(&args[2.min(args.len())..]);

    let rc = match sub {
        "keygen"     => cmd_keygen(&rest),
        "publish"    => cmd_publish(&rest),
        "verify"     => cmd_verify(&rest),
        "apply"      => cmd_apply(&rest),
        "--version" | "-V" | "version" => {
            println!("sigil-updater {} (schema v{})",
                env!("CARGO_PKG_VERSION"),
                sigil_updater::ANNOUNCEMENT_SCHEMA_VERSION);
            Ok(())
        }
        _ => {
            print_usage();
            return ExitCode::from(64);
        }
    };

    match rc {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("sigil-updater: {}", e);
            ExitCode::from(1)
        }
    }
}

fn print_usage() {
    println!("{}", USAGE);
}

const USAGE: &str = "\
sigil-updater — SIGIL release publish/verify/apply

USAGE:
  sigil-updater keygen  --out-prefix NAME
  sigil-updater publish --binary B --proof P --version V --sk-hex SK --pk-hex PK
                        --activation-height H [--url U] [--min-consensus N]
                        [--product P] [--note S] [--out OUT.json]
  sigil-updater verify  --announcement A [--binary B]
  sigil-updater apply   --announcement A --binary B --target T

EXAMPLES:
  sigil-updater keygen --out-prefix rocky
    → rocky.sk.hex, rocky.pk.hex

  sigil-updater publish \\
      --binary sigil-node-v0.0.2 \\
      --proof  sigil-node-v0.0.2.proof \\
      --version 0.0.2 \\
      --sk-hex $(cat rocky.sk.hex) \\
      --pk-hex $(cat rocky.pk.hex) \\
      --activation-height 1024 \\
      --out sigil-node-v0.0.2.announcement.json

  sigil-updater verify --announcement sigil-node-v0.0.2.announcement.json
  sigil-updater verify --announcement A.json --binary sigil-node-v0.0.2

  sigil-updater apply  --announcement A.json --binary sigil-node-v0.0.2 \\
                       --target /usr/local/bin/sigil-node
";

// ── Flag parsing ────────────────────────────────────────────────────────────

fn parse_flags(args: &[String]) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        if let Some(name) = arg.strip_prefix("--") {
            let val = args.get(i + 1).cloned().unwrap_or_default();
            out.insert(name.to_string(), val);
            i += 2;
        } else {
            i += 1;
        }
    }
    out
}

fn req<'a>(flags: &'a HashMap<String, String>, name: &str) -> Result<&'a String, String> {
    flags.get(name).filter(|v| !v.is_empty())
        .ok_or_else(|| format!("missing required flag --{}", name))
}

fn opt_u64(flags: &HashMap<String, String>, name: &str) -> Result<Option<u64>, String> {
    match flags.get(name).filter(|v| !v.is_empty()) {
        None => Ok(None),
        Some(v) => v.parse::<u64>()
            .map(Some)
            .map_err(|e| format!("--{}: {}", name, e)),
    }
}

fn opt_u32(flags: &HashMap<String, String>, name: &str) -> Result<Option<u32>, String> {
    match flags.get(name).filter(|v| !v.is_empty()) {
        None => Ok(None),
        Some(v) => v.parse::<u32>()
            .map(Some)
            .map_err(|e| format!("--{}: {}", name, e)),
    }
}

// ── keygen ──────────────────────────────────────────────────────────────────

fn cmd_keygen(flags: &HashMap<String, String>) -> Result<(), String> {
    let prefix = req(flags, "out-prefix")?;
    let (sk, pk) = flux_sqisign::keygen();
    let sk_path = format!("{}.sk.hex", prefix);
    let pk_path = format!("{}.pk.hex", prefix);
    // Restrict sk perms on Unix; on Windows the OS will handle ACLs sensibly enough.
    fs::write(&sk_path, hex::encode(&sk)).map_err(|e| format!("write {}: {}", sk_path, e))?;
    fs::write(&pk_path, hex::encode(&pk)).map_err(|e| format!("write {}: {}", pk_path, e))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut p = fs::metadata(&sk_path).map_err(|e| e.to_string())?.permissions();
        p.set_mode(0o600);
        let _ = fs::set_permissions(&sk_path, p);
    }
    println!("wrote {} ({} bytes hex)", sk_path, sk.len() * 2);
    println!("wrote {} ({} bytes hex)", pk_path, pk.len() * 2);
    Ok(())
}

// ── publish ─────────────────────────────────────────────────────────────────

fn cmd_publish(flags: &HashMap<String, String>) -> Result<(), String> {
    let binary_path = PathBuf::from(req(flags, "binary")?);
    let proof_path = PathBuf::from(req(flags, "proof")?);
    let version = req(flags, "version")?.clone();
    let sk_hex = req(flags, "sk-hex")?;
    let pk_hex = req(flags, "pk-hex")?;
    let activation_height = opt_u64(flags, "activation-height")?
        .ok_or_else(|| "missing required flag --activation-height".to_string())?;
    let url = flags.get("url").cloned().unwrap_or_default();
    let min_consensus = opt_u32(flags, "min-consensus")?.unwrap_or(0);
    let product = flags.get("product").cloned().unwrap_or_else(|| "sigil-node".to_string());
    let note = flags.get("note").cloned().unwrap_or_default();
    let out_path = flags.get("out").cloned();

    let sk = hex::decode(sk_hex).map_err(|e| format!("--sk-hex: {}", e))?;
    let pk = hex::decode(pk_hex).map_err(|e| format!("--pk-hex: {}", e))?;
    let binary_bytes = fs::read(&binary_path)
        .map_err(|e| format!("read binary {}: {}", binary_path.display(), e))?;
    let proof_blob = fs::read(&proof_path)
        .map_err(|e| format!("read proof {}: {}", proof_path.display(), e))?;

    let ts_us = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0);

    let mut a = ReleaseAnnouncement::unsigned(
        product, version, url, &binary_bytes, proof_blob, pk,
        min_consensus, activation_height, ts_us, note,
    );
    a.sign(&sk).map_err(|e: UpdaterError| e.to_string())?;

    let json = serde_json::to_string_pretty(&a)
        .map_err(|e| format!("serialize: {}", e))?;
    match out_path {
        Some(p) => {
            fs::write(&p, &json).map_err(|e| format!("write {}: {}", p, e))?;
            println!("wrote {} ({} bytes)", p, json.len());
        }
        None => println!("{}", json),
    }
    Ok(())
}

// ── verify ──────────────────────────────────────────────────────────────────

fn cmd_verify(flags: &HashMap<String, String>) -> Result<(), String> {
    let announcement_path = PathBuf::from(req(flags, "announcement")?);
    let bytes = fs::read(&announcement_path)
        .map_err(|e| format!("read {}: {}", announcement_path.display(), e))?;
    let a: ReleaseAnnouncement = serde_json::from_slice(&bytes)
        .map_err(|e| format!("parse announcement: {}", e))?;
    let ok = verify_announcement(&a).map_err(|e| e.to_string())?;
    println!("announcement signature: OK");
    println!("  product:           {}", ok.product);
    println!("  version:           {}", ok.version);
    println!("  binary blake3:     {}", ok.binary_blake3_hex);
    println!("  binary size:       {} bytes", ok.binary_size_bytes);
    println!("  activation height: {}", ok.activation_height);
    println!("  min consensus ver: {}", ok.min_consensus_version);

    if let Some(binary) = flags.get("binary").filter(|v| !v.is_empty()) {
        let binary_path = PathBuf::from(binary);
        let bytes = fs::read(&binary_path)
            .map_err(|e| format!("read binary {}: {}", binary_path.display(), e))?;
        verify_binary_bytes(&a, &bytes).map_err(|e| e.to_string())?;
        println!("binary hash:           OK ({} bytes)", bytes.len());
    }
    Ok(())
}

// ── apply ───────────────────────────────────────────────────────────────────

fn cmd_apply(flags: &HashMap<String, String>) -> Result<(), String> {
    let announcement_path = PathBuf::from(req(flags, "announcement")?);
    let binary_path = PathBuf::from(req(flags, "binary")?);
    let target = PathBuf::from(req(flags, "target")?);

    let ann_bytes = fs::read(&announcement_path)
        .map_err(|e| format!("read {}: {}", announcement_path.display(), e))?;
    let a: ReleaseAnnouncement = serde_json::from_slice(&ann_bytes)
        .map_err(|e| format!("parse announcement: {}", e))?;
    // Sig check first (cheap) so we don't even read the binary if the
    // announcement is invalid.
    verify_announcement(&a).map_err(|e| e.to_string())?;

    let new_bytes = fs::read(&binary_path)
        .map_err(|e| format!("read binary {}: {}", binary_path.display(), e))?;
    let outcome = apply_to_target(&a, &new_bytes, &target).map_err(|e| e.to_string())?;

    println!("applied {} v{}", a.product, a.version);
    println!("  target:        {}", outcome.target.display());
    println!("  backup:        {} ({})", outcome.backup.display(),
        if outcome.previous_existed { "previous binary saved" } else { "no previous binary" });
    println!("  bytes written: {}", new_bytes.len());
    Ok(())
}
