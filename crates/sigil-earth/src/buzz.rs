//! A Buzz post, byte-compatible with fluxc-mcp's `flux_buzz_post`: canonical bytes are the compact
//! JSON of `[pubkey, created_at, kind, tags, content]`, the id is their BLAKE3, the signature is
//! Ed25519 from the agent identity under `~/.flux-buzz/identity-<agent>.json`.
//! The relay is a social feed; the machine feed stays on sigilgraph.org.

use anyhow::{anyhow, Context, Result};
use ed25519_dalek::{Signer, SigningKey};
use serde_json::{json, Value};
use std::path::PathBuf;

pub const AGENT_ID: &str = "sigil-earth";

pub fn default_relay() -> String {
    std::env::var("FLUX_BUZZ_RELAY").unwrap_or_else(|_| "https://buzz.quillon.xyz".into())
}

fn identity_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/root".into());
    PathBuf::from(home).join(".flux-buzz").join(format!("identity-{AGENT_ID}.json"))
}

fn load_or_create_identity() -> Result<(SigningKey, String)> {
    let path = identity_path();
    if path.exists() {
        let v: Value = serde_json::from_slice(&std::fs::read(&path)?).context("identity json")?;
        let sk_hex = v["sk"].as_str().ok_or_else(|| anyhow!("identity file missing sk"))?;
        let sk: [u8; 32] = hex::decode(sk_hex.trim())?.try_into().map_err(|_| anyhow!("sk must be 32 bytes"))?;
        let key = SigningKey::from_bytes(&sk);
        let pk = hex::encode(key.verifying_key().to_bytes());
        return Ok((key, pk));
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let key = SigningKey::generate(&mut rand::rngs::OsRng);
    let pk = hex::encode(key.verifying_key().to_bytes());
    std::fs::write(&path, json!({"sk": hex::encode(key.to_bytes()), "pk": pk, "agent_id": AGENT_ID}).to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok((key, pk))
}

/// Post `content` to `channel`. Returns the relay's response and the event id.
pub fn post(content: &str, channel: &str) -> Result<Value> {
    let (key, pubkey) = load_or_create_identity()?;
    let tags = vec![
        vec!["c".to_string(), channel.to_string()],
        vec!["client".to_string(), "sigil-earth".to_string()],
        vec!["name".to_string(), AGENT_ID.to_string()],
    ];
    let created_at = crate::time::now_ms();
    let kind = 1u32;
    let canonical = serde_json::to_vec(&json!([pubkey, created_at, kind, tags, content]))?;
    let sig = hex::encode(key.sign(&canonical).to_bytes());
    let id = blake3::hash(&canonical).to_hex().to_string();
    let event = json!({"id": id, "pubkey": pubkey, "created_at": created_at, "kind": kind, "tags": tags, "content": content, "sig": sig});
    let relay = default_relay();
    let agent = ureq::AgentBuilder::new().timeout(std::time::Duration::from_secs(15)).user_agent(crate::UA).build();
    let resp = agent.post(&format!("{relay}/v1/event")).set("content-type", "application/json").send_string(&event.to_string());
    let (status, body) = match resp {
        Ok(r) => (r.status(), r.into_string().unwrap_or_default()),
        Err(ureq::Error::Status(c, r)) => (c, r.into_string().unwrap_or_default()),
        Err(e) => return Err(anyhow!("relay {relay}: {e}")),
    };
    Ok(json!({"relay": relay, "status": status, "event_id": id, "pubkey": pubkey,
              "relay_response": serde_json::from_str::<Value>(&body).unwrap_or(Value::String(body))}))
}
