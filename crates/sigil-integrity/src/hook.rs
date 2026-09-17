//! Webhook side: the nodes push `certificate` events to us, signed. `POST /v1/webhooks` on a node registers a
//! receiver URL; the node refuses loopback/private/link-local targets unless started with
//! `SIGIL_WEBHOOK_ALLOW_PRIVATE=1`, and Epsilon's INPUT chain drops unknown ports — so the receiver needs a
//! PUBLIC url and an open port, which is an operator decision. Until then the SSE follower carries the same
//! events; this module is the push path, verified by the same signature the node produces.
use hmac::{Hmac, Mac};
use sha2::Sha256;

pub const SIGNATURE_HEADER: &str = "X-Sigil-Signature";

/// `sha256=<hex>` over the raw body keyed by the registration secret — what the node sends.
pub fn sign(secret: &str, body: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("hmac accepts any key length");
    mac.update(body);
    format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
}

/// Constant-time-enough verification of the header against the body.
pub fn verify(secret: &str, body: &[u8], header: &str) -> bool {
    let want = sign(secret, body);
    want.len() == header.len() && want.bytes().zip(header.bytes()).fold(0u8, |acc, (a, b)| acc | (a ^ b)) == 0
}

/// Register a non-`once` certificate webhook on `base` pointing at `url`. Returns the id the node assigned.
/// Registrations live 1 h in the node's RAM — call again before that.
pub fn register(agent: &ureq::Agent, base: &str, url: &str, secret: &str) -> anyhow::Result<String> {
    let body = serde_json::json!({ "url": url, "secret": secret, "kinds": ["certificate"], "once": false });
    let v: serde_json::Value = agent.post(&format!("{base}/v1/webhooks")).send_json(body)?.into_json()?;
    let d = v.get("data").unwrap_or(&v);
    d.get("id").and_then(|x| x.as_str()).map(|s| s.to_string()).ok_or_else(|| anyhow::anyhow!("no webhook id in {v}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_and_verify_round_trip() {
        let body = br#"{"kind":"certificate","height":1}"#;
        let h = sign("s3cret", body);
        assert!(h.starts_with("sha256="));
        assert!(verify("s3cret", body, &h));
        assert!(!verify("other", body, &h));
        assert!(!verify("s3cret", b"{}", &h));
    }
}
