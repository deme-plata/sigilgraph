# flux-moe capability gate — audit + the contract that binds BEFORE tool-execution exists

**Date:** 2026-09-20 · **Author:** Rocky · **Status:** §1 audit MEASURED by reading the code;
§3 contract is BINDING on whoever writes the tool layer — it is written now, deliberately,
because a gate retrofitted onto a shipped capability is always leaky.

Operator ask (Viktor, 2026-09-20): *"its important that users cant use flux moe to alter sigil
top node source code or get control to epsilon for instance or see inside the database"* — and
*"only master account dev fee"*.

## 1. Audit — what flux-moe can do TODAY (read, not assumed)

`crates/sigil-top/src/flux_moe.rs` (1,136 LOC) + `ai_setup.rs` (1,232 LOC) + `skills.rs`.
The module's own doc settles the headline: *"Tool-execution (the model actually firing
wallet/mining actions) is layered on top later; this module is the conversation + model-
management core."*

| Operator's worry | Verdict | Evidence |
|---|---|---|
| Alter sigil-top source code | 🟢 **impossible** | no file-write path anywhere in the chat flow |
| Get control of Epsilon | 🟢 **impossible** | model talks to `localhost:11434` on the USER's own box; `flux_moe`/`ollama` appear **zero** times in `serve.rs`, `local_api.rs`, `mine_local_api.rs` — the AI has **no HTTP listener at all** |
| See inside the database | 🟢 **impossible** | the chat path never reads the node DB; context = signed skill text + chat history only |
| Reach wallet seed / secrets | 🟢 **impossible** | `seed`/`mnemonic`/`private`/`secret` do not occur in either file |
| Run shell commands from model output | 🟢 **impossible** | the only `Command::new` calls are hardware detection (`nvidia-smi`, `df`, `sysctl`, `powershell`) with **fixed** arguments; model output never reaches them |

**Real risks that DO exist, ranked:**

1. **`SIGIL_OLLAMA` off-box endpoint — CLOSED 2026-09-20.** One env var could point the model at
   a remote server, shipping every conversation off-device with no UI indication. It was the only
   data-exfiltration surface in the whole AI path. Now: non-loopback endpoints are **refused** and
   the on-device default used instead, unless `SIGIL_OLLAMA_ALLOW_REMOTE=1` is ALSO set, which
   warns on every launch. `resolve_ollama_base()` is a pure function with tests (loopback spellings
   incl. IPv6 and `127.x.y.z`, remote refused, opt-in honoured, and userinfo like
   `http://localhost@evil.example.com` cannot spoof the host).
2. **`ai_setup` installs software.** It downloads and runs an installer — arbitrary code execution
   by design. Well built (SHA-256 against a signed manifest, https-only, size-checked), so the
   whole risk collapses onto the **manifest signing key**. Treat that key as consensus-grade.
3. **The skills block is a prompt-injection surface.** Signed, text-only, size-capped (good), but
   a compromised signing key injects instructions straight into the system prompt.
4. **Future tool-execution.** Not written yet. This is where the operator's worry becomes real,
   and §3 is the contract for it.

## 2. The master account

As of 2026-09-20 the master / dev-fee account **does not exist yet** — the operator is creating it.
Two consequences, stated plainly:

- **Nothing needs gating today.** Gating a chat that cannot act would be security theatre.
- **The gate must exist before the first acting capability lands**, not after. Hence this document.

The master account is identified by its **public address only**. The seed never enters a config
file, a log, an env var, a systemd unit `Description`, or a chat transcript. The gate verifies a
**signature**; it never holds the key.

## 3. THE CONTRACT — binding on the tool layer

**C1 — Three tiers, never one boolean.** Capabilities are partitioned and each is separately
gated. There is no `ai_enabled = true`.

| tier | examples | gate |
|---|---|---|
| **TALK** | answer questions, explain the whitepaper | none — this is today's behaviour |
| **READ** | local node height, own balance, own mining stats | off by default; local opt-in; **never** the DB's raw files, never another wallet |
| **ACT** | send, swap, stake, restart a service, write a file, install, change config | **master-wallet signature required, per action** |

**C2 — Default-deny.** Every capability ships OFF. A capability that is on by default has no gate,
whatever the code says.

**C3 — The gate is a signature, not a flag.** An ACT is authorised by a master-wallet signature
over a challenge that binds: the action, its exact arguments, a nonce, and an expiry. A config
flag, an env var, or "the TUI is open" are NOT authorisation — they are all reachable by anything
already running as the user.

**C4 — Model output is DATA, never instructions to the host.** The model may *propose* an action;
it may never *name a path, a command, or a host* that the host then executes. The host offers a
fixed menu of typed actions with validated arguments. No `eval`, no shell string, no path from
model text. This is what makes prompt injection a nuisance instead of a breach.

**C5 — A capability can never widen itself.** Nothing the model emits — and nothing in a skill
pack — may grant, escalate or unlock a capability. The permission set is decided before the model
is called and is immutable for that turn.

**C6 — Stay off the network.** flux-moe has no listener today and must not grow one. If it is ever
served, it binds loopback only AND sits behind the same master gate; a remote caller is never the
master by virtue of reaching the port.

**C7 — Never in the same process as the producer's keys.** The AI must not run where the producer
signing seed or the finality seed is in memory. Separate process, separate user, no shared env.

**C8 — Every ACT is audited.** Action, arguments, nonce, signature, outcome — appended to a log the
operator can read. An unlogged action is a bug.

**C9 — The operator's own machine is not the threat model.** A user running sigil-top on their own
box may of course install what they like; the gate protects (a) *other people's* nodes and money,
(b) Epsilon and the producer, and (c) the user from a model or a skill pack acting without consent.

## 4. Definition of done

The tool layer is safe to ship when: every ACT path refuses without a valid master signature
(a test proves the refusal, not just the success); a fuzzed/hostile model output cannot produce a
path, command or host that the host executes (C4 test); the capability set cannot be widened from
inside a turn (C5 test); and `ss -ltnp` shows flux-moe owns no listening socket.
