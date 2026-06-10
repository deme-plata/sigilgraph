# sigil-rpcd auth — client signing contract (v0.35 gate, 0.36.1 rollout)

Every **money-mutating** flat route on `sigil-rpcd` requires the caller to prove
control of the wallet whose funds move. A request must carry:

- `sig` — 128-hex (64-byte) Ed25519 signature, **by the actor wallet's key**, over
  the canonical message below.
- `req_nonce` — a strictly-increasing per-wallet integer (use a millisecond
  timestamp, e.g. `date +%s%3N`). The server rejects `req_nonce <= last_seen`.

The wallet **is** the Ed25519 public key (32 bytes). Derive the keypair from a
32-byte seed exactly as the node does (`sigil_oauth::Keypair::from_seed`).

## Canonical message
```
sigil-rpc/v1|<action>|<field0>|<field1>|...|nonce=<req_nonce>
```
UTF-8, `|`-joined, domain-tagged. A signature for one action cannot authorize
another. **Field order is fixed per route and MUST match the server exactly:**

| Route | action | ordered fields | actor (signer) |
|---|---|---|---|
| `POST /swap` | `swap` | `from_hex` `pool_hex` `dir`(`AtoB`\|`BtoA`) `amount_in` `min_out` | `from` |
| `POST /add_liquidity` | `add_liquidity` | `from_hex` `pool_hex` `amount_a` `amount_b` | `from` |
| `POST /deploy_token` | `deploy_token` | `symbol` `supply` `to_hex` | `to` (creator) |
| `POST /credit` | `credit` | `operator_pool_hex` `pool_amount` `verifiers_csv` | `operator_pool` |
| `POST /mine` (legacy) | `mine` | `miner_hex` `header` `pow_nonce` | `miner` |

`verifiers_csv` = the verifier wallet hexes joined by `,` in submission order.
Numeric fields are their base-10 string form.

> `/mining/submit` is **not** auth-gated — it's gated by the PoW/VDF check
> (`check_submission`), and the miner wallet is bound into the header at solve
> time. `/onboard` is **not** signable (cold start) — it's protected by a finite
> faucet (debits OPERATOR, never mints) + a per-IP rate-limit
> (`SIGIL_ONBOARD_MAX_PER_HOUR`, default 3).

## Reference signer
`sigil-sign` (in `sigil-rpc`) is the contract, executable — it reuses the exact
server crypto, so its output always verifies:
```
sigil-sign <seed_hex32> <req_nonce> <action> [field ...]
  → {"wallet":"<hex>","req_nonce":<n>,"sig":"<128-hex>"}
```
Example `/swap`:
```bash
N=$(date +%s%3N)
SIG=$(sigil-sign "$SEED" "$N" swap "$FROM" "$POOL" AtoB 1000 1 | jq -r .sig)
curl -s "$RPC/swap" -d "{\"from\":\"$FROM\",\"pool\":\"$POOL\",\"dir\":\"AtoB\",\"amount_in\":1000,\"min_out\":1,\"req_nonce\":$N,\"sig\":\"$SIG\"}"
```
`scripts/swarm-money-round.sh` is a working consumer.

## Migration
- `SIGIL_RPC_NO_AUTH=1` on the daemon disables the gate — **dev / migration ONLY**,
  never on the public `:8099`.
- Web clients: derive the keypair in-browser from the wallet seed (`@noble/ed25519`
  + the same `from_seed` derivation), build the canonical message, sign, attach
  `sig`+`req_nonce`. The React wallet's read-only/snapshot paths are unaffected;
  only direct flat-route POSTs need signing.

Errors: `missing 'sig'` / `'sig' must be 128-hex` / `missing 'req_nonce'` /
`stale/replayed req_nonce N` / `bad wallet signature`.
