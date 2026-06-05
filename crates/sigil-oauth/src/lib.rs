//! sigil-oauth — OAuth2 (authorization-code + PKCE), but with the three central
//! points of trust replaced by SIGIL primitives:
//!
//!  1. **DNS-anchored issuer key.** A classic OAuth2 client trusts the AS because
//!     of its TLS cert + a `/.well-known/openid-configuration` document. Here the
//!     AS publishes its *signing* public key into a DNS TXT record
//!     (`_sigil-oauth.<issuer>`), SQIsign/Ed25519, DoH-fetched and DNSSEC-hardened.
//!     The trust root is the DNS anchor — no CA, no well-known endpoint to spoof.
//!     (Same codec family as `sigil-dns-anchor`'s `v=sigil1` tip anchor.)
//!
//!  2. **Wallet login, no passwords.** The user authenticates by *signing the
//!     authorization request with their SIGIL wallet key*. Ownership of the wallet
//!     IS the credential. No password store, no shared secret to leak.
//!
//!  3. **Offline, post-quantum tokens.** The access token is signed by the AS key.
//!     A resource server verifies it against the DNS-anchored pubkey with zero
//!     round-trips back to the AS — no token-introspection endpoint. Swap `alg=`
//!     in the anchor (ed25519 → sqisign5 / dilithium5) and the whole surface is PQ.
//!
//! And the DNS thesis taken further:
//!
//!  4. **Revocation-via-DNS.** The anchor carries a key **epoch** (`e=`). Tokens are
//!     stamped with the epoch that minted them; verifiers reject any token whose
//!     epoch is below the anchor's. `Issuer::revoke_all()` bumps the epoch, and the
//!     moment the new TXT propagates (cached, ~minutely TTL) *every* old token dies
//!     network-wide — without a per-token introspection call. The trust root
//!     doubles as the kill switch.
//!
//!  5. **Rotating refresh tokens with reuse-detection.** A long-lived refresh token
//!     rotates on each use (family + generation). Replaying a spent generation is
//!     treated as theft and revokes the whole family — the OAuth2 best-practice.
//!
//! Plus DPoP-style proof-of-possession (`cnf`) so a stolen bearer token is useless
//! without the holder's key.
//!
//! Ed25519 is the working hot-path scheme here; the `alg=` field on [`DnsAnchor`]
//! is the crypto-agility hook (drop-in SqiSign5 / Dilithium5 via the same dispatch
//! `sigil-tx` uses). The flow, PKCE, single-use codes, epoch revocation, refresh
//! rotation, and offline verify are real.

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const ANCHOR_VERSION: &str = "sigil-oauth1";
/// Authorization codes are short-lived and single-use.
pub const CODE_TTL_SECS: u64 = 300; // 5 min
/// Access tokens expire — verifiers reject past `exp` with no AS contact.
pub const TOKEN_TTL_SECS: u64 = 3600; // 1 h
/// Refresh tokens are long-lived but rotate on every use.
pub const REFRESH_TTL_SECS: u64 = 2_592_000; // 30 d
/// DPoP proofs must be fresh to thwart replay.
pub const DPOP_SKEW_SECS: u64 = 60;

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
}
fn b3(data: &[u8]) -> [u8; 32] {
    *blake3::hash(data).as_bytes()
}
fn rand32() -> [u8; 32] {
    use rand::Rng;
    let mut s = [0u8; 32];
    rand::thread_rng().fill(&mut s[..]);
    s
}

// ───────────────────────────── errors ─────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OAuthError {
    UnknownClient,
    RedirectMismatch,
    BadWalletSig,
    UnknownCode,
    CodeExpired,
    CodeUsed,
    ClientMismatch,
    PkceMismatch,
    BadToken,
    BadSignature,
    IssuerMismatch,
    TokenExpired,
    KeyRevoked,
    NotRefreshToken,
    RefreshReuse,
    UnsupportedAlg,
    BadAnchor,
    NoConfirmation,
    HolderMismatch,
    DpopStale,
}
impl std::fmt::Display for OAuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for OAuthError {}

// ───────────────────────────── keys ─────────────────────────────

/// An Ed25519 keypair. Used for the AS signing key, wallet keys, and DPoP holders.
#[derive(Clone)]
pub struct Keypair {
    sk: SigningKey,
    pub vk: VerifyingKey,
}
impl Keypair {
    pub fn from_seed(seed: &[u8; 32]) -> Self {
        let sk = SigningKey::from_bytes(seed);
        let vk = sk.verifying_key();
        Self { sk, vk }
    }
    pub fn generate() -> Self {
        Self::from_seed(&rand32())
    }
    pub fn pubkey(&self) -> [u8; 32] {
        self.vk.to_bytes()
    }
    pub fn pubkey_hex(&self) -> String {
        hex::encode(self.vk.to_bytes())
    }
    pub fn sign(&self, msg: &[u8]) -> [u8; 64] {
        self.sk.sign(msg).to_bytes()
    }
}

