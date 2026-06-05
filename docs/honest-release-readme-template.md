# SIGIL vX.Y.Z — <Codename>

**Status:** 🟡 **Release Candidate** — soak gate (R9) pending. Promotion to "tested + soaked" happens after 24h of clean Delta + Epsilon production at this version.

**Codename:** <one-line tagline — what makes THIS release distinctive>

**Date:** <ISO date of RC cut> · **Promotion target:** <RC + 3 to 5 days>

---

## The story this bundle tells in 30 seconds

> <One paragraph. Concrete. Sensory if possible — what does a new user SEE when this release works? Lead with the user experience, not the internals.>

This is the <Nth> SIGIL release. The previous release shipped <one-line summary>. This release adds <one-line summary of headline feature>.

---

## What this release IS

- **<binary 1>** — what it does, who runs it
- **<binary 2>** — what it does
- **`.proof` bundles** alongside each binary — SQIsign-L5 signature over artifact-BLAKE3 + source-BLAKE3 + agent wallet + swarm task_id + fluxc-version. Verifiable offline.
- **Signed `ReleaseAnnouncement`** for auto-update propagation via `/sigil/g0/release` gossipsub topic.
- **Demo scripts** carried forward from prior releases (re-targeted to this version) + any new demo specific to this release.

## What this release IS NOT

(Read this before deciding whether to use this version in production.)

- **Not yet soak-tested at scale.** R9 (24h continuous Delta + Epsilon production) has not been gated. RC status until it does.
- <list other non-goals — every release has at least 3 things it explicitly doesn't do>

## Bundle contents

| File | What | Provenance |
|---|---|---|
| `sigil-node-vX.Y.Z` | release binary, Linux x86_64 musl | `.proof` ✓ |
| `sigil-node-vX.Y.Z.proof` | SQIsign-L5 provenance bundle | self-attesting |
| `sigil-node-vX.Y.Z.announcement.json` | signed `ReleaseAnnouncement` | SQIsign-L5 ✓ |
| <repeat per binary> | | |
| `<demo>.sh` | demo runner | — |
| `README.md` | this file | — |

**BLAKE3 hashes:**

```
sigil-node-vX.Y.Z             BLAKE3: <R3 fills>
<repeat per binary>
```

## Running the demos (operator quick path)

Prereqs:
- Delta + Epsilon set up via `scripts/host-setup.sh`
- vX.Y.Z deployed to both via `scripts/deploy.sh delta X.Y.Z` + `scripts/deploy.sh epsilon X.Y.Z` (R6)

### Demo <N> — <name>

```bash
releases/vX.Y.Z/<demo>.sh
# Expected: <one-line headline outcome>
```

## How to verify the binary

```bash
# 1. BLAKE3
b3sum sigil-node-vX.Y.Z
jq .artifact_blake3 sigil-node-vX.Y.Z.proof
# (must match)

# 2. SQIsign signature
fluxc verify-provenance sigil-node-vX.Y.Z.proof --pubkey <agent_pubkey>

# 3. Source tree
git -C sigil log --oneline | head -1
# vs .proof.fluxc_git
```

All three checks must pass.

## Soak gate status (R9)

```
Soak start:    [pending]
Soak end:      [pending]
Status:        ⏳ not yet started
```

After R9 passes, the above section is updated:

```
Soak start:    <ISO 8601>
Soak end:      <+24h>
Status:        ✅ Tested + soaked, 24h survival verified
```

That status line is the only thing that promotes RC to recommended-stable.

## Next releases in the pipeline

| Version | Codename | Target | Scope |
|---|---|---|---|
| **vX.Y.Z** | <Codename> | now | this release |
| <next version> | <next codename> | ~Nd | <scope> |

---

## Honest section — what could go wrong

(List specific unverified paths this release exercises for the first time. Every release has at least one.)

- <unverified path 1>
- <unverified path 2>

If any fails: REVERT, root-cause, ship a fix release. RC stays RC.

---

— <agent_id>, <ISO date>
