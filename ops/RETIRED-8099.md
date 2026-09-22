# The :8099 retirement — sigil-rpcd and everything that fronted it

Closed 2026-09-22 on the operator's instruction (*"sigil rpdc is dead code and
retired we use sigil api now"* → *"fjern restart=always og slet sigil-rpc
crate'en"* → *"ja fjern halen også"*).

The one live money/data backend is **`sigil-api` on `127.0.0.1:18181`**, embedded
in the `sigil-node` binary and backed by the real `SigilState` /
`commit_state_transition` chokepoint. Nothing should point at `:8099` again.

## Why it kept coming back — nobody was restarting it

`sigil-rpcd` was stopped + disabled 2026-08-17, found running 08-31, stopped
again, and found running again 09-22. It was never a person. `Restart=always`
turned every OOM-kill into a relaunch:

```
restart counter   1,288
OOM kills / day   3
CPU per life      18 min 31 s
RSS after 40 s    1.6 GB and climbing
```

A retired service had been burning RAM and cores on the production box for weeks
while answering a port nothing should call. Box load fell **157 → 99** when it
stopped. **Generalise: a "permanently retired" service that reappears is a
supervisor, not a person. Read the restart counter before blaming anyone.**

## What was done

| thing | action |
|---|---|
| `sigil-rpcd.service` | `Restart=always` → `Restart=no`, stopped, disabled |
| `sigil-tls.service` | socat `:8843` → dead `:8099`; `disable --now` |
| `sigil-ledger-tip.service` | `disable --now` — see the trap below |
| `crates/sigil-rpc` | deleted, 5,999 lines (sigil `43a1edc2`) |
| q-flux `fluxapp.xyz` vhost | `backend = "127.0.0.1:8099"` → `"127.0.0.1:18181"` |

`sigilgraph.quillon.xyz` had already been repointed on 2026-08-23. Of the four
`8099` hits in `q-flux.toml`, three were comments — only one live backend
remained.

## 🪤 The trap: `sigil-ledger-tip` was NOT dead weight

It looked like pure tail. It is not — it writes
`sigilgraph.org/sigil-tip.json`, which `sigil-explorer.html` fetches. Deleting it
blind would have removed a live feed.

What saved it was decoding the payload instead of trusting the filename and the
mtime:

```
network_id bytes -> sigil-g0      # the chain that died 2026-08-29
height           -> 326,553       # g2 was at 23,830,602
```

It had been publishing a tip from a **dead chain for three and a half weeks**,
looking perfectly healthy. The only reason it did no damage is that the explorer
guards on `g.height === 0` (it wants the genesis block), so the stale file was
already being rejected.

**Decode the payload before calling a feed live.** A running unit, a fresh mtime
and an HTTP 200 prove nothing about what is inside.

Its four stale JSON files were deliberately LEFT in place: both explorers' guards
reject them, and deleting from a served root is what the chain-firewall rule
forbids.

## Rollback

Every unit file and the q-flux config are backed up in
`/home/storage/sigil-scratch/`:

```
sigil-rpcd.service.bak-20260922-173905
sigil-tls.service.bak-<ts>
sigil-ledger-tip.service.bak-<ts>
q-flux.toml.bak-20260922-181739
sigil-rpc-retired-20260922.tar.gz     # 676 files, 18 .rs
```

The crate itself is in git history; the tarball is belt-and-braces.
🪤 `systemctl mask` does NOT work on these units — mask only covers units in
`/lib`, and these are real files in `/etc/systemd/system`.

## Verification, measured

```
                                   before      after
fluxapp.xyz/v1/health                 502        200  {"ok":true,"data":"sigil-api"}
quillon.xyz/                          200        200
quillon.xyz/api/v1/status             200        200
sigilgraph.org/v1/health              200        200
sigilgraph.quillon.xyz/v1/health      200        200
```

`q-flux` has **no `ExecReload`**, so applying the vhost change needs a full
restart and the blast radius is all of quillon.xyz — hence the five-URL baseline
before and after. `q-api-server` was `active` throughout.

End state: 0 listeners on `:8099`, 0 live q-flux backends on it, three units
inactive + disabled, crate deleted.
