#!/usr/bin/env python3
"""Verify what clients actually fetch: <base>/<name> plus its detached signature(s), over the network.

usage: verify-live-manifest.py <base_url> <name> [<name> ...]

  <name>.sig2 must verify against KEY 2 (a78061cf…). sigil-top >= 10.6.9 and the wallet >= 1.13.5
              read this file and nothing else.
  <name>.sig  must verify against KEY 1 (150fb84d…) WHILE THE BRIDGE IS ON, i.e. while key 1's seed is
              still on this host (SIGIL_RELEASE_SEED, default /root/.config/sigil/release-sign.seed).
              Installs from before the 2026-10-23 rotation trust only key 1; without this signature
              they cannot reach the build that trusts key 2. Force with SIGIL_RELEASE_BRIDGE=on|off.

Exit 1 on any failure. A body that is not JSON (e.g. the site's HTML fallback page) is reported as
ABSENT, not as a bad signature.
"""
import json, os, sys, time, urllib.request
from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PublicKey
from cryptography.exceptions import InvalidSignature

KEY2 = "a78061cfb9e2b2cdd5e876da5d737ba0edce6ef77ba3b91e83acb892741225ae"
KEY1 = "150fb84d4b2c83e6e81a27f629e60686acf8663be5ce73f46208cce4f5686402"

def bridge_on() -> bool:
    forced = os.environ.get("SIGIL_RELEASE_BRIDGE", "auto")
    if forced in ("on", "off"):
        return forced == "on"
    return os.path.exists(os.environ.get("SIGIL_RELEASE_SEED", "/root/.config/sigil/release-sign.seed"))

def fetch(url: str) -> bytes:
    sep = "&" if "?" in url else "?"
    return urllib.request.urlopen(f"{url}{sep}t={time.time_ns()}", timeout=30).read()

def check(base: str, name: str, suffix: str, key_hex: str) -> str:
    body = fetch(f"{base}/{name}")
    try:
        doc = json.loads(body)
    except ValueError:
        raise SystemExit(f"✗ {base}/{name}: ABSENT (not JSON, {len(body)} B — HTML fallback?)")
    raw = fetch(f"{base}/{name}{suffix}").decode(errors="replace").strip()
    if len(raw) != 128 or any(c not in "0123456789abcdefABCDEF" for c in raw):
        raise SystemExit(f"✗ {base}/{name}{suffix}: ABSENT (not a 128-hex signature, {len(raw)} chars — HTML fallback?)")
    sig = bytes.fromhex(raw)
    Ed25519PublicKey.from_public_bytes(bytes.fromhex(key_hex)).verify(sig, body)
    return str(doc.get("version") or doc.get("product") or "?")

def main() -> int:
    if len(sys.argv) < 3:
        print(__doc__, file=sys.stderr)
        return 2
    base, names = sys.argv[1].rstrip("/"), sys.argv[2:]
    legs = [(".sig2", KEY2, "key 2")] + ([(".sig", KEY1, "key 1 bridge")] if bridge_on() else [])
    host = base.split("//", 1)[-1].split("/", 1)[0]
    bad = 0
    for name in names:
        for suffix, key, label in legs:
            try:
                what = check(base, name, suffix, key)
                print(f"  ✓ {host:28s} {name}{suffix:6s} verifies ({label} {key[:8]}…) — {what}")
            except InvalidSignature:
                bad += 1
                print(f"  ✗ {host:28s} {name}{suffix:6s} does NOT verify against {label} {key[:8]}…")
            except SystemExit as e:
                bad += 1
                print(f"  {e}")
            except Exception as e:  # network, 404, malformed hex
                bad += 1
                print(f"  ✗ {host:28s} {name}{suffix:6s}: {type(e).__name__}: {e}")
    if not bridge_on():
        print("  · bridge over: key 1's .sig is no longer checked (pre-rotation installs fail closed)")
    return 1 if bad else 0

if __name__ == "__main__":
    sys.exit(main())
