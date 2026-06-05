//! presence.rs — **Score King**: spoof-resistant proof-of-presence earning + on-chain
//! shop revenue-shares.
//!
//! The map becomes the ledger: a human (or delivery robot) proves presence at a
//! physical **beacon** (shop / gas station / power station) and earns `QCREDIT`.
//! The hard part is anti-spoof — GPS lies — so every claim passes a gate:
//!   1. **cooldown** — can't re-farm the same beacon faster than `COOLDOWN_S`.
//!   2. **movement plausibility** — can't be at two beacons whose distance/Δt
//!      implies a speed above `MAX_SPEED_KMH` (the teleport check — the same
//!      "can't be in two places at once" idea The Hundred's gate uses).
//!   3. **signed nonce** — the beacon co-signs a fresh challenge you can only
//!      get on-site (modelled here as a presence proof; the real beacon holds a
//!      key). Stake-to-claim + slashing layer on top later.
//!
//! Shops are revenue-sharing **smart-contract entities**: `buy_shop_share` mints
//! a per-shop share token against payment to the shop treasury; the network (or
//! users) thereby "buy the shop or its shares to get part of the revenue", and
//! `distribute_shop_revenue` splits incoming revenue pro-rata to shareholders.
//! Every write goes through `commit_state_transition` — settled, root-committed,
//! 21M-cap-guarded for NATIVE.

use sigil_state::{
    commit_state_transition, SigilState, StateMutation, StateTransition, TokenId, WalletId, NATIVE,
};

/// The location-reward token earned by proving presence.
pub const QCREDIT: TokenId = [0xC1; 32];

/// Anti-farm cooldown per (user, beacon), seconds.
pub const COOLDOWN_S: u64 = 300;
/// Above this implied speed between two claims, it's a teleport/spoof.
pub const MAX_SPEED_KMH: f64 = 900.0; // generous (covers a flight); tune down per-tier

#[derive(Debug, Clone, PartialEq)]
pub enum PresenceError {
    Cooldown { since_s: u64 },
    Teleport { implied_kmh: f64 },
    BadProof,
    State(String),
}

/// A physical beacon (shop / gas / power node) the network recognises.
#[derive(Debug, Clone, Copy)]
pub struct Beacon {
    pub id: [u8; 32],
    pub lat: f64,
    pub lon: f64,
}

/// The user's previous claim, for the movement-plausibility check.
#[derive(Debug, Clone, Copy)]
pub struct LastClaim {
    pub lat: f64,
    pub lon: f64,
    pub ts: u64,
    /// Whether the previous claim was at THIS same beacon (for cooldown).
    pub same_beacon: bool,
}

/// Great-circle distance in km (haversine).
pub fn haversine_km(a_lat: f64, a_lon: f64, b_lat: f64, b_lon: f64) -> f64 {
    let r = 6371.0_f64;
    let (la1, la2) = (a_lat.to_radians(), b_lat.to_radians());
    let dla = (b_lat - a_lat).to_radians();
    let dlo = (b_lon - a_lon).to_radians();
    let h = (dla / 2.0).sin().powi(2) + la1.cos() * la2.cos() * (dlo / 2.0).sin().powi(2);
    2.0 * r * h.sqrt().asin()
}

/// The anti-spoof gate. Returns Ok if the claim is physically plausible.
pub fn presence_gate(
    beacon: &Beacon,
    now_ts: u64,
    last: Option<&LastClaim>,
    proof_ok: bool,
) -> Result<(), PresenceError> {
    if !proof_ok {
        return Err(PresenceError::BadProof); // beacon-signed nonce must verify
    }
    if let Some(l) = last {
        let dt = now_ts.saturating_sub(l.ts);
        if l.same_beacon && dt < COOLDOWN_S {
            return Err(PresenceError::Cooldown { since_s: dt });
        }
        // teleport check: distance / time implies an impossible speed
        let km = haversine_km(l.lat, l.lon, beacon.lat, beacon.lon);
        if km > 0.05 && dt > 0 {
            let kmh = km / (dt as f64 / 3600.0);
            if kmh > MAX_SPEED_KMH {
                return Err(PresenceError::Teleport { implied_kmh: kmh });
            }
        } else if km > 0.05 && dt == 0 {
            return Err(PresenceError::Teleport { implied_kmh: f64::INFINITY });
        }
    }
    Ok(())
}

