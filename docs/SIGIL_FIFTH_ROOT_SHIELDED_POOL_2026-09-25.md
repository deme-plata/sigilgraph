# The fifth root — `shielded_pool_root` in the block header (height-gated, follower-first)

*2026-09-25 · operator instruction: "add the shielded pool root to the header, height gated, follower first"*

## Why
On 2026-09-25 a docker follower on Epsilon (`sigil-node-4`) re-derived the whole chain from an archive copy and
agreed with the producer on all four header roots at equal height — and the audit that produced that result found
that the SHIELDED POOL, where every coin has lived since transparent sends were retired
(`sigil_tx::SHIELDED_ONLY_HEIGHT == 0`), had no root in `SigilBlockHeaderV0` at all. Two nodes could hold different
notes and nullifiers and sign identical headers. `ShieldedPool::digest()` had been written "for folding into
`wallet_state_root`" and never got a caller (the committed-but-never-called class, again). It also hashes every
ciphertext and every sealed epoch on each call — tens of MB per block on today's pool — so it could not be the thing
that ships on a producer minting at 8–110 blk/s.

## What
- `sigil_state::ShieldedPool::pool_root()` — BLAKE3 over `"sigil-shielded-pool-root-v1" ‖ epoch ‖ notes_root ‖
  note_count ‖ nullifier_count ‖ acc_to_root(nullifier_acc) ‖ value_locked ‖ archive_len ‖ every sealed epoch root`.
  O(1) per block on an untouched pool: the sparse notes root is memoized by `(epoch, notes.len())` (notes are
  append-only within an epoch) and the nullifier set is an additive multiset accumulator (the `wallet_state_root`
  construction, `SIGIL/nullifier-acc/v1` leaves) maintained at the single insert site and folded from the set on first
  use after a boot. Both memos are `#[serde(skip)]` and invisible to `PartialEq`, so a snapshot-booted node computes
  exactly what a genesis-replayed node computes (pinned by `pool_root_is_identical_warm_cold_after_roundtrip_and_order_independent`).
  v1 excludes ciphertexts and the address/key registries — the postbag and the address book, not the money.
- `SigilBlockHeaderV0.shielded_pool_root: Option<[u8; 32]>`, `#[serde(default)]`, appended after `topology_commitment`.
  Hash/signature stability for the ~26 M historical blocks is the `topology_commitment` recipe: the exact
  `,"shielded_pool_root":null` fragment is stripped from the JSON that `hash()` / `signing_bytes()` digest while the
  field is `None`; bincode stays uniform (no `skip_serializing_if` — the 2026-08-15 incident). A pre-field bincode
  record decodes to `None` (test: the new encoding minus its trailing `Option` byte); a named record without the key
  decodes to `None`.
- Gate: `sigil_header::SHIELDED_POOL_ROOT_ACTIVATION_HEIGHT = 27_000_000`. `precheck()` refuses a missing root at/after
  the gate AND a premature one below it (`HeaderError::ShieldedPoolRoot{Missing,Premature}`), so old and new binaries
  agree on every pre-gate block. `Block::check_shielded_pool_root` + `ChainTip::apply` refuse a gated block whose root
  is not the follower's own (`STATE DIVERGENCE … shielded pool root`). `mint_next_block` fills it from the same
  post-transition state as the four roots (`build_block_body_*` now returns it as a fifth element).
- `/v1/integrity` → `roots.shielded_pool_root`, `shielded_pool_root_in_header`, `shielded_pool_root_activation_height`.
- **The wire, and why it needed its own layer.** Every header page on the backfill wire (`serve_read` codecs 0
  `'H'` / 1 `'Z'`), the client decoders (`sigil-top` `block_sync/verify.rs`, `sigil-serve`) and the client block store
  encode headers with POSITIONAL bincode — and positional bincode cannot default a missing trailing field
  (`UnexpectedEof`, pinned by `bincode_is_not_cross_version_but_named_formats_are`). Had the node serialized the fifth
  root under the old codecs, every deployed `sigil-top` (v10.1.2 requests `headers_only, codec: 1`) would have
  misaligned on every page the moment the producer restarted — before any gate. So:
  `sigil_header::SigilBlockHeaderV0Legacy` (the pre-field layout, byte-identical — pinned by
  `legacy_page_is_byte_identical_to_the_pre_field_encoding`) stays on codecs 0/1; the full layout rides
  **codec 5, tag `'V'`** (`HEADER_PAGE_CODEC_V1`), which the new `sigil-top` requests and both decoders accept;
  an old node answers a codec-5 request with `'H'` (legacy) and is still decoded. The client block store
  (`sigil-block-store::StoredBlock`) reads both record layouts (untagged = legacy, `0x01`-tagged = fifth-root) and
  writes the tagged one, so an existing `sigil-top-blocks.db` keeps working after the update (a downgraded client
  would not read the new records). The chain log, gossip and full-block backfill are MessagePack-named and were
  never affected. Still positional and deliberately untouched: the pre-`SIGILM1` bincode full-block fallback
  (`serve_read.rs` legacy decode) and the `bulk_import_prefix` content-addressed header blobs.
- Simulation hook: `sigil_header::override_shielded_pool_root_activation_for_simulation(h)` (process-global, never
  called by `sigil-node start`) so chronos and the integration gate can cross the gate on a young chain.

## Tests that pin it
- `sigil-header`: hash + signing bytes unaffected by `None`; committed when `Some`; bincode pre-field decode, mixed
  Vec round trip, named decode without the key; the gate rule pure + through `precheck`.
- `sigil-state`: root moves with a note, a nullifier, locked value, an epoch rotation, and the same nullifier in two
  epochs; warm ≡ cold ≡ rmp/JSON round trip ≡ reversed spend order.
- `sigil-chronos::scenarios::tamper_shielded_pool_root_at_gate`: honest gated block applied, tampered root =
  Divergence, missing root at the gate = Rejected, premature root below the real gate = Rejected.
- `sigil-node/tests/chronos_shielded_pool_root_agreement.rs`: the REAL loop with REAL shielded payments across a
  lowered gate — present ⇔ required on every header, an independent `ChainTip` re-derives every gated root, the root
  takes ≥ 3 distinct values (not vacuous), a flipped header root is refused naming the shielded pool, a body missing
  its spend does not re-derive the header's root.

## Rollout (a producer cannot be rolled back out of its own blocks)
1. Tests green (this document's list) → build `--release`.
2. **Follower first:** `sigil-node-4` (docker, Epsilon) on the new binary while the producer is still old. Every block
   is pre-gate, the follower just verifies `None`. Then the producer restart (order_hash proof before/after).
3. Every `sigil-top` client MUST run a build with this header before the tip reaches 27,000,000, or it computes a
   different hash for every gated block and parks on "missing parents". Release before the gate; the gate is ~1.4 M
   blocks out (two days at the 8 blk/s floor, ~3.5 h at the 110 blk/s ceiling).
4. At the gate the producer emits `Some(root)`, node 4 logs `🔏 shielded pool root ACTIVE from height 27000000 …`
   and refuses any disagreement. Watch `/v1/integrity` on both.
5. Rollback plan = a HEIGHT, never "reinstall the old binary": if the gate must move, move it BEFORE the tip reaches
   it. Backup of the pre-change producer binary: `<live>.bak-pre-fifth-root-<ts>`.