/// Verify an Ed25519 signature; never panics on malformed inputs.
pub fn verify_sig(pk: &[u8; 32], msg: &[u8], sig: &[u8; 64]) -> bool {
    match VerifyingKey::from_bytes(pk) {
        Ok(vk) => vk.verify(msg, &Signature::from_bytes(sig)).is_ok(),
        Err(_) => false,
    }
}

/// SIGIL wallet address = `sgl` ++ hex(BLAKE3(pubkey)). The token's `sub`.
pub fn wallet_id(pk: &[u8; 32]) -> String {
    format!("sgl{}", hex::encode(b3(pk)))
}

// ─────────────────── crypto agility: the issuer signing key ───────────────────
// The Authorization Server key (published in the DNS anchor, signs the tokens) is
// pluggable: Ed25519 today, real SQIsign Level-5 for post-quantum. Wallet/DPoP keys
// stay Ed25519 (the SIGIL wallet). NEVER hardcode the scheme in the verify path —
// dispatch on `alg` so dropping in dilithium5 later is a new arm, not a rewrite.
pub enum IssuerSigner {
    Ed25519(Keypair),
    Sqisign5 { pk: Vec<u8>, sk: Vec<u8> },
}
impl IssuerSigner {
    /// Fresh SQIsign Level-5 issuer key (129B pubkey / 292B sigs).
    pub fn sqisign5() -> Self {
        let (sk, pk) = flux_sqisign::keygen(); // NB: keygen returns (secret, public) in that order
        IssuerSigner::Sqisign5 { pk, sk }
    }
    pub fn from_sqisign5(pk: Vec<u8>, sk: Vec<u8>) -> Self { IssuerSigner::Sqisign5 { pk, sk } }
    pub fn alg(&self) -> &'static str {
        match self { IssuerSigner::Ed25519(_) => "ed25519", IssuerSigner::Sqisign5 { .. } => "sqisign5" }
    }
    pub fn pubkey(&self) -> Vec<u8> {
        match self {
            IssuerSigner::Ed25519(k) => k.pubkey().to_vec(),
            IssuerSigner::Sqisign5 { pk, .. } => pk.clone(),
        }
    }
    pub fn sign(&self, msg: &[u8]) -> Vec<u8> {
        match self {
            IssuerSigner::Ed25519(k) => k.sign(msg).to_vec(),
            IssuerSigner::Sqisign5 { pk, sk } => flux_sqisign::sign(msg, sk, pk).unwrap_or_default(),
        }
    }
}

/// Verify an issuer signature by algorithm — the single agility dispatch point.
pub fn issuer_verify(alg: &str, pk: &[u8], msg: &[u8], sig: &[u8]) -> bool {
    match alg {
        "ed25519" => {
            let (pk32, sig64): ([u8; 32], [u8; 64]) = match (pk.try_into(), sig.try_into()) {
                (Ok(p), Ok(s)) => (p, s),
                _ => return false,
            };
            verify_sig(&pk32, msg, &sig64)
        }
        "sqisign5" => flux_sqisign::verify(msg, sig, pk).unwrap_or(false),
        _ => false,
    }
}

// ─────────────────────── DNS anchor (the trust root) ───────────────────────