/// Prove presence at a beacon → gate → credit `reward` QCREDIT to `user`.
/// Returns the user's new QCREDIT balance. Settled + root-committed.
#[allow(clippy::too_many_arguments)]
pub fn claim_presence(
    state: &mut SigilState,
    height: u64,
    user: WalletId,
    beacon: &Beacon,
    now_ts: u64,
    last: Option<&LastClaim>,
    proof_ok: bool,
    reward: u128,
) -> Result<u128, PresenceError> {
    presence_gate(beacon, now_ts, last, proof_ok)?;
    let pre = state.balance_of(&user, &QCREDIT);
    let t = StateTransition {
        at_height: height,
        mutations: vec![StateMutation::SetBalance { wallet: user, token: QCREDIT, amount: pre + reward }],
    };
    commit_state_transition(state, &t, height).map_err(|e| PresenceError::State(e.to_string()))?;
    Ok(pre + reward)
}

/// Per-shop share token id (first 31 bytes of the shop id, 0xS5 tag in byte 0).
pub fn shop_share_token(shop_id: &[u8; 32]) -> TokenId {
    let mut t = *shop_id;
    t[0] = 0x55; // 'S' tag so share tokens are distinguishable from wallets
    t
}

/// Buy `shares` of a shop: `payer` pays `cost` NATIVE to the shop `treasury`,
/// and receives `shares` of the shop's share token (minted). This is the
/// "buy the shop or its shares" mechanism. Returns payer's new share balance.
pub fn buy_shop_share(
    state: &mut SigilState,
    height: u64,
    payer: WalletId,
    treasury: WalletId,
    shop_id: &[u8; 32],
    cost: u128,
    shares: u128,
) -> Result<u128, PresenceError> {
    let pay_pre = state.balance_of(&payer, &NATIVE);
    if pay_pre < cost {
        return Err(PresenceError::State("insufficient NATIVE for shares".into()));
    }
    let share_tok = shop_share_token(shop_id);
    let treas_pre = state.balance_of(&treasury, &NATIVE);
    let share_pre = state.balance_of(&payer, &share_tok);
    let t = StateTransition {
        at_height: height,
        mutations: vec![
            // pay NATIVE payer → treasury (conserved)
            StateMutation::SetBalance { wallet: payer, token: NATIVE, amount: pay_pre - cost },
            StateMutation::SetBalance { wallet: treasury, token: NATIVE, amount: treas_pre + cost },
            // mint shares to payer
            StateMutation::SetBalance { wallet: payer, token: share_tok, amount: share_pre + shares },
        ],
    };
    commit_state_transition(state, &t, height).map_err(|e| PresenceError::State(e.to_string()))?;
    Ok(share_pre + shares)
}

