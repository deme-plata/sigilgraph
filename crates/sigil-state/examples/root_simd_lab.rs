//! root_simd_lab — how much of the state-root commitment ceiling is actually
//! reachable, and which lever moves it?
//!
//! ── WHY THIS EXISTS ─────────────────────────────────────────────────────────
//! The headline "state commits run at 209M/s" (SIGIL-TPS-LADDER.md §1, quoting
//! `zkflux.rs`) is NOT measured on the production commitment primitive. Reading
//! `examples/batch_auth.rs` and `examples/stargate_500m.rs`, the thing being
//! timed at 209M/s is:
//!
//!     fn mix(x: u64) -> u64 { xor-shift + one 64-bit multiply }
//!     fn fold_op(acc: &mut u64, op) { *acc ^= mix(..) × 4 }
//!
//! — a 64-bit non-cryptographic XOR-mix. The ladder itself hedges this as
//! "13M sound / 209M unsafe". The REAL primitive, `sigil_state::Accumulator`
//! (`acc.rs:88`), is per leaf touch:
//!
//!     leaf = BLAKE3(LEAF_DOMAIN ‖ len(key) ‖ key ‖ len(value) ‖ value)
//!     sum  = wrapping_add_256(sum, leaf);  count += 1
//!
//! That is a cryptographic hash plus a 256-bit add — categorically more work
//! than four XOR-mixes. So "209M" cannot be spent as if it were the sound
//! number, and any TPS plan resting on it is resting on the wrong constant.
//! This harness measures the real one and quantifies each lever separately.
//!
//! ── WHAT EACH EXPERIMENT ISOLATES ───────────────────────────────────────────
//!   E0  Accumulator::insert — the production primitive, as shipped.
//!   E1  leaf_hash alone     — how much of E0 is hashing vs bookkeeping.
//!   E2  one-shot blake3::hash() on a pre-packed fixed 64-byte preimage —
//!       removes the per-call Hasher init/finalize and the 5 update() calls.
//!   E3  wrapping_add_256 alone — is the 256-bit add even visible?
//!   E4  BLAKE3 over one large buffer — the crate's own SIMD path IS engaged
//!       here, so this yields the per-64-byte-block cost when the machine's
//!       vector units are actually used. E2/E4 is the headroom being left on
//!       the table by hashing leaves one at a time.
//!   E5  Platform::hash_many::<64> — 16-way AVX-512 batched compression, the
//!       direct realization of that headroom. CORRECTNESS-CHECKED against
//!       blake3::hash() before it is allowed to report a rate; if the check
//!       fails the number is suppressed rather than reported.
//!   E6  best single-core primitive scaled across all cores.
//!
//! ── THE STRUCTURAL FINDING E5 TESTS ─────────────────────────────────────────
//! `hash_many` requires FIXED-LENGTH inputs (`&[&[u8; N]]`). The production
//! leaf preimage is variable-length and length-prefixed, so the batched path
//! is unreachable *for the current encoding* — the leaf format itself is what
//! blocks 16-way SIMD. A fixed-width leaf encoding would unlock it, but
//! `leaf_hash` defines the state root, so changing it changes every root: a
//! consensus change that belongs at a fork/genesis boundary, not a patch.
//! E5 measures what that change would be worth before anyone pays for it.
//!
//! Run: cargo run --release --example root_simd_lab   (via fluxc)

use std::time::Instant;

// ── BLAKE3 constants (public, from the spec — the crate keeps its copies private)
const IV: [u32; 8] = [
    0x6A09E667, 0xBB67AE85, 0x3C6EF372, 0xA54FF53A, 0x510E527F, 0x9B05688C, 0x1F83D9AB, 0x5BE0CD19,
];
const CHUNK_START: u8 = 1;
const CHUNK_END: u8 = 2;
const ROOT: u8 = 8;

