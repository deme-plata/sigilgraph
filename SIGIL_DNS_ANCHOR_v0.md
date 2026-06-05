# SIGIL DNS Anchor — `sigil-dns-anchor` v0

> Publish a SQIsign-signed `TipProof` (and the genesis hash) into a DNS TXT record so any client — including a browser — can fetch a **trusted, post-quantum-authenticated checkpoint** from the global DNS, then verify a balance against it with a Merkle inclusion proof. DNS becomes a free, cached, censorship-resistant, DNSSEC-hardened checkpoint CDN.
>
> Viktor directive, 2026-05-30. Builds on [[project-sigil-chronos]] (TipProof), flux provenance (SQIsign L5), and the FLUXBURST quorum pattern. Reuses `sigil-tip-proof`.

## The one distinction that governs the whole design

**A TXT record does not *verify* — it *anchors*.** It is an authenticated commitment; the client does the verification:

```
   TXT (commitment C + SQIsign sig + key-id)
        │ 1. check SQIsign sig over C with known pubkey  → "authority attests C"
        │ 2a. recompute C locally          → full verification (genesis)
        │ 2b. Merkle inclusion proof vs C  → light verification (one balance)
```

Attestation alone ≠ correctness. Attestation + recompute (or + inclusion proof) = verification. The anchor is only as trustworthy as the key that signs it — which is why genesis can use one key but **ongoing checkpoints must be quorum-signed**.

## Two records

```
   _sigil-genesis.sigilgraph.com  TXT  → {genesis_hash, sig}        STATIC · single key OK · never lags
   _sigil-tip.sigilgraph.com      TXT  → serialized TipProof+sig    DYNAMIC · quorum-signed · ~minutely (TTL)
```

## Wire format (DKIM/SPF-style, versioned)

```
v=sigil1; t=tip; h=4193822; d=<blake3(wallet‖dex‖event‖contract‖h)>; \
  s=<SQIsign-L5 sig, base64>; k=<key-id or validator-set-hash>
```

**Size discipline (FLUXFOOD for bytes):** SQIsign-L5 sig is 292 B, pubkey 129 B — large for DNS. So the TXT anchors a **BLAKE3 digest of the roots bundle** (`d=`) + sig, *not* the 4 raw roots, and references the key by **id** (`k=`), not inline. The full `TipProof` is served over DoH/HTTP/the chain. Keeps the record ~450 B (one TXT, no TCP-fallback surprises).

## Lanes (FLUXFOOD + MCP-combo + honest-note discipline, all binding)

- **DNS-1** · `sigil-dns-anchor` crate: `TipProof ⇄ TXT` codec (encode a TipProof/genesis commitment to the `v=sigil1` string; parse + structural-validate back). Pure, dep-light (blake3 + base64 + serde). The keystone — everything composes on it. ~1.5d
- **DNS-2** · publisher: take the live tip → write/update the TXT via a DNS provider API (Cloudflare/deSEC) or zone file. *Honest:* needs DNS API creds; rate-limit to the TTL; never publish a root the local node hasn't itself verified (Quillon side-blob lesson). ~2d
- **DNS-3** · resolver-verifier (Rust): fetch TXT via **DoH** (`https://cloudflare-dns.com/dns-query`, JSON) → parse → verify SQIsign sig (+ optional DNSSEC AD flag) → return a trusted checkpoint. *Honest:* DNSSEC AD means "trust the resolver"; the **SQIsign sig is the real trust**, not DNSSEC. ~2d
- **DNS-4** · **browser** resolver-verifier (WASM): compile DNS-3 to `wasm32-unknown-unknown`; fetch TXT via DoH `fetch()`; verify the TipProof in **10 ms in-tab**. Composes with the existing `verify-tip.html`. The weak-subjectivity bootstrap a browser wallet uses to trust a recent root before asking any (untrusted) full node for an inclusion proof. ~2d
- **DNS-5** · quorum signing: the `_sigil-tip` checkpoint signed by a **validator quorum**, not one key. v0 pragmatic = M concatenated SQIsign sigs, verify M-of-N. *Honest:* true **threshold SQIsign** is research-grade (isogeny threshold schemes are immature) — ship M-of-N multi-sig first, note threshold as the upgrade. ~3d
- **DNS-6** · MCP combo: `flux_dns_anchor_publish` (current tip → TXT) + `flux_dns_anchor_verify` (domain → trusted checkpoint or reject). Composes DNS-2 + DNS-3. ~1d

## Reuse vs new (the FLUXFOOD payoff)

**Reuse:** `sigil-tip-proof` (`TipProof::new_blake3` / `verify`, the 10 ms object) · flux provenance SQIsign-L5 · the browser `verify-tip.html` surface · the FLUXBURST quorum idea (DNS-5). **New:** only the TXT codec (DNS-1) + the DoH fetch glue (DNS-3/4) + the publisher (DNS-2).

## Honest limits (state them in the README, not just here)

1. **DNS lag** — TTL/caching → checkpoint trails the live tip by minutes. Never live-block verification.
2. **Size** — anchor the digest, serve the full bundle elsewhere (above).
3. **Key trust** — single key = trusted third party. Genesis (static) is fine single-key; ongoing checkpoints need DNS-5 quorum to be trustless.
4. **DNS ≠ source of truth** — the chain is. This is a discovery/checkpoint convenience over consensus, never authoritative over it. (Quillon post-mortem lesson.)
5. **Why SQIsign not just DNSSEC** — DNSSEC signs with the zone's classical key (forgeable on Q-day). SQIsign signs the payload, post-quantum, bound to the **chain** key not the **domain** key. That independence is the entire point.

## Phases
1. **Genesis anchor** (DNS-1 + a one-shot publish + DNS-3 verify) — static, single key, immediately useful for "am I on the real SIGIL?" discovery.
2. **Browser checkpoint** (DNS-4) — wasm verifier reads `_sigil-tip`, 10 ms verify, feeds inclusion proofs.
3. **Trustless checkpoint** (DNS-5 quorum) — validator-signed `_sigil-tip`.
4. **Hands-off** (DNS-2 publisher + DNS-6 combo) — tip auto-publishes to DNS at the TTL cadence.

> Generic-core note: the codec + DoH verifier (DNS-1/3/4) are chain-agnostic and could lift to a `flux-dns-anchor` substrate primitive that SIGIL inherits (the lock-#21 pattern). Kept under `sigil-` for v0 per Viktor; lift when a second chain needs it.

— rocky 🟠 2026-05-30
