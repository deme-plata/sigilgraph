//! sigil-pipeline-bench — every number the corridor paper prints, measured at generation
//! time on this box, as one JSON object on stdout. Nothing here is a constant.
//!
//! Usage: sigil-pipeline-bench [reserve_wsigil_wei reserve_usdc_units]
//!   (the optional pair lets the caller pass LIVE Uniswap reserves for the slippage table)

use std::time::Instant;

use sigil_bridge::proof::header_meets_target;
use sigil_bridge::{dsha256, BridgeLedger, SpvProof};
use sigil_pipeline::btc_in::{admit_deposit, decode_header_hex, display_hash, mainnet_pow_limit, synthetic, BtcPolicy, BITCOIN_GENESIS_HEADER_HEX};
use sigil_pipeline::eth_out::{Settlement, DECIMAL_SHIFT, POLYGON_CHAIN_ID, WSIGIL3_POLYGON};
use sigil_pipeline::external::{AssetId, ExternalChain};
use sigil_pipeline::intent::{CrossChainIntent, Privacy};
use sigil_pipeline::private_mid::{build_private_exit_memo, mint_deposit_note, shield_pk_wire, vault_open_exit_memo, verify_private_exit, Rate};
use sigil_pipeline::solver::Pool;
use sigil_shield::note_cipher::{enc_identity_from_seed, ShieldedAddress};
use sigil_shield::note_v1::{padding_leaf, to_wire};
use sigil_shield::wallet::{NoteStore, ShieldedAccount};

fn padded_n(cms: &[[u8; 32]], n: usize) -> Vec<[u8; 32]> {
    let mut v = cms.to_vec();
    for i in v.len()..n {
        v.push(to_wire(padding_leaf(i as u64)));
    }
    v
}

