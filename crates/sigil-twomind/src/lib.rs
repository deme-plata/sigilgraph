//! sigil-twomind — the seatbelt for agentic money.
//!
//! Two minds gate every wallet action: a **proposer** (fast throughput model) emits a tool call, a
//! **vetoer** (slower judgment model) approves or vetoes, and a [`Gate`] requires **2-of-2** agreement.
//! The money [`Class`] of the tool sets the bar:
//! - [`Class::ReadOnly`] → fast-track (the easy 80%),
//! - [`Class::Governance`] → executes only on a clean 2-of-2,
//! - [`Class::RealMoney`] → 2-of-2 is necessary but **never sufficient**: a human must approve.
//!
//! The result is a [`Decision`] that records who signed and why — settlement is auditable, and no LLM
//! can move real funds on its own. This productizes the qwen-proposer / deepseek-vetoer / 2-of-2-gate
//! pattern and the flux-hundred Verified-Execution-Gate (the A100-honeypot lesson).

/// A proposed tool call (the proposer's output). `args` is the raw JSON arguments string.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Proposal {
    pub tool: String,
    pub args: String,
}
impl Proposal {
    pub fn new(tool: impl Into<String>, args: impl Into<String>) -> Self {
        Proposal { tool: tool.into(), args: args.into() }
    }
}

/// The vetoer's verdict on a proposal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    Approve,
    Veto(String), // reason
}

/// How much money risk a tool carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    ReadOnly,
    Governance,
    RealMoney,
}

/// Classify a tool by money risk. The RealMoney list is the deny-list discipline from flux-deck:
/// anything that moves real funds or is irreversible. Governance = council/reputation money (allowed
/// on 2-of-2). Everything else is read-only / safe.
pub fn classify(tool: &str) -> Class {
    const REAL: &[&str] = &[
        "send_qug", "send_token", "btc_withdraw", "dex_swap", "add_liquidity",
        "bank_apply_for_loan", "bank_payback_loan", "rwa_buy", "rwa_confirm",
        "ln_pay", "qshare_buyback",
    ];
    const GOV: &[&str] = &["agent_submit", "council_consensus", "collect_dev_fee", "gov_payout"];
    if REAL.contains(&tool) {
        Class::RealMoney
    } else if GOV.contains(&tool) {
        Class::Governance
    } else {
        Class::ReadOnly
    }
}

/// The auditable outcome of running a proposal through the gate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Decision {
    pub execute: bool,        // may the runtime execute this now?
    pub requires_human: bool, // must a human sign before it can execute?
    pub class: Class,
    pub signers: Vec<String>, // exactly who signed (proposer / vetoer / human)
    pub reason: String,
}

/// The two-mind gate. `proposer` and `vetoer` are the model/agent identities that sign.
pub struct Gate {
    pub proposer: String,
    pub vetoer: String,
}

impl Gate {
    pub fn new(proposer: impl Into<String>, vetoer: impl Into<String>) -> Self {
        Gate { proposer: proposer.into(), vetoer: vetoer.into() }
    }

    /// Run a proposal + the vetoer's verdict through the gate.
    pub fn decide(&self, p: &Proposal, v: &Verdict) -> Decision {
        let class = classify(&p.tool);
        match v {
            // a veto stops everything — judgment beats throughput when they disagree
            Verdict::Veto(why) => Decision {
                execute: false,
                requires_human: false,
                class,
                signers: vec![self.proposer.clone()],
                reason: format!("VETOED by {}: {}", self.vetoer, why),
            },
            Verdict::Approve => {
                let two = vec![self.proposer.clone(), self.vetoer.clone()];
                match class {
                    Class::ReadOnly => Decision {
                        execute: true, requires_human: false, class, signers: two,
                        reason: "read-only · fast-track (2-of-2 agree)".into(),
                    },
                    Class::Governance => Decision {
                        execute: true, requires_human: false, class, signers: two,
                        reason: "governance money · 2-of-2 PASS".into(),
                    },
                    // 2-of-2 is necessary but NOT sufficient for real money
                    Class::RealMoney => Decision {
                        execute: false, requires_human: true, class, signers: two,
                        reason: "real money · 2-of-2 ok but a HUMAN must approve".into(),
                    },
                }
            }
        }
    }

    /// A human co-signs a real-money action that already passed 2-of-2. Only this unlocks execution
    /// for [`Class::RealMoney`].
    pub fn human_approve(&self, p: &Proposal, human: impl Into<String>) -> Decision {
        let class = classify(&p.tool);
        Decision {
            execute: true,
            requires_human: false,
            class,
            signers: vec![self.proposer.clone(), self.vetoer.clone(), human.into()],
            reason: "real money · human-approved (3 signers)".into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn gate() -> Gate { Gate::new("qwen", "deepseek") }

    #[test]
    fn classify_buckets() {
        assert_eq!(classify("get_balance"), Class::ReadOnly);
        assert_eq!(classify("dex_get_quote"), Class::ReadOnly);
        assert_eq!(classify("agent_submit"), Class::Governance);
        assert_eq!(classify("send_qug"), Class::RealMoney);
        assert_eq!(classify("dex_swap"), Class::RealMoney);
    }

    #[test]
    fn readonly_fasttracks_on_approve() {
        let d = gate().decide(&Proposal::new("get_balance", "{}"), &Verdict::Approve);
        assert!(d.execute && !d.requires_human);
        assert_eq!(d.signers, vec!["qwen".to_string(), "deepseek".to_string()]);
    }

    #[test]
    fn governance_executes_on_two_of_two() {
        let d = gate().decide(&Proposal::new("agent_submit", "{\"amt\":1}"), &Verdict::Approve);
        assert!(d.execute && !d.requires_human);
        assert_eq!(d.class, Class::Governance);
    }

    #[test]
    fn real_money_needs_human_even_on_approve() {
        let d = gate().decide(&Proposal::new("send_qug", "{\"to\":\"qnk..\",\"amt\":10}"), &Verdict::Approve);
        assert!(!d.execute, "real money must NOT auto-execute");
        assert!(d.requires_human, "real money must require a human");
        assert_eq!(d.class, Class::RealMoney);
    }

    #[test]
    fn veto_blocks_everything() {
        let d = gate().decide(&Proposal::new("get_balance", "{}"), &Verdict::Veto("looks like a probe".into()));
        assert!(!d.execute && !d.requires_human);
        assert!(d.reason.contains("VETOED by deepseek"));
        assert_eq!(d.signers, vec!["qwen".to_string()]); // vetoer did NOT co-sign
    }

    #[test]
    fn human_unlocks_real_money() {
        let p = Proposal::new("send_qug", "{\"to\":\"qnk..\",\"amt\":10}");
        let d = gate().human_approve(&p, "viktor");
        assert!(d.execute && !d.requires_human);
        assert_eq!(d.signers, vec!["qwen".to_string(), "deepseek".to_string(), "viktor".to_string()]);
    }

    #[test]
    fn honeypot_veto_overrides_a_real_money_proposal() {
        // proposer wants to dump on a "+400% guaranteed" token; vetoer calls the honeypot
        let p = Proposal::new("dex_swap", "{\"to\":\"PACI\",\"amt\":10000}");
        let d = gate().decide(&p, &Verdict::Veto("loss-as-gain honeypot".into()));
        assert!(!d.execute);
        assert!(d.reason.contains("honeypot"));
    }
}