/// What the Authorization Server publishes at `_sigil-oauth.<issuer>` TXT.
/// A client fetches it over DoH, then verifies every token offline against it.
/// `epoch` is the revocation counter — bumping it (republishing the TXT) kills
/// every token minted under a lower epoch.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct DnsAnchor {
    pub issuer: String,
    pub key_id: String,
    pub pubkey: Vec<u8>,       // 32B (ed25519) or 129B (sqisign5) — variable for crypto agility
    pub alg: String, // "ed25519" | "sqisign5" | "dilithium5"
    pub epoch: u64,
}
impl DnsAnchor {
    /// Build the anchor from a raw issuer pubkey + alg. Use `Issuer::anchor()` normally.
    pub fn for_issuer(issuer: &str, pubkey: &[u8], alg: &str, epoch: u64) -> Self {
        Self {
            issuer: issuer.to_string(),
            key_id: hex::encode(&b3(pubkey)[..4]),
            pubkey: pubkey.to_vec(),
            alg: alg.to_string(),
            epoch,
        }
    }
    /// Encode to the single-TXT wire string (`v=… iss=… k=… alg=… e=… p=…`).
    pub fn to_txt(&self) -> String {
        format!(
            "v={} iss={} k={} alg={} e={} p={}",
            ANCHOR_VERSION,
            self.issuer,
            self.key_id,
            self.alg,
            self.epoch,
            hex::encode(&self.pubkey)
        )
    }
    /// Parse + structurally validate a TXT string back into an anchor.
    pub fn from_txt(s: &str) -> Result<Self, OAuthError> {
        let mut iss = None;
        let mut k = None;
        let mut alg = None;
        let mut p = None;
        let mut epoch = 0u64;
        let mut ver_ok = false;
        for tok in s.split_whitespace() {
            let (key, val) = tok.split_once('=').ok_or(OAuthError::BadAnchor)?;
            match key {
                "v" => ver_ok = val == ANCHOR_VERSION,
                "iss" => iss = Some(val.to_string()),
                "k" => k = Some(val.to_string()),
                "alg" => alg = Some(val.to_string()),
                "e" => epoch = val.parse().map_err(|_| OAuthError::BadAnchor)?,
                "p" => p = Some(val.to_string()),
                _ => {}
            }
        }
        if !ver_ok {
            return Err(OAuthError::BadAnchor);
        }
        let pubkey = hex::decode(p.ok_or(OAuthError::BadAnchor)?).map_err(|_| OAuthError::BadAnchor)?;
        Ok(Self {
            issuer: iss.ok_or(OAuthError::BadAnchor)?,
            key_id: k.ok_or(OAuthError::BadAnchor)?,
            pubkey,
            alg: alg.ok_or(OAuthError::BadAnchor)?,
            epoch,
        })
    }
}

/// How a client obtains an issuer's anchor. Real impl is a DoH `fetch()` of the
/// TXT record; tests use [`StaticResolver`]. Pluggable so the browser/WASM client
/// and the server share one verification path.
pub trait AnchorResolver {
    fn resolve(&self, issuer: &str) -> Option<DnsAnchor>;
}

/// In-memory resolver (tests, and a warm cache in front of DoH).
#[derive(Default, Clone)]
pub struct StaticResolver(pub HashMap<String, DnsAnchor>);
impl StaticResolver {
    pub fn with(mut self, a: DnsAnchor) -> Self {
        self.0.insert(a.issuer.clone(), a);
        self
    }
}
impl AnchorResolver for StaticResolver {
    fn resolve(&self, issuer: &str) -> Option<DnsAnchor> {
        self.0.get(issuer).cloned()
    }
}

// ───────────────────────── PKCE (client side) ─────────────────────────

/// Generate a PKCE (verifier, challenge) pair. Challenge = hex(SHA-256(verifier)),
/// method `S256`. The client keeps the verifier secret until the token exchange.
pub fn pkce_pair() -> (String, String) {
    let verifier = hex::encode(rand32());
    let challenge = hex::encode(Sha256::digest(verifier.as_bytes()));
    (verifier, challenge)
}

// ───────────────────────── flow messages ─────────────────────────

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AuthRequest {
    pub client_id: String,
    pub redirect_uri: String,
    pub scope: String,
    pub code_challenge: String,
    pub code_challenge_method: String, // "S256"
    pub state: String,
    pub nonce: String,
}
impl AuthRequest {
    /// Canonical digest the wallet signs to prove ownership ("the login").
    pub fn digest(&self) -> [u8; 32] {
        b3(&serde_json::to_vec(self).expect("auth req serializes"))
    }
}

/// The user's proof that they control the wallet — replaces a password.
#[derive(Clone)]
pub struct WalletAssertion {
    pub wallet_pubkey: [u8; 32],
    pub sig: [u8; 64],
}
impl WalletAssertion {
    pub fn sign(wallet: &Keypair, req: &AuthRequest) -> Self {
        Self {
            wallet_pubkey: wallet.pubkey(),
            sig: wallet.sign(&req.digest()),
        }
    }
}

#[derive(Clone)]
pub struct TokenRequest {
    pub code: String,
    pub code_verifier: String,
    pub client_id: String,
    /// Optional DPoP binding: the key the token will be bound to (`cnf`).
    pub holder_pubkey: Option<[u8; 32]>,
}

