//! verify.rs — the first MandatPilot product: CVR-Verify. Charges 10 credits, then
//! cross-checks the MitID Erhverv claims against the open CVR register and returns a
//! proven answer. The edge (flux-id-mcp) fetches the MitID claims + the CVR record;
//! this is the on-chain settlement + the cross-check logic.

use crate::{debit_action, MandatError};
use flux_uint::Amount;
use sigil_state::{SigilState, StateTransition, WalletId};

/// 10 credits per verification.
pub const VERIFY_COST: Amount = Amount::from_ore(10);

/// What MitID Erhverv asserts (from the OIDC login).
#[derive(Clone, Debug)]
pub struct MitidClaims {
    pub cvr: String,
    pub person_name: String,
    pub is_signatory: bool,
}

/// What the open CVR register says (cvrapi.dk / Virk). Free fields, no registration.
#[derive(Clone, Debug)]
pub struct CvrRecord {
    pub cvr: String,
    pub company_name: String,
    pub active: bool,
    pub bankrupt: bool,   // cvrapi `creditbankrupt` — the risk signal
    pub employees: u32,
    pub industry: String, // cvrapi `industrydesc`
}

/// The proven result the customer gets back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifyResult {
    pub verified: bool,
    pub cvr: String,
    pub company_name: String,
    pub signatory: bool,
    pub company_active: bool,
    pub bankrupt: bool,
    pub employees: u32,
    pub industry: String,
}

/// Run a CVR-Verify: charge 10 credits (overdraw-guarded), cross-check MitID × CVR
/// register, return the transition to commit + the proven result. The verification
/// RAN regardless of outcome, so it charges even when the cross-check is negative —
/// insufficient credits is the only thing that blocks (no charge, no verify).
pub fn verify_business(
    state: &SigilState,
    account: &WalletId,
    claims: &MitidClaims,
    cvr: &CvrRecord,
    at_height: u64,
) -> Result<(StateTransition, VerifyResult), MandatError> {
    let t = debit_action(state, account, VERIFY_COST, at_height)?;
    let verified = claims.is_signatory && cvr.active && claims.cvr == cvr.cvr;
    let res = VerifyResult {
        verified,
        cvr: claims.cvr.clone(),
        company_name: cvr.company_name.clone(),
        signatory: claims.is_signatory,
        company_active: cvr.active,
        bankrupt: cvr.bankrupt,
        employees: cvr.employees,
        industry: cvr.industry.clone(),
    };
    Ok((t, res))
}
