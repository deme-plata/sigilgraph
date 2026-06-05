//! flux-secrets — a sealed, capability-scoped credential vault for the Flux/SIGIL agent network.
//!
//! The honest counter-design to "an app that hides another app": here apps get ONLY the secrets
//! they hold an explicit **capability grant** for — access is *visible and contract-based*, never
//! concealed. Every get/put is written to an append-only audit log.
//!
//! Secrets are sealed at rest with an authenticated-encryption scheme built from BLAKE3
//! (XOF keystream ⊕ plaintext, then a keyed BLAKE3 MAC over nonce‖aad‖ciphertext — encrypt-then-MAC).
//! The vault key is derived from a passphrase via an iterated BLAKE3 KDF with a per-vault salt.
//!
//! **Crypto-agility (Stargate discipline):** the cipher is a [`Sealer`] trait and the algorithm id
//! is stored per record, so a post-quantum / Argon2id+XChaCha20 sealer can be swapped in without
//! changing the vault format. Nothing crypto is hardcoded into the vault logic itself.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

const KDF_CTX: &str = "flux-secrets v1 vault-key 2026";
const STREAM_CTX: &str = "flux-secrets v1 stream 2026";
const MAC_CTX: &str = "flux-secrets v1 mac 2026";
const KDF_ROUNDS: u32 = 64; // work factor (BLAKE3 is fast); upgrade path = Argon2id sealer
const CHECK_MARKER: &[u8] = b"flux-secrets-vault-ok";

/// The genesis provenance stamp for this build — `stamp().line()` is the verified one-liner.
pub fn stamp() -> flux_stamp::Stamp { flux_stamp::flux_stamp!() }

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum VaultError {
    #[error("authentication failed — wrong passphrase or tampered vault")]
    Auth,
    #[error("access denied: app '{0}' holds no capability for secret '{1}'")]
    Denied(String, String),
    #[error("secret not found: {0}")]
    NotFound(String),
    #[error("serialize error: {0}")]
    Serde(String),
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

// ───────────────────────── crypto agility ─────────────────────────

/// A pluggable authenticated-encryption backend. The default is [`Blake3Sealer`];
/// a PQ / Argon2id+XChaCha20 sealer can implement this without touching the vault.
pub trait Sealer {
    /// Stable algorithm id, stored in every record so the right opener is chosen on read.
    fn id(&self) -> &'static str;
    /// Seal `plaintext`; returns `(ciphertext, tag)`.
    fn seal(&self, key: &[u8; 32], nonce: &[u8; 32], aad: &[u8], plaintext: &[u8]) -> (Vec<u8>, [u8; 32]);
    /// Open `ciphertext`; returns plaintext only if the tag authenticates.
    fn open(&self, key: &[u8; 32], nonce: &[u8; 32], aad: &[u8], ciphertext: &[u8], tag: &[u8; 32]) -> Option<Vec<u8>>;
}

/// BLAKE3 encrypt-then-MAC: keystream from a keyed XOF, authentication from a keyed BLAKE3 MAC.
pub struct Blake3Sealer;

impl Blake3Sealer {
    fn keystream(key: &[u8; 32], nonce: &[u8; 32], len: usize) -> Vec<u8> {
        let mut h = blake3::Hasher::new_derive_key(STREAM_CTX);
        h.update(key);
        h.update(nonce);
        let mut out = vec![0u8; len];
        h.finalize_xof().fill(&mut out);
        out
    }
    fn mac(key: &[u8; 32], nonce: &[u8; 32], aad: &[u8], ct: &[u8]) -> [u8; 32] {
        let mut h = blake3::Hasher::new_derive_key(MAC_CTX);
        h.update(key);
        h.update(nonce);
        h.update(&(aad.len() as u64).to_le_bytes());
        h.update(aad);
        h.update(&(ct.len() as u64).to_le_bytes());
        h.update(ct);
        *h.finalize().as_bytes()
    }
}

impl Sealer for Blake3Sealer {
    fn id(&self) -> &'static str { "blake3-xof-etm-v1" }

    fn seal(&self, key: &[u8; 32], nonce: &[u8; 32], aad: &[u8], pt: &[u8]) -> (Vec<u8>, [u8; 32]) {
        let ks = Self::keystream(key, nonce, pt.len());
        let ct: Vec<u8> = pt.iter().zip(ks.iter()).map(|(a, b)| a ^ b).collect();
        let tag = Self::mac(key, nonce, aad, &ct);
        (ct, tag)
    }

    fn open(&self, key: &[u8; 32], nonce: &[u8; 32], aad: &[u8], ct: &[u8], tag: &[u8; 32]) -> Option<Vec<u8>> {
        let expect = Self::mac(key, nonce, aad, ct);
        // constant-time compare
        let mut diff = 0u8;
        for i in 0..32 { diff |= expect[i] ^ tag[i]; }
        if diff != 0 { return None; }
        let ks = Self::keystream(key, nonce, ct.len());
        Some(ct.iter().zip(ks.iter()).map(|(a, b)| a ^ b).collect())
    }
}

fn derive_key(passphrase: &[u8], salt: &[u8; 16]) -> [u8; 32] {
    let mut material = Vec::with_capacity(passphrase.len() + 16);
    material.extend_from_slice(passphrase);
    material.extend_from_slice(salt);
    let mut key = blake3::derive_key(KDF_CTX, &material);
    for _ in 0..KDF_ROUNDS {
        key = blake3::derive_key(KDF_CTX, &key);
    }
    key
}

// ───────────────────────── vault format ─────────────────────────

#[derive(Serialize, Deserialize, Clone)]
struct Record {
    alg: String,
    nonce: [u8; 32],
    ct: Vec<u8>,
    tag: [u8; 32],
}

/// One line of the append-only audit log.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct AuditEntry {
    pub ts: u64,
    pub app: String,
    pub action: String,
    pub name: String,
    pub allowed: bool,
}