/// Mirrors `sigil_state::acc`'s private helper so E3 can time it in isolation.
#[inline]
fn wrapping_add_256(a: [u8; 32], b: [u8; 32]) -> [u8; 32] {
    let mut out = [0u8; 32];
    let mut carry = 0u16;
    for i in 0..32 {
        let s = a[i] as u16 + b[i] as u16 + carry;
        out[i] = s as u8;
        carry = s >> 8;
    }
    out
}

fn rate(label: &str, n: u64, secs: f64) {
    let r = n as f64 / secs;
    let unit = if r >= 1e6 { format!("{:>9.2} M/s", r / 1e6) } else { format!("{:>9.0}   /s", r) };
    println!("  {label:<52} {unit}   ({:.1} ns/op)", secs * 1e9 / n as f64);
}

fn main() {
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    println!("\n  ROOT/SIMD LAB — the real state-commit primitive vs the 209M headline");
    println!("  cores={cores}  platform={:?}\n", blake3::platform::Platform::detect());

    // Realistic production leaf: key = WalletId(32) ‖ TokenId(32), value = u128 balance.
    let key: Vec<u8> = (0u8..64).collect();
    let val: Vec<u8> = (0u8..16).collect();

    const N: u64 = 3_000_000;

    // ── E0 — the production primitive, exactly as shipped ───────────────────
    {
        let mut acc = sigil_state::Accumulator::new();
        let t = Instant::now();
        for i in 0..N {
            let mut k = key.clone();
            k[0] = i as u8; // defeat any constant-folding; realistic distinct keys
            acc.insert(&k, &val);
        }
        let s = t.elapsed().as_secs_f64();
        rate("E0  Accumulator::insert (PRODUCTION, incl. key clone)", N, s);
        std::hint::black_box(acc);
    }
    // E0b without the per-op allocation, to separate primitive cost from the harness's own
    {
        let mut acc = sigil_state::Accumulator::new();
        let mut k = key.clone();
        let t = Instant::now();
        for i in 0..N {
            k[0] = i as u8;
            acc.insert(&k, &val);
        }
        let s = t.elapsed().as_secs_f64();
        rate("E0b Accumulator::insert (no per-op alloc)", N, s);
        std::hint::black_box(acc);
    }

    // ── E1 — leaf_hash alone ────────────────────────────────────────────────
    {
        let mut k = key.clone();
        let mut sink = [0u8; 32];
        let t = Instant::now();
        for i in 0..N {
            k[0] = i as u8;
            let h = sigil_state::Accumulator::leaf_hash(&k, &val);
            sink[0] ^= h[0];
        }
        let s = t.elapsed().as_secs_f64();
        rate("E1  leaf_hash alone (Hasher + 5 updates + finalize)", N, s);
        std::hint::black_box(sink);
    }

    // ── E2 — one-shot hash of a pre-packed fixed 64-byte preimage ───────────
    {
        let mut buf = [0u8; 64];
        buf[..48].copy_from_slice(&key[..48]);
        buf[48..].copy_from_slice(&val);
        let mut sink = [0u8; 32];
        let t = Instant::now();
        for i in 0..N {
            buf[0] = i as u8;
            let h = blake3::hash(&buf);
            sink[0] ^= h.as_bytes()[0];
        }
        let s = t.elapsed().as_secs_f64();
        rate("E2  blake3::hash() one-shot, fixed 64B preimage", N, s);
        std::hint::black_box(sink);
    }

    // ── E3 — the 256-bit add on its own ─────────────────────────────────────
    {
        let mut a = [0u8; 32];
        let b = [7u8; 32];
        let t = Instant::now();
        for _ in 0..N {
            a = wrapping_add_256(a, b);
        }
        let s = t.elapsed().as_secs_f64();
        rate("E3  wrapping_add_256 alone", N, s);
        std::hint::black_box(a);
    }

    // ── E4 — BLAKE3 bulk (SIMD engaged) → per-64B-block cost ────────────────
    {
        let big = vec![0xA5u8; 64 << 20]; // 64 MiB
        let t = Instant::now();
        let h = blake3::hash(&big);
        let s = t.elapsed().as_secs_f64();
        let blocks = (big.len() / 64) as u64;
        println!(
            "  {:<52} {:>9.2} GiB/s",
            "E4  blake3 bulk 64 MiB (SIMD engaged)",
            big.len() as f64 / s / (1024.0 * 1024.0 * 1024.0)
        );
        rate("E4b   → equivalent 64-byte blocks", blocks, s);
        std::hint::black_box(h);
    }

    // ── E5 — 16-way AVX-512 batched compression, correctness-gated ──────────
    {
        let plat = blake3::platform::Platform::detect();
        let deg = plat.simd_degree();
        println!("\n  E5  batched hash_many — simd_degree = {deg}");

        let mut inputs_owned: Vec<[u8; 64]> = Vec::with_capacity(deg);
        for i in 0..deg {
            let mut b = [0u8; 64];
            b[0] = i as u8;
            b[1] = 0xEE;
            inputs_owned.push(b);
        }
        let refs: Vec<&[u8; 64]> = inputs_owned.iter().collect();
        let mut out = vec![0u8; deg * 32];
        plat.hash_many(
            &refs,
            &IV,
            0,
            blake3::IncrementCounter::No,
            ROOT,
            CHUNK_START,
            CHUNK_END,
            &mut out,
        );

        // CORRECTNESS GATE — must equal the stable one-shot hash, or no number.
        let mut ok = true;
        for (i, inp) in inputs_owned.iter().enumerate() {
            let want = blake3::hash(inp);
            if &out[i * 32..(i + 1) * 32] != want.as_bytes() {
                ok = false;
                if i == 0 {
                    println!("      ✗ MISMATCH on input 0 — batched flags/IV wiring is wrong.");
                    println!("        got  {}", hex32(&out[0..32]));
                    println!("        want {}", hex32(want.as_bytes()));
                }
                break;
            }
        }
        if !ok {
            println!("      ⇒ rate SUPPRESSED: an unverified hash is not a result.");
        } else {
            println!("      ✓ all {deg} lanes byte-match blake3::hash()");
            let batches = N / deg as u64;
            let t = Instant::now();
            for i in 0..batches {
                inputs_owned[0][2] = i as u8;
                let refs: Vec<&[u8; 64]> = inputs_owned.iter().collect();
                plat.hash_many(
                    &refs,
                    &IV,
                    0,
                    blake3::IncrementCounter::No,
                    ROOT,
                    CHUNK_START,
                    CHUNK_END,
                    &mut out,
                );
            }
            let s = t.elapsed().as_secs_f64();
            rate("E5  hash_many batched (per leaf)", batches * deg as u64, s);
            std::hint::black_box(&out);
        }
    }

    // ── E6 — production primitive across all cores ──────────────────────────
    {
        println!();
        let per = N / cores as u64;
        let t = Instant::now();
        let hs: Vec<_> = (0..cores)
            .map(|c| {
                let key = key.clone();
                let val = val.clone();
                std::thread::spawn(move || {
                    let mut acc = sigil_state::Accumulator::new();
                    let mut k = key.clone();
                    for i in 0..per {
                        k[0] = (i as u8).wrapping_add(c as u8);
                        acc.insert(&k, &val);
                    }
                    acc.root()
                })
            })
            .collect();
        let roots: Vec<_> = hs.into_iter().map(|h| h.join().unwrap()).collect();
        let s = t.elapsed().as_secs_f64();
        rate(
            &format!("E6  Accumulator::insert × {cores} threads (PRODUCTION)"),
            per * cores as u64,
            s,
        );
        std::hint::black_box(roots);
    }

    println!("\n  Reminder: E0/E6 are the SOUND numbers. The 209M headline is a u64 XOR-mix,");
    println!("  not this primitive — do not budget TPS against it.\n");
}

fn hex32(b: &[u8]) -> String {
    b.iter().take(8).map(|x| format!("{x:02x}")).collect()
}