/// Distribute `revenue` (NATIVE) from `treasury` pro-rata to `holders`
/// (each `(wallet, shares)`), proportional to shares / total_shares. Dust from
/// integer division stays in the treasury. Returns total actually paid out.
pub fn distribute_shop_revenue(
    state: &mut SigilState,
    height: u64,
    treasury: WalletId,
    revenue: u128,
    holders: &[(WalletId, u128)],
) -> Result<u128, PresenceError> {
    let total_shares: u128 = holders.iter().map(|(_, s)| *s).sum();
    if total_shares == 0 || revenue == 0 {
        return Ok(0);
    }
    let treas_pre = state.balance_of(&treasury, &NATIVE);
    if treas_pre < revenue {
        return Err(PresenceError::State("treasury underfunded for distribution".into()));
    }
    let mut mutations = Vec::with_capacity(holders.len() + 1);
    let mut paid = 0u128;
    for (w, s) in holders {
        let cut = revenue.saturating_mul(*s) / total_shares;
        if cut == 0 {
            continue;
        }
        let pre = state.balance_of(w, &NATIVE);
        mutations.push(StateMutation::SetBalance { wallet: *w, token: NATIVE, amount: pre + cut });
        paid += cut;
    }
    // debit treasury by exactly what was paid (dust retained)
    mutations.push(StateMutation::SetBalance { wallet: treasury, token: NATIVE, amount: treas_pre - paid });
    commit_state_transition(state, &StateTransition { at_height: height, mutations }, height)
        .map_err(|e| PresenceError::State(e.to_string()))?;
    Ok(paid)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ME: WalletId = [0x11; 32];
    const SHOP: [u8; 32] = [0x5A; 32];
    const TREASURY: WalletId = [0x7E; 32];

    fn beacon(lat: f64, lon: f64) -> Beacon {
        Beacon { id: SHOP, lat, lon }
    }
    fn funded(native: u128) -> SigilState {
        let mut s = SigilState::new();
        let t = StateTransition {
            at_height: 0,
            mutations: vec![
                StateMutation::SetMasterWallet { wallet: [0xFF; 32] },
                StateMutation::SetBalance { wallet: ME, token: NATIVE, amount: native },
            ],
        };
        commit_state_transition(&mut s, &t, 0).unwrap();
        s
    }

    #[test]
    fn genuine_presence_credits_qcredit() {
        let mut s = funded(0);
        let b = beacon(55.6761, 12.5683); // Copenhagen
        let bal = claim_presence(&mut s, 1, ME, &b, 1000, None, true, 10).unwrap();
        assert_eq!(bal, 10);
        assert_eq!(s.balance_of(&ME, &QCREDIT), 10);
    }

    #[test]
    fn teleport_is_blocked() {
        // Copenhagen at t=0, then Tokyo 60s later → ~8600 km in a minute = spoof
        let mut s = funded(0);
        let last = LastClaim { lat: 55.6761, lon: 12.5683, ts: 1000, same_beacon: false };
        let tokyo = Beacon { id: [0x01; 32], lat: 35.6762, lon: 139.6503 };
        let r = claim_presence(&mut s, 1, ME, &tokyo, 1060, Some(&last), true, 10);
        assert!(matches!(r, Err(PresenceError::Teleport { .. })), "got {r:?}");
    }

    #[test]
    fn cooldown_blocks_refarm() {
        let mut s = funded(0);
        let b = beacon(55.6761, 12.5683);
        let last = LastClaim { lat: b.lat, lon: b.lon, ts: 1000, same_beacon: true };
        let r = claim_presence(&mut s, 1, ME, &b, 1100, Some(&last), true, 10); // only 100s < 300
        assert!(matches!(r, Err(PresenceError::Cooldown { .. })), "got {r:?}");
    }

    #[test]
    fn walking_pace_is_allowed() {
        // 0.3 km in 300s = 3.6 km/h, a stroll → fine
        let mut s = funded(0);
        let last = LastClaim { lat: 55.6761, lon: 12.5683, ts: 1000, same_beacon: false };
        let near = beacon(55.6788, 12.5683); // ~0.3 km north
        let bal = claim_presence(&mut s, 1, ME, &near, 1300, Some(&last), true, 10).unwrap();
        assert_eq!(bal, 10);
    }

    #[test]
    fn bad_proof_rejected() {
        let mut s = funded(0);
        let b = beacon(55.6761, 12.5683);
        assert!(matches!(claim_presence(&mut s, 1, ME, &b, 1000, None, false, 10), Err(PresenceError::BadProof)));
    }

    #[test]
    fn buy_shares_then_revenue_distributes_prorata() {
        let mut s = funded(1000);
        // ME buys 70 shares for 700 NATIVE; a second holder buys 30 for 300.
        let other: WalletId = [0x22; 32];
        // fund other
        let op = s.balance_of(&other, &NATIVE);
        commit_state_transition(&mut s, &StateTransition { at_height: 1, mutations: vec![
            StateMutation::SetBalance { wallet: other, token: NATIVE, amount: op + 300 }] }, 1).unwrap();

        let me_sh = buy_shop_share(&mut s, 2, ME, TREASURY, &SHOP, 700, 70).unwrap();
        let ot_sh = buy_shop_share(&mut s, 3, other, TREASURY, &SHOP, 300, 30).unwrap();
        assert_eq!(me_sh, 70);
        assert_eq!(ot_sh, 30);
        assert_eq!(s.balance_of(&TREASURY, &NATIVE), 1000); // 700 + 300 collected

        // shop earns 100 revenue → 70/30 split
        let me_pre = s.balance_of(&ME, &NATIVE);
        let ot_pre = s.balance_of(&other, &NATIVE);
        let paid = distribute_shop_revenue(&mut s, 4, TREASURY, 100,
            &[(ME, 70), (other, 30)]).unwrap();
        assert_eq!(paid, 100);
        assert_eq!(s.balance_of(&ME, &NATIVE) - me_pre, 70);
        assert_eq!(s.balance_of(&other, &NATIVE) - ot_pre, 30);
        assert_eq!(s.balance_of(&TREASURY, &NATIVE), 900); // 1000 - 100 distributed
    }

    #[test]
    fn share_token_is_tagged() {
        assert_eq!(shop_share_token(&SHOP)[0], 0x55);
    }
}
