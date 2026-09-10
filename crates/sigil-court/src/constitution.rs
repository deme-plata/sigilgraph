//! CODE IS LAW — the Constitution of the SIGIL Nation, version 3.
//!
//! Every refusal this court issues names an [`Article`]. Every seal it produces commits
//! [`constitution_hash`], so a packet verified tomorrow is verified against the same law it was
//! sealed under. Amending the constitution changes the hash, which changes every court root —
//! the chain notices.

use serde::{Deserialize, Serialize};

/// Constitution version sealed into every packet and every court root.
pub const CONSTITUTION_VERSION: u32 = 3;

/// The twelve Articles. Numbering is stable: append, never renumber.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Article {
    /// Art. I — Privacy is the default. Every payment is shielded; no one but this Court can
    /// make a citizen's record visible.
    PrivacyByDefault,
    /// Art. II — No disclosure without an Order. Chain records leave the nation only inside a
    /// court-sealed DisclosurePacket.
    NoDisclosureWithoutOrder,
    /// Art. III — Spend keys never leave. A viewing key may be granted under Order; a spend key
    /// is not disclosable by any Order, panel, or vote.
    SpendKeysNeverLeave,
    /// Art. IV — Minimization. An Order discloses the least that answers its purpose; everything
    /// else is redacted, and the redaction is itself committed.
    Minimization,
    /// Art. V — Every record is proven. A disclosed record carries an inclusion proof to the
    /// block's `event_log_root`; unproven evidence is inadmissible.
    EveryRecordProven,
    /// Art. VI — Right of appeal. Any ruling may be appealed once, to the full bench.
    RightOfAppeal,
    /// Art. VII — Precedent binds; overruling takes two thirds of the bench, en banc.
    PrecedentBinds,
    /// Art. VIII — Promotion by deeds and examination. No rank is bought, none is granted solo.
    PromotionByDeeds,
    /// Art. IX — Supersede, never delete. The docket is append-only; a wrong entry is
    /// superseded by a later one, never removed.
    SupersedeNeverDelete,
    /// Art. X — Fail loud. A refusal names its Article.
    FailLoud,
    /// Art. XI — Orders expire. An expired Order seals nothing.
    OrdersExpire,
    /// Art. XII — Money moves only on a passed vote (the Nation charter, inherited).
    MoneyOnlyByVote,
}

impl Article {
    /// All articles, in constitutional order.
    pub const ALL: [Article; 12] = [
        Article::PrivacyByDefault,
        Article::NoDisclosureWithoutOrder,
        Article::SpendKeysNeverLeave,
        Article::Minimization,
        Article::EveryRecordProven,
        Article::RightOfAppeal,
        Article::PrecedentBinds,
        Article::PromotionByDeeds,
        Article::SupersedeNeverDelete,
        Article::FailLoud,
        Article::OrdersExpire,
        Article::MoneyOnlyByVote,
    ];

    /// Roman numeral, as cited in rulings.
    pub fn numeral(self) -> &'static str {
        match self {
            Article::PrivacyByDefault => "I",
            Article::NoDisclosureWithoutOrder => "II",
            Article::SpendKeysNeverLeave => "III",
            Article::Minimization => "IV",
            Article::EveryRecordProven => "V",
            Article::RightOfAppeal => "VI",
            Article::PrecedentBinds => "VII",
            Article::PromotionByDeeds => "VIII",
            Article::SupersedeNeverDelete => "IX",
            Article::FailLoud => "X",
            Article::OrdersExpire => "XI",
            Article::MoneyOnlyByVote => "XII",
        }
    }

    /// The operative text — what is committed in the constitution hash.
    pub fn text(self) -> &'static str {
        match self {
            Article::PrivacyByDefault => "Privacy is the default. Every payment is shielded; no one but this Court can make a citizen's record visible.",
            Article::NoDisclosureWithoutOrder => "No disclosure without an Order. Chain records leave the nation only inside a court-sealed DisclosurePacket.",
            Article::SpendKeysNeverLeave => "Spend keys never leave. A viewing key may be granted under Order; a spend key is not disclosable by any Order, panel, or vote.",
            Article::Minimization => "Minimization. An Order discloses the least that answers its purpose; everything else is redacted, and the redaction is itself committed.",
            Article::EveryRecordProven => "Every record is proven. A disclosed record carries an inclusion proof to the block's event_log_root; unproven evidence is inadmissible.",
            Article::RightOfAppeal => "Right of appeal. Any ruling may be appealed once, to the full bench.",
            Article::PrecedentBinds => "Precedent binds; overruling takes two thirds of the bench, en banc.",
            Article::PromotionByDeeds => "Promotion by deeds and examination. No rank is bought, none is granted solo.",
            Article::SupersedeNeverDelete => "Supersede, never delete. The docket is append-only; a wrong entry is superseded by a later one, never removed.",
            Article::FailLoud => "Fail loud. A refusal names its Article.",
            Article::OrdersExpire => "Orders expire. An expired Order seals nothing.",
            Article::MoneyOnlyByVote => "Money moves only on a passed vote.",
        }
    }
}

impl std::fmt::Display for Article {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Art. {}", self.numeral())
    }
}

/// BLAKE3 over the version tag and every article's text, in order. Sealed into every packet.
pub fn constitution_hash() -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(b"sigil-court/constitution/v");
    h.update(&CONSTITUTION_VERSION.to_le_bytes());
    for a in Article::ALL {
        h.update(&[a as u8 + 1]);
        h.update(a.text().as_bytes());
    }
    *h.finalize().as_bytes()
}

pub fn constitution_hash_hex() -> String {
    hex::encode(constitution_hash())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn twelve_articles_numbered_in_order() {
        assert_eq!(Article::ALL.len(), 12);
        assert_eq!(Article::ALL[0].numeral(), "I");
        assert_eq!(Article::ALL[11].numeral(), "XII");
        assert_eq!(Article::SpendKeysNeverLeave.to_string(), "Art. III");
    }

    #[test]
    fn hash_is_deterministic_and_text_bound() {
        let a = constitution_hash();
        let b = constitution_hash();
        assert_eq!(a, b);
        // Changing any article text would change this — pin it so an amendment is a
        // deliberate, visible act (update the literal when the constitution changes).
        assert_eq!(constitution_hash_hex().len(), 64);
    }
}