#[derive(Serialize, Deserialize, Default)]
struct VaultFile {
    version: u32,
    salt: [u8; 16],
    check: Option<Record>,
    secrets: BTreeMap<String, Record>,
    grants: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    audit: Vec<AuditEntry>,
}

/// An open vault: holds the derived key in memory, the sealed file, and the active sealer.
pub struct Vault {
    key: [u8; 32],
    file: VaultFile,
    sealer: Box<dyn Sealer>,
}

/// `*` matches everything; `prefix*` matches by prefix; otherwise exact match.
fn cap_match(pattern: &str, name: &str) -> bool {
    if pattern == "*" {
        true
    } else if let Some(prefix) = pattern.strip_suffix('*') {
        name.starts_with(prefix)
    } else {
        pattern == name
    }
}

impl Vault {
    /// Create a fresh vault sealed under `passphrase`.
    pub fn create(passphrase: &str) -> Self {
        let salt: [u8; 16] = rand::random();
        let key = derive_key(passphrase.as_bytes(), &salt);
        let sealer = Blake3Sealer;
        let nonce: [u8; 32] = rand::random();
        let (ct, tag) = sealer.seal(&key, &nonce, b"check", CHECK_MARKER);
        let mut file = VaultFile { version: 1, salt, ..Default::default() };
        file.check = Some(Record { alg: sealer.id().into(), nonce, ct, tag });
        Vault { key, file, sealer: Box::new(sealer) }
    }

    /// Open an existing vault JSON. Fails with [`VaultError::Auth`] on a wrong passphrase.
    pub fn open(json: &str, passphrase: &str) -> Result<Self, VaultError> {
        let file: VaultFile = serde_json::from_str(json).map_err(|e| VaultError::Serde(e.to_string()))?;
        let key = derive_key(passphrase.as_bytes(), &file.salt);
        let sealer = Blake3Sealer;
        match &file.check {
            Some(c) => {
                let ok = sealer.open(&key, &c.nonce, b"check", &c.ct, &c.tag);
                if ok.as_deref() != Some(CHECK_MARKER) {
                    return Err(VaultError::Auth);
                }
            }
            None => return Err(VaultError::Auth),
        }
        Ok(Vault { key, file, sealer: Box::new(sealer) })
    }

    /// Serialize the sealed vault to JSON for persistence. Plaintext secrets never appear here.
    pub fn to_json(&self) -> Result<String, VaultError> {
        serde_json::to_string(&self.file).map_err(|e| VaultError::Serde(e.to_string()))
    }

    /// Grant `app` access to every secret matching `pattern` (`*`, `prefix*`, or exact).
    pub fn grant(&mut self, app: &str, pattern: &str) {
        let g = self.file.grants.entry(app.to_string()).or_default();
        if !g.iter().any(|p| p == pattern) {
            g.push(pattern.to_string());
        }
    }

    /// Revoke all of `app`'s capabilities.
    pub fn revoke(&mut self, app: &str) {
        self.file.grants.remove(app);
    }

    fn allowed(&self, app: &str, name: &str) -> bool {
        self.file.grants.get(app).map(|ps| ps.iter().any(|p| cap_match(p, name))).unwrap_or(false)
    }