fn median_us(samples: &mut Vec<u128>) -> u128 {
    samples.sort();
    samples[samples.len() / 2]
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let (res_w, res_u): (u128, u128) = match (args.get(1), args.get(2)) {
        (Some(a), Some(b)) => (a.parse().unwrap_or(10u128.pow(18)), b.parse().unwrap_or(1_000)),
        _ => (10u128.pow(18), 1_000),
    };
    let mut out = serde_json::Map::new();

    // ── Bitcoin leg ────────────────────────────────────────────────────────────────────
    let g = decode_header_hex(BITCOIN_GENESIS_HEADER_HEX).unwrap();
    let t = Instant::now();
    let mut n = 0u32;
    while t.elapsed().as_millis() < 200 { assert!(header_meets_target(&g)); n += 1; }
    let header_check_ns = t.elapsed().as_nanos() / n as u128;
    out.insert("genesis_hash".into(), display_hash(dsha256(&g)).into());
    out.insert("header_pow_check_ns".into(), (header_check_ns as u64).into());
    out.insert("mainnet_pow_limit_hex".into(), hex::encode(mainnet_pow_limit()).into());

    let alice = ShieldedAccount::from_seed([11u8; 32]);
    let vault = ShieldedAccount::from_seed([99u8; 32]);
    let vault_enc = enc_identity_from_seed(&[99u8; 32]);
    let vault_addr = ShieldedAddress::new(vault.public_key(), &vault_enc.public_hex());

    let t = Instant::now();
    let proof: SpvProof = synthetic::deposit_proof(100_000, shield_pk_wire(&alice), 6);
    out.insert("synthetic_mine_6_headers_ms".into(), (t.elapsed().as_millis() as u64).into());
    let mut s = Vec::new();
    for _ in 0..50 { let t = Instant::now(); proof.verify(6).unwrap(); s.push(t.elapsed().as_nanos()); }
    out.insert("spv_verify_6conf_us".into(), ((median_us(&mut s) as f64) / 1000.0).into());
    out.insert("spv_proof_bytes".into(), ((proof.tx_bytes.len() + 32 + proof.branch.len() * 32 + 8 + proof.headers.len() * 80) as u64).into());

    // ── SIGIL leg: prove/verify vs pool depth ──────────────────────────────────────────
    let mut rows = Vec::new();
    for log2 in [3u32, 7, 15] { // v5 needs depth+1 a power of two: capacities 2^1, 2^3, 2^7, 2^15
        let cap = 1usize << log2;
        let mut ledger = BridgeLedger::new();
        let p = synthetic::deposit_proof(100_000, shield_pk_wire(&alice), 6);
        let dep = admit_deposit(&mut ledger, &p, &BtcPolicy::synthetic()).unwrap();
        let mut store = NoteStore::new();
        let (idx, cm) = mint_deposit_note(&alice, &mut store, &dep, Rate::UNIT).unwrap();
        let pool = padded_n(&[cm], cap);
        let memo = "SIGILX-exit/v1|chain=polygon|to=0xd7cab8075188df9a50dc494e9bb827f96df93936|asset=USDC|min=1|intent=0000000000000000000000000000000000000000000000000000000000000000|deadline=2000000000";
        let t = Instant::now();
        let exit = build_private_exit_memo(&alice, &mut store, &pool, idx, 60_000, 1, &vault_addr, memo).unwrap();
        let prove_ms = t.elapsed().as_secs_f64() * 1000.0;
        let mut v = Vec::new();
        for _ in 0..5 { let t = Instant::now(); verify_private_exit(&exit).unwrap(); v.push(t.elapsed().as_nanos()); }
        let verify_ms = median_us(&mut v) as f64 / 1e6;
        let t = Instant::now();
        let (val, m) = vault_open_exit_memo(&vault, &vault_enc, &exit).unwrap();
        let open_us = t.elapsed().as_nanos() as f64 / 1000.0;
        assert_eq!(val, 60_000); assert_eq!(m, memo);
        rows.push(serde_json::json!({
            "log2_capacity": log2, "capacity": cap, "prove_ms": prove_ms, "verify_ms": verify_ms,
            "proof_bytes": exit.bundle.proof.len(), "ciphertext_bytes": exit.sealed_to_vault.0.len(),
            "vault_open_us": open_us, "memo_bytes": memo.len(),
        }));
    }
    out.insert("stark_by_depth".into(), rows.into());

    // ── Ethereum leg ───────────────────────────────────────────────────────────────────
    let st = Settlement { sigil_tx_hash: [0x11; 32], eth_dest: [0xd7; 20], glyphs: 100_000_000, chain_id: POLYGON_CHAIN_ID, contract: WSIGIL3_POLYGON };
    out.insert("mint_calldata_bytes".into(), (st.calldata().len() as u64).into());
    out.insert("decimal_shift".into(), DECIMAL_SHIFT.to_string().into());
    out.insert("lock2_0_01_sigil_wei".into(), st.wei().to_string().into());

    // ── Intent / solver: slippage curve on the given pool ──────────────────────────────
    let pool = Pool { chain: ExternalChain::Polygon, a: AssetId::WSigil, b: AssetId::Usdc, reserve_a: res_w, reserve_b: res_u, fee_bps: 30 };
    let spot = pool.spot_1e18(AssetId::WSigil).unwrap_or(0);
    let mut slip = Vec::new();
    for frac_bps in [1u128, 10, 100, 1_000, 5_000, 10_000, 50_000] {
        let amount_in = res_w * frac_bps / 10_000;
        if amount_in == 0 { continue; }
        let out_q = pool.quote(AssetId::WSigil, amount_in).unwrap_or(0);
        let ideal = amount_in * spot / 1_000_000_000_000_000_000;
        let eff_bps = if ideal > 0 { 10_000 - (out_q * 10_000 / ideal) } else { 0 };
        slip.push(serde_json::json!({ "trade_pct_of_pool": frac_bps as f64 / 100.0, "amount_in_wei": amount_in.to_string(), "out_units": out_q.to_string(), "loss_bps_vs_spot": eff_bps.to_string() }));
    }
    out.insert("pool_reserve_wsigil_wei".into(), res_w.to_string().into());
    out.insert("pool_reserve_usdc_units".into(), res_u.to_string().into());
    out.insert("pool_spot_usdc_per_wsigil_1e18".into(), spot.to_string().into());
    out.insert("slippage".into(), slip.into());

    let intent = CrossChainIntent { source: ExternalChain::Bitcoin, input: AssetId::Btc, amount_in: 100_000, destination: ExternalChain::Polygon, output: AssetId::Usdc, min_output: 1, recipient: [0xd7; 20], privacy: Privacy::ShieldedTransit, deadline: 2_000_000_000, nonce: 1 };
    out.insert("intent_memo_bytes".into(), (intent.exit_memo().encode().len() as u64).into());
    out.insert("memo_budget_bytes".into(), (sigil_shield::note_cipher::MEMO_LEN as u64).into());

    println!("{}", serde_json::to_string_pretty(&serde_json::Value::Object(out)).unwrap());
}