/// What `/token` and `/refresh` return.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TokenResponse {
    pub access_token: String,
    pub refresh_token: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct TokenClaims {
    pub typ: String, // "access"
    pub iss: String,
    pub sub: String, // wallet id
    pub aud: String, // client_id
    pub scope: String,
    pub iat: u64,
    pub exp: u64,
    pub epoch: u64, // key-epoch that minted this token
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cnf: Option<String>, // hex(holder pubkey) for proof-of-possession
}
impl TokenClaims {
    /// Space-delimited scope membership test.
    pub fn has_scope(&self, s: &str) -> bool {
        self.scope.split_whitespace().any(|x| x == s)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct RefreshClaims {
    typ: String, // "refresh"
    iss: String,
    sub: String,
    aud: String,
    scope: String,
    family: String,
    gen: u64,
    iat: u64,
    exp: u64,
    epoch: u64,
    cnf: Option<String>,
}

// ───────────────────────── signed-blob codec ─────────────────────────

/// `hex(json).hex(sig)` — JWT-shaped but Ed25519/PQ-signed (not HMAC/RSA).
fn sign_blob<T: Serialize>(signer: &IssuerSigner, v: &T) -> String {
    let body = serde_json::to_vec(v).expect("blob serialize");
    let sig = signer.sign(&b3(&body));
    format!("{}.{}", hex::encode(&body), hex::encode(sig))
}
fn open_blob<T: DeserializeOwned>(blob: &str, alg: &str, pk: &[u8]) -> Result<T, OAuthError> {
    let (b, s) = blob.split_once('.').ok_or(OAuthError::BadToken)?;
    let body = hex::decode(b).map_err(|_| OAuthError::BadToken)?;
    let sig = hex::decode(s).map_err(|_| OAuthError::BadToken)?;
    if !issuer_verify(alg, pk, &b3(&body), &sig) {
        return Err(OAuthError::BadSignature);
    }
    serde_json::from_slice(&body).map_err(|_| OAuthError::BadToken)
}

/// Verify an access token **offline** against a DNS-anchored issuer key. No AS
/// contact. Rejects wrong-issuer, expired, and **epoch-revoked** tokens.
pub fn verify_token(token: &str, anchor: &DnsAnchor, now_ts: u64) -> Result<TokenClaims, OAuthError> {
    // alg dispatch lives in issuer_verify (ed25519 / sqisign5); unknown algs fail closed there.
    let claims: TokenClaims = open_blob(token, &anchor.alg, &anchor.pubkey)?;
    if claims.typ != "access" {
        return Err(OAuthError::BadToken);
    }
    if claims.iss != anchor.issuer {
        return Err(OAuthError::IssuerMismatch);
    }
    if claims.epoch < anchor.epoch {
        return Err(OAuthError::KeyRevoked);
    }
    if now_ts > claims.exp {
        return Err(OAuthError::TokenExpired);
    }
    Ok(claims)
}

/// Resolve the issuer's anchor via DNS, then verify — the full client path.
pub fn verify_token_via_dns<R: AnchorResolver>(
    token: &str,
    resolver: &R,
    now_ts: u64,
) -> Result<TokenClaims, OAuthError> {
    let (b, _) = token.split_once('.').ok_or(OAuthError::BadToken)?;
    let body = hex::decode(b).map_err(|_| OAuthError::BadToken)?;
    let claims: TokenClaims = serde_json::from_slice(&body).map_err(|_| OAuthError::BadToken)?;
    let anchor = resolver.resolve(&claims.iss).ok_or(OAuthError::IssuerMismatch)?;
    verify_token(token, &anchor, now_ts)
}

// ───────────────────────── DPoP (proof-of-possession) ─────────────────────────

/// Holder signs `(method|url|blake3(token)|ts)` — proves it holds the `cnf` key.
pub fn make_dpop(holder: &Keypair, method: &str, url: &str, token: &str, ts: u64) -> [u8; 64] {
    let th = hex::encode(b3(token.as_bytes()));
    holder.sign(&b3(format!("{method}|{url}|{th}|{ts}").as_bytes()))
}

/// Resource server: bearer token alone is not enough; the caller must prove `cnf`.
#[allow(clippy::too_many_arguments)]
pub fn verify_dpop(
    claims: &TokenClaims,
    holder_pk: &[u8; 32],
    method: &str,
    url: &str,
    token: &str,
    ts: u64,
    now_ts: u64,
    sig: &[u8; 64],
) -> Result<(), OAuthError> {
    let cnf = claims.cnf.as_ref().ok_or(OAuthError::NoConfirmation)?;
    if *cnf != hex::encode(holder_pk) {
        return Err(OAuthError::HolderMismatch);
    }
    if now_ts.abs_diff(ts) > DPOP_SKEW_SECS {
        return Err(OAuthError::DpopStale);
    }
    let th = hex::encode(b3(token.as_bytes()));
    if !verify_sig(holder_pk, &b3(format!("{method}|{url}|{th}|{ts}").as_bytes()), sig) {
        return Err(OAuthError::BadSignature);
    }
    Ok(())
}

// ───────────────────────── the Authorization Server ─────────────────────────

struct StoredCode {
    sub: String,
    client_id: String,
    code_challenge: String,
    scope: String,
    redirect_uri: String,
    exp: u64,
    used: bool,
    holder_pubkey: Option<[u8; 32]>,
}

#[derive(Clone)]
pub struct ClientReg {
    pub redirect_uris: Vec<String>,
}

/// The OAuth2 Authorization Server. Its anchor ([`Issuer::anchor`]) is what gets
/// published to DNS; everything else is the in-process flow state.
pub struct Issuer {
    pub issuer: String,
    signer: IssuerSigner,
    epoch: u64,
    codes: HashMap<String, StoredCode>,
    clients: HashMap<String, ClientReg>,
    refresh_families: HashMap<String, u64>, // family -> current generation
}
impl Issuer {
    pub fn new(issuer: &str, kp: Keypair) -> Self {
        Self::with_signer(issuer, IssuerSigner::Ed25519(kp))
    }
    /// Post-quantum issuer: real SQIsign Level-5 key (anchor + tokens are sqisign5).
    pub fn new_sqisign(issuer: &str, signer: IssuerSigner) -> Self {
        Self::with_signer(issuer, signer)
    }
    pub fn with_signer(issuer: &str, signer: IssuerSigner) -> Self {
        Self {
            issuer: issuer.to_string(),
            signer,
            epoch: 0,
            codes: HashMap::new(),
            clients: HashMap::new(),
            refresh_families: HashMap::new(),
        }
    }
    /// The DNS TXT anchor this AS publishes — the client's trust root.
    pub fn anchor(&self) -> DnsAnchor {
        DnsAnchor::for_issuer(&self.issuer, &self.signer.pubkey(), self.signer.alg(), self.epoch)
    }
    pub fn epoch(&self) -> u64 {
        self.epoch
    }
    /// Rotate the key epoch — the DNS kill switch. Republishing the new anchor
    /// invalidates every token minted under the old epoch the moment it propagates
    /// (cached, ~minutely TTL), with no per-token introspection. Also drops all
    /// refresh families.
    pub fn revoke_all(&mut self) {
        self.epoch += 1;
        self.refresh_families.clear();
    }
    pub fn register_client(&mut self, client_id: &str, redirect_uris: Vec<String>) {
        self.clients.insert(client_id.to_string(), ClientReg { redirect_uris });
    }

    /// Step 1 — authorize. Verify the wallet assertion ("login"), enforce the
    /// registered redirect, mint a single-use, PKCE-bound authorization code.
    pub fn authorize(&mut self, req: &AuthRequest, assertion: &WalletAssertion) -> Result<String, OAuthError> {
        let reg = self.clients.get(&req.client_id).ok_or(OAuthError::UnknownClient)?;
        if !reg.redirect_uris.contains(&req.redirect_uri) {
            return Err(OAuthError::RedirectMismatch);
        }
        if req.code_challenge_method != "S256" || req.code_challenge.is_empty() {
            return Err(OAuthError::PkceMismatch);
        }
        if !verify_sig(&assertion.wallet_pubkey, &req.digest(), &assertion.sig) {
            return Err(OAuthError::BadWalletSig);
        }
        let code = hex::encode(rand32());
        self.codes.insert(
            code.clone(),
            StoredCode {
                sub: wallet_id(&assertion.wallet_pubkey),
                client_id: req.client_id.clone(),
                code_challenge: req.code_challenge.clone(),
                scope: req.scope.clone(),
                redirect_uri: req.redirect_uri.clone(),
                exp: now() + CODE_TTL_SECS,
                used: false,
                holder_pubkey: None,
            },
        );
        Ok(code)
    }

    /// Build a fresh access+refresh pair for a (sub, aud, scope). Pure — the caller
    /// records the refresh family in `refresh_families`.
    fn mint_pair(
        &self,
        sub: &str,
        aud: &str,
        scope: &str,
        cnf: Option<String>,
        family: (String, u64),
        now_ts: u64,
    ) -> TokenResponse {
        let access = sign_blob(
            &self.signer,
            &TokenClaims {
                typ: "access".into(),
                iss: self.issuer.clone(),
                sub: sub.into(),
                aud: aud.into(),
                scope: scope.into(),
                iat: now_ts,
                exp: now_ts + TOKEN_TTL_SECS,
                epoch: self.epoch,
                cnf: cnf.clone(),
            },
        );
        let refresh = sign_blob(
            &self.signer,
            &RefreshClaims {
                typ: "refresh".into(),
                iss: self.issuer.clone(),
                sub: sub.into(),
                aud: aud.into(),
                scope: scope.into(),
                family: family.0,
                gen: family.1,
                iat: now_ts,
                exp: now_ts + REFRESH_TTL_SECS,
                epoch: self.epoch,
                cnf,
            },
        );
        TokenResponse { access_token: access, refresh_token: refresh }
    }

    /// Step 2 — token. Verify PKCE (S256), enforce single-use, mint a signed,
    /// offline-verifiable access token + a rotating refresh token.
    pub fn token(&mut self, tr: &TokenRequest) -> Result<TokenResponse, OAuthError> {
        let (sub, aud, scope);
        {
            let c = self.codes.get_mut(&tr.code).ok_or(OAuthError::UnknownCode)?;
            if c.used {
                return Err(OAuthError::CodeUsed);
            }
            if now() > c.exp {
                return Err(OAuthError::CodeExpired);
            }
            if c.client_id != tr.client_id {
                return Err(OAuthError::ClientMismatch);
            }
            let computed = hex::encode(Sha256::digest(tr.code_verifier.as_bytes()));
            if computed != c.code_challenge {
                return Err(OAuthError::PkceMismatch);
            }
            c.used = true;
            c.holder_pubkey = tr.holder_pubkey;
            sub = c.sub.clone();
            aud = c.client_id.clone();
            scope = c.scope.clone();
        }
        let cnf = tr.holder_pubkey.map(hex::encode);
        let family = hex::encode(rand32());
        self.refresh_families.insert(family.clone(), 0);
        Ok(self.mint_pair(&sub, &aud, &scope, cnf, (family, 0), now()))
    }

    /// Exchange a refresh token for a fresh pair. Rotates the family generation;
    /// replaying a spent generation is theft → the whole family is revoked.
    pub fn refresh(&mut self, refresh_token: &str, now_ts: u64) -> Result<TokenResponse, OAuthError> {
        let rc: RefreshClaims = open_blob(refresh_token, self.signer.alg(), &self.signer.pubkey())?;
        if rc.typ != "refresh" {
            return Err(OAuthError::NotRefreshToken);
        }
        if now_ts > rc.exp {
            return Err(OAuthError::TokenExpired);
        }
        if rc.epoch < self.epoch {
            return Err(OAuthError::KeyRevoked);
        }
        match self.refresh_families.get(&rc.family).copied() {
            None => return Err(OAuthError::RefreshReuse), // unknown or already-revoked family
            Some(g) if g != rc.gen => {
                // a spent generation was replayed → assume theft, kill the family
                self.refresh_families.remove(&rc.family);
                return Err(OAuthError::RefreshReuse);
            }
            Some(_) => {}
        }
        let new_gen = rc.gen + 1;
        self.refresh_families.insert(rc.family.clone(), new_gen);
        Ok(self.mint_pair(&rc.sub, &rc.aud, &rc.scope, rc.cnf.clone(), (rc.family, new_gen), now_ts))
    }

    /// Issue a **role / capability credential** — a signed access token the AS
    /// vouches for directly (no PKCE, no wallet login, no DPoP). This is the
    /// *registrar* primitive: e.g.
    /// `issue_credential("sglu…agent", "sigil-university", "university:professor", 30*86400)`
    /// asserts "this subject holds the professor role", verifiable OFFLINE against
    /// the DNS anchor like any access token. Stamped with the current epoch, so
    /// `revoke_all()` (a DNS epoch bump) kills outstanding credentials too. Use a
    /// longer `ttl_secs` than an access token (a role doesn't expire hourly).
    pub fn issue_credential(&self, sub: &str, aud: &str, scope: &str, ttl_secs: u64) -> String {
        let t = now();
        sign_blob(
            &self.signer,
            &TokenClaims {
                typ: "access".into(),
                iss: self.issuer.clone(),
                sub: sub.into(),
                aud: aud.into(),
                scope: scope.into(),
                iat: t,
                exp: t + ttl_secs,
                epoch: self.epoch,
                cnf: None,
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> (Issuer, Keypair, DnsAnchor) {
        let issuer = Issuer::new("auth.sigilgraph.com", Keypair::from_seed(&[7u8; 32]));
        let anchor = issuer.anchor();
        let wallet = Keypair::from_seed(&[9u8; 32]);
        (issuer, wallet, anchor)
    }
    fn req(challenge: &str) -> AuthRequest {
        AuthRequest {
            client_id: "c".into(),
            redirect_uri: "https://c/cb".into(),
            scope: "dex:trade send:read".into(),
            code_challenge: challenge.into(),
            code_challenge_method: "S256".into(),
            state: "xyz".into(),
            nonce: "n1".into(),
        }
    }
    /// register client "c", run authorize+token, return the response.
    fn run_flow(iss: &mut Issuer, wallet: &Keypair, holder: Option<[u8; 32]>) -> TokenResponse {
        iss.register_client("c", vec!["https://c/cb".into()]);
        let (verifier, challenge) = pkce_pair();
        let r = req(&challenge);
        let code = iss.authorize(&r, &WalletAssertion::sign(wallet, &r)).unwrap();
        iss.token(&TokenRequest {
            code,
            code_verifier: verifier,
            client_id: "c".into(),
            holder_pubkey: holder,
        })
        .unwrap()
    }

    #[test]
    fn full_flow_offline_verify() {
        let (mut iss, wallet, anchor) = setup();
        let resp = run_flow(&mut iss, &wallet, None);
        let claims = verify_token(&resp.access_token, &anchor, now()).unwrap();
        assert_eq!(claims.sub, wallet_id(&wallet.pubkey()));
        assert_eq!(claims.iss, "auth.sigilgraph.com");
        assert_eq!(claims.aud, "c");
        assert!(claims.has_scope("dex:trade"));
        assert!(!claims.has_scope("admin"));
    }

    #[test]
    fn verify_via_dns_resolver() {
        let (mut iss, wallet, anchor) = setup();
        let resp = run_flow(&mut iss, &wallet, None);
        let resolver = StaticResolver::default().with(anchor);
        let claims = verify_token_via_dns(&resp.access_token, &resolver, now()).unwrap();
        assert_eq!(claims.sub, wallet_id(&wallet.pubkey()));
    }

    #[test]
    fn wrong_wallet_sig_rejected() {
        let (mut iss, _wallet, _) = setup();
        iss.register_client("c", vec!["https://c/cb".into()]);
        let (_v, ch) = pkce_pair();
        let r = req(&ch);
        let attacker = Keypair::from_seed(&[1u8; 32]);
        let mut other = r.clone();
        other.nonce = "different".into();
        let bad = WalletAssertion { wallet_pubkey: attacker.pubkey(), sig: attacker.sign(&other.digest()) };
        assert_eq!(iss.authorize(&r, &bad), Err(OAuthError::BadWalletSig));
    }

    #[test]
    fn pkce_mismatch_rejected() {
        let (mut iss, wallet, _) = setup();
        iss.register_client("c", vec!["https://c/cb".into()]);
        let (_verifier, challenge) = pkce_pair();
        let r = req(&challenge);
        let code = iss.authorize(&r, &WalletAssertion::sign(&wallet, &r)).unwrap();
        let wrong = iss.token(&TokenRequest {
            code,
            code_verifier: "not-the-verifier".into(),
            client_id: "c".into(),
            holder_pubkey: None,
        });
        assert_eq!(wrong, Err(OAuthError::PkceMismatch));
    }

    #[test]
    fn code_is_single_use() {
        let (mut iss, wallet, _) = setup();
        iss.register_client("c", vec!["https://c/cb".into()]);
        let (v, ch) = pkce_pair();
        let r = req(&ch);
        let code = iss.authorize(&r, &WalletAssertion::sign(&wallet, &r)).unwrap();
        let tr = TokenRequest { code, code_verifier: v, client_id: "c".into(), holder_pubkey: None };
        assert!(iss.token(&tr).is_ok());
        assert_eq!(iss.token(&tr), Err(OAuthError::CodeUsed));
    }

    #[test]
    fn tampered_token_rejected() {
        let (mut iss, wallet, anchor) = setup();
        let resp = run_flow(&mut iss, &wallet, None);
        let mut bytes = resp.access_token.into_bytes();
        bytes[0] = if bytes[0] == b'a' { b'b' } else { b'a' };
        let tampered = String::from_utf8(bytes).unwrap();
        assert!(matches!(
            verify_token(&tampered, &anchor, now()),
            Err(OAuthError::BadSignature) | Err(OAuthError::BadToken)
        ));
    }

    #[test]
    fn rogue_issuer_key_rejected() {
        let (mut iss, wallet, real_anchor) = setup();
        let resp = run_flow(&mut iss, &wallet, None);
        let rogue = DnsAnchor::for_issuer("auth.sigilgraph.com", &Keypair::from_seed(&[42u8; 32]).pubkey(), "ed25519", 0);
        assert_eq!(verify_token(&resp.access_token, &rogue, now()), Err(OAuthError::BadSignature));
        assert!(verify_token(&resp.access_token, &real_anchor, now()).is_ok());
    }

    #[test]
    fn expired_token_rejected() {
        let (mut iss, wallet, anchor) = setup();
        let resp = run_flow(&mut iss, &wallet, None);
        let far_future = now() + TOKEN_TTL_SECS + 10;
        assert_eq!(verify_token(&resp.access_token, &anchor, far_future), Err(OAuthError::TokenExpired));
    }

    #[test]
    fn redirect_uri_enforced() {
        let (mut iss, wallet, _) = setup();
        iss.register_client("c", vec!["https://c/cb".into()]);
        let (_v, ch) = pkce_pair();
        let mut r = req(&ch);
        r.redirect_uri = "https://evil/cb".into();
        assert_eq!(iss.authorize(&r, &WalletAssertion::sign(&wallet, &r)), Err(OAuthError::RedirectMismatch));
    }

    #[test]
    fn dns_anchor_txt_roundtrip() {
        let a = DnsAnchor::for_issuer("auth.sigilgraph.com", &Keypair::from_seed(&[3u8; 32]).pubkey(), "ed25519", 7);
        let txt = a.to_txt();
        assert!(txt.contains("e=7"));
        let b = DnsAnchor::from_txt(&txt).unwrap();
        assert_eq!(a, b);
        assert!(DnsAnchor::from_txt("v=wrongver iss=x k=y alg=ed25519 e=0 p=00").is_err());
    }

    #[test]
    fn dpop_binds_holder() {
        let (mut iss, wallet, anchor) = setup();
        let holder = Keypair::from_seed(&[11u8; 32]);
        let resp = run_flow(&mut iss, &wallet, Some(holder.pubkey()));
        let claims = verify_token(&resp.access_token, &anchor, now()).unwrap();
        let ts = now();
        let proof = make_dpop(&holder, "POST", "https://api/send", &resp.access_token, ts);
        assert!(verify_dpop(&claims, &holder.pubkey(), "POST", "https://api/send", &resp.access_token, ts, ts, &proof).is_ok());
        let thief = Keypair::from_seed(&[12u8; 32]);
        assert_eq!(
            verify_dpop(&claims, &thief.pubkey(), "POST", "https://api/send", &resp.access_token, ts, ts, &proof),
            Err(OAuthError::HolderMismatch)
        );
    }

    #[test]
    fn epoch_revokes_old_tokens() {
        // THE revocation-via-DNS property: bumping the anchor epoch kills old tokens.
        let (mut iss, wallet, _) = setup();
        let resp = run_flow(&mut iss, &wallet, None);
        assert!(verify_token(&resp.access_token, &iss.anchor(), now()).is_ok());
        iss.revoke_all(); // AS rotates the key epoch + republishes the TXT
        let new_anchor = iss.anchor();
        assert_eq!(new_anchor.epoch, 1);
        assert_eq!(verify_token(&resp.access_token, &new_anchor, now()), Err(OAuthError::KeyRevoked));
    }

    #[test]
    fn refresh_rotates_and_detects_reuse() {
        let (mut iss, wallet, anchor) = setup();
        let resp = run_flow(&mut iss, &wallet, None);
        // rotate: the refresh token yields a fresh, valid access token
        let resp2 = iss.refresh(&resp.refresh_token, now()).unwrap();
        assert!(verify_token(&resp2.access_token, &anchor, now()).is_ok());
        // replaying the SPENT (gen-0) refresh token is theft → family revoked
        assert_eq!(iss.refresh(&resp.refresh_token, now()), Err(OAuthError::RefreshReuse));
        // and the rotated (gen-1) token is now dead too — the whole family is killed
        assert_eq!(iss.refresh(&resp2.refresh_token, now()), Err(OAuthError::RefreshReuse));
    }

    #[test]
    fn cross_use_rejected() {
        let (mut iss, wallet, anchor) = setup();
        let resp = run_flow(&mut iss, &wallet, None);
        // an access token fed to /refresh is rejected (won't parse as refresh, or typ guard)
        assert!(matches!(
            iss.refresh(&resp.access_token, now()),
            Err(OAuthError::BadToken) | Err(OAuthError::NotRefreshToken)
        ));
        // a refresh token fed to the access verifier is rejected by the typ guard
        assert_eq!(verify_token(&resp.refresh_token, &anchor, now()), Err(OAuthError::BadToken));
    }

    #[test]
    fn issued_credential_verifies_and_revokes() {
        // the registrar primitive: AS vouches for a subject's role directly.
        let mut iss = Issuer::new("registrar.sigilgraph.quillon.xyz", Keypair::from_seed(&[5u8; 32]));
        let cred = iss.issue_credential("sglu_agent", "sigil-university", "university:professor", 86_400);
        let c = verify_token(&cred, &iss.anchor(), now()).unwrap();
        assert_eq!(c.sub, "sglu_agent");
        assert!(c.has_scope("university:professor"));
        assert!(!c.has_scope("university:auditor"));
        iss.revoke_all(); // a DNS epoch bump kills issued credentials too
        assert_eq!(verify_token(&cred, &iss.anchor(), now()), Err(OAuthError::KeyRevoked));
    }
}