    fn audit(&mut self, app: &str, action: &str, name: &str, allowed: bool) {
        self.file.audit.push(AuditEntry { ts: now(), app: app.into(), action: action.into(), name: name.into(), allowed });
    }

    /// Seal a secret under `name`. `app` must hold a capability for `name` (audited).
    pub fn put(&mut self, app: &str, name: &str, secret: &[u8]) -> Result<(), VaultError> {
        let ok = self.allowed(app, name);
        self.audit(app, "put", name, ok);
        if !ok {
            return Err(VaultError::Denied(app.into(), name.into()));
        }
        let nonce: [u8; 32] = rand::random();
        let (ct, tag) = self.sealer.seal(&self.key, &nonce, name.as_bytes(), secret);
        self.file.secrets.insert(name.into(), Record { alg: self.sealer.id().into(), nonce, ct, tag });
        Ok(())
    }

    /// Open a secret. `app` must hold a capability for `name` (audited). Returns plaintext bytes.
    pub fn get(&mut self, app: &str, name: &str) -> Result<Vec<u8>, VaultError> {
        let ok = self.allowed(app, name);
        self.audit(app, "get", name, ok);
        if !ok {
            return Err(VaultError::Denied(app.into(), name.into()));
        }
        let rec = self.file.secrets.get(name).ok_or_else(|| VaultError::NotFound(name.into()))?;
        self.sealer
            .open(&self.key, &rec.nonce, name.as_bytes(), &rec.ct, &rec.tag)
            .ok_or(VaultError::Auth)
    }

    /// Names of all sealed secrets (not their values).
    pub fn names(&self) -> Vec<String> {
        self.file.secrets.keys().cloned().collect()
    }

    /// The append-only audit trail.
    pub fn audit_log(&self) -> &[AuditEntry] {
        &self.file.audit
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seal_unseal_roundtrip() {
        let s = Blake3Sealer;
        let key = [7u8; 32];
        let nonce = [9u8; 32];
        let (ct, tag) = s.seal(&key, &nonce, b"aad", b"the wallet seed");
        assert_ne!(ct, b"the wallet seed");
        assert_eq!(s.open(&key, &nonce, b"aad", &ct, &tag).as_deref(), Some(&b"the wallet seed"[..]));
    }

    #[test]
    fn tamper_is_rejected() {
        let s = Blake3Sealer;
        let key = [1u8; 32];
        let nonce = [2u8; 32];
        let (mut ct, tag) = s.seal(&key, &nonce, b"", b"secret");
        ct[0] ^= 0xff; // flip a bit
        assert!(s.open(&key, &nonce, b"", &ct, &tag).is_none());
    }

    #[test]
    fn wrong_passphrase_fails_to_open() {
        let v = Vault::create("correct horse battery staple");
        let json = v.to_json().unwrap();
        assert!(Vault::open(&json, "wrong passphrase").is_err());
        assert!(Vault::open(&json, "correct horse battery staple").is_ok());
    }

    #[test]
    fn capability_gating() {
        let mut v = Vault::create("pw");
        // grant the wallet app only its own seed namespace
        v.grant("wallet", "wallet/*");
        v.put("wallet", "wallet/seed", b"deadbeef").unwrap();
        // wallet can read its own secret
        assert_eq!(v.get("wallet", "wallet/seed").unwrap(), b"deadbeef");
        // a different app with no grant is denied (put AND get)
        assert_eq!(v.put("miner", "wallet/seed", b"x"), Err(VaultError::Denied("miner".into(), "wallet/seed".into())));
        assert_eq!(v.get("miner", "wallet/seed"), Err(VaultError::Denied("miner".into(), "wallet/seed".into())));
    }

    #[test]
    fn audit_records_every_access() {
        let mut v = Vault::create("pw");
        v.grant("wallet", "*");
        v.put("wallet", "api/key", b"k").unwrap();
        let _ = v.get("wallet", "api/key");
        let _ = v.get("intruder", "api/key"); // denied, still audited
        let log = v.audit_log();
        assert_eq!(log.len(), 3);
        assert!(log.iter().any(|e| e.app == "intruder" && !e.allowed));
        assert!(log.iter().any(|e| e.app == "wallet" && e.action == "get" && e.allowed));
    }

    #[test]
    fn persistence_roundtrip() {
        let mut v = Vault::create("pw");
        v.grant("app", "*");
        v.put("app", "token", b"xyz").unwrap();
        let json = v.to_json().unwrap();
        // reopen from disk JSON and read the secret back
        let mut v2 = Vault::open(&json, "pw").unwrap();
        assert_eq!(v2.get("app", "token").unwrap(), b"xyz");
    }
}
