//! End-to-end round-trip: publish → verify → apply → verify-applied.
//!
//! Exercises the full pipeline with real SQIsign L5 keys, a real binary
//! fixture, and the actual filesystem. Catches integration bugs that the
//! per-module unit tests miss (e.g. signing-bytes drift between publish
//! and verify, byte-equality after apply).

use std::fs;

use sigil_updater::{
    apply_to_target, verify_announcement, verify_binary_bytes,
    ReleaseAnnouncement,
};

fn fake_binary() -> Vec<u8> {
    // Mimic a real release binary's shape: ELF-ish prefix + bulk.
    let mut v = Vec::new();
    v.extend_from_slice(&[0x7f, b'E', b'L', b'F']); // not a real ELF, just bytes
    v.extend_from_slice(&[0u8; 4096]);              // 4 KB of zeros
    v.extend_from_slice(b"sigil-node-v0.0.2 phase-0 fake binary");
    v.extend_from_slice(&[42u8; 1024]);             // some entropy
    v
}

fn fake_proof_json() -> Vec<u8> {
    serde_json::json!({
        "agent_wallet": "qnk7154929a6aa0c118791373ea21004aca6e494e6e031c36f780cd5acedf031ccb",
        "artifact_blake3_hex": "deadbeef".repeat(8),
        "compiled_at_us": 1_780_069_000_000_000u64,
        "fluxc_version": "0.17.0",
        "module": "sigil-node",
        "note": "round-trip integration fixture",
        "size_bytes": 5160,
        "sqisign_pubkey_hex": "(test fixture)",
        "sqisign_sig_hex": "(test fixture)",
        "synthetic": true,
        "version": 1,
    }).to_string().into_bytes()
}

#[test]
fn round_trip_publish_verify_apply() {
    let dir = tempfile::tempdir().expect("tempdir");

    // ── 1. publisher side ─────────────────────────────────────────────────
    let (sk, pk) = flux_sqisign::keygen();
    let binary = fake_binary();
    let proof_blob = fake_proof_json();

    let mut announcement = ReleaseAnnouncement::unsigned(
        "sigil-node",
        "0.0.2",
        "https://example.org/sigil-node-v0.0.2",
        &binary,
        proof_blob.clone(),
        pk.clone(),
        /*min_consensus_version*/ 0,
        /*activation_height   */ 1024,
        /*timestamp_us        */ 1_780_069_000_000_000u64,
        "integration round-trip",
    );
    announcement.sign(&sk).expect("sign");

    let announcement_path = dir.path().join("sigil-node-v0.0.2.announcement.json");
    let serialized = serde_json::to_vec(&announcement).expect("serialize");
    fs::write(&announcement_path, &serialized).expect("write announcement");

    let binary_path = dir.path().join("sigil-node-v0.0.2");
    fs::write(&binary_path, &binary).expect("write binary");

    // ── 2. verifier side (a node receiving the announcement) ──────────────
    let received_bytes = fs::read(&announcement_path).expect("read announcement");
    let received: ReleaseAnnouncement =
        serde_json::from_slice(&received_bytes).expect("deserialize");
    assert_eq!(received, announcement, "round-trip serde must be identity");

    let ok = verify_announcement(&received).expect("announcement valid");
    assert_eq!(ok.product, "sigil-node");
    assert_eq!(ok.version, "0.0.2");
    assert_eq!(ok.binary_size_bytes, binary.len() as u64);
    assert_eq!(ok.activation_height, 1024);

    // Fetch + check the binary bytes (here just a local file read).
    let fetched = fs::read(&binary_path).expect("fetch binary");
    verify_binary_bytes(&received, &fetched).expect("hash matches");

    // ── 3. apply to a target path ────────────────────────────────────────
    let target = dir.path().join("install").join("sigil-node");
    fs::create_dir_all(target.parent().unwrap()).expect("mkdir install/");
    let outcome = apply_to_target(&received, &fetched, &target).expect("apply");

    assert!(!outcome.previous_existed, "fresh install should report no previous");
    assert_eq!(outcome.target, target);

    // ── 4. verify what landed on disk byte-for-byte ──────────────────────
    let installed = fs::read(&target).expect("read installed");
    assert_eq!(installed, binary, "installed bytes must match the original binary");

    // ── 5. apply again — second-run should preserve the previous binary ──
    let outcome2 = apply_to_target(&received, &fetched, &target).expect("apply 2");
    assert!(outcome2.previous_existed, "second run should report previous existed");
    let backed_up = fs::read(&outcome2.backup).expect("read backup");
    assert_eq!(backed_up, binary, "backup is the previous (identical) install");
    let installed_again = fs::read(&target).expect("read installed 2");
    assert_eq!(installed_again, binary);
}

#[test]
fn round_trip_detects_swapped_binary_bytes() {
    // An attacker who controls the binary mirror but not the publisher's key
    // can't swap bytes without us catching it. This validates rule #1 of the
    // SIGIL north star: every binary is provenance-signed at build time.
    let (sk, pk) = flux_sqisign::keygen();
    let real_binary = b"the real bytes that were signed".to_vec();
    let mut announcement = ReleaseAnnouncement::unsigned(
        "sigil-node", "0.0.2",
        "https://example.org/binary",
        &real_binary,
        fake_proof_json(),
        pk, 0, 1024, 0, "tamper test",
    );
    announcement.sign(&sk).expect("sign");

    let attacker_binary = b"these are not the bytes the publisher signed".to_vec();
    // verify_announcement still passes — the sig is over the announcement,
    // not the binary itself. That's why the BLAKE3 check must run separately.
    verify_announcement(&announcement).expect("announcement sig itself is intact");
    assert!(
        verify_binary_bytes(&announcement, &attacker_binary).is_err(),
        "swapped binary must be rejected by the hash check"
    );
}

#[test]
fn round_trip_detects_tampered_announcement() {
    let (sk, pk) = flux_sqisign::keygen();
    let binary = b"some bytes".to_vec();
    let mut a = ReleaseAnnouncement::unsigned(
        "sigil-node", "0.0.2", "url",
        &binary, fake_proof_json(),
        pk, 0, 1024, 0, "original note",
    );
    a.sign(&sk).expect("sign");

    // Attacker bumps activation_height to make the release apply sooner.
    let mut tampered = a.clone();
    tampered.activation_height = 1;
    assert!(verify_announcement(&tampered).is_err(),
        "any field flip must break the signature");
}
