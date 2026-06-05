//! # template-forge — auto-propose project templates
//!
//! Generates innovative project blueprints, each ~50 features + a dedicated
//! SIGIL task list, seeded from wickes-cms, sigil-spawn, and novel domains.
//! Every template = a domain feature pool + the shared flux-native platform
//! spine, so they all inherit QUG payments, PQ auth, MCP, flux-db, etc.
//!
//! The catalog feeds the template-picker UI (`template-picker.html`): pick
//! several to build simultaneously, filter/sort their features, and let the
//! built-in advisor suggest the next task as you go.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Proposed,
    Planned,
    Building,
    Done,
}

impl Default for Status {
    fn default() -> Self {
        Status::Proposed
    }
}

/// Effort weight, used by the advisor to order suggestions (low effort + high
/// leverage first).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Effort {
    S,
    M,
    L,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Feature {
    pub name: String,
    pub category: String,
    pub status: Status,
    pub effort: Effort,
}

impl Feature {
    fn new(name: &str, category: &str, effort: Effort) -> Self {
        Self { name: name.into(), category: category.into(), status: Status::Proposed, effort }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectTemplate {
    pub id: String,
    pub name: String,
    pub tagline: String,
    pub kind: String,
    pub icon: String,
    pub features: Vec<Feature>,
    /// Dedicated SIGIL build tasks (the ordered "do this" list).
    pub sigil_tasks: Vec<String>,
}

impl ProjectTemplate {
    pub fn feature_count(&self) -> usize {
        self.features.len()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Catalog {
    pub generated_unix: u64,
    pub templates: Vec<ProjectTemplate>,
}

/// The shared flux-native platform spine — every template inherits these,
/// which is what makes even a "simple" template best-in-class. (20 features.)
const PLATFORM_SPINE: &[(&str, &str, Effort)] = &[
    ("QUG-native payments", "payments", Effort::M),
    ("Post-quantum auth (SQIsign/Dilithium)", "security", Effort::M),
    ("MCP tool surface (AI-agent control)", "ai", Effort::M),
    ("flux-db embedded store (no PG/Redis)", "data", Effort::S),
    ("flux-search full-text + ranking", "data", Effort::M),
    ("flux-p2p multi-node sync", "infra", Effort::L),
    ("Single-binary deploy (flux serve)", "infra", Effort::S),
    ("Content-hash build cache", "infra", Effort::S),
    ("Role-based access control", "security", Effort::M),
    ("On-chain audit trail", "security", Effort::M),
    ("Webhooks + event bus", "infra", Effort::S),
    ("Realtime WebSocket/SSE", "ux", Effort::M),
    ("i18n / localization", "ux", Effort::S),
    ("Theming + design system", "ux", Effort::S),
    ("Rate limiting / DoS guard", "security", Effort::S),
    ("Backup / restore (flux-db snapshot)", "infra", Effort::S),
    ("Observability + perf/watt metrics", "infra", Effort::M),
    ("Admin dashboard", "ux", Effort::M),
    ("Auto-generated API + SDKs (flux-api)", "api", Effort::M),
    ("Self-update channel", "infra", Effort::S),
];

/// (id, name, icon, kind, tagline, domain features, sigil tasks)
type Seed = (
    &'static str,
    &'static str,
    &'static str,
    &'static str,
    &'static str,
    &'static [(&'static str, &'static str, Effort)],
    &'static [&'static str],
);

fn seeds() -> Vec<Seed> {
    use Effort::*;
    vec![
        (
            "wickes-commerce", "Wickes Commerce", "🏬", "cms-erp",
            "Headless CMS + SAP-grade ERP, AI-managed, QUG-paid.",
            &[
                ("Generic content types", "cms", S), ("Notion-style block editor", "cms", L),
                ("Media library + image optimize", "cms", M), ("SEO + sitemap + OpenGraph", "cms", S),
                ("Editorial workflow (draft→review→publish)", "cms", M), ("Versioning + rollback", "cms", M),
                ("Inventory by location", "erp", M), ("Sales orders + ATP check", "erp", M),
                ("Purchase orders + receiving", "erp", M), ("MRP planning engine", "erp", L),
                ("BOM explosion", "erp", L), ("Warehouse / bin management", "erp", M),
                ("Invoicing from orders", "finance", M), ("General Ledger + chart of accounts", "finance", L),
                ("AP / AR aging", "finance", M), ("VAT / GST tax reports", "finance", M),
                ("CRM contacts + pipeline", "crm", M), ("Quotes → orders", "crm", M),
                ("Support tickets", "crm", M), ("HCM employees + payroll", "hcm", L),
                ("Time tracking + leave", "hcm", M), ("AI content suggestions", "ai", M),
                ("Storefront + cart + checkout", "cms", M), ("Multi-currency pricing", "finance", M),
                ("Discounts + coupons", "cms", S), ("Subscriptions + recurring billing", "finance", M),
                ("Returns / RMA", "erp", M), ("Vendor portal", "erp", M),
                ("Reporting + KPI dashboard", "finance", M), ("Module enable/disable", "cms", S),
            ],
            &["scaffold wickes-cms (DONE)", "wire wickes-erp inventory+orders", "wickes-finance GL + invoicing", "wickes-mcp 30 tools", "wickes-ui storefront + admin"],
        ),
        (
            "forge-chain", "Forge Chain", "⛓️", "chain",
            "Spawn a sovereign PoW chain in minutes, supply-capped by construction.",
            &[
                ("ChainSpec (26 knobs)", "chain", S), ("Name-derived non-colliding identity", "chain", S),
                ("Genesis commitment", "chain", S), ("Blake4 PoW miner", "consensus", L),
                ("Halving emission schedule", "economics", M), ("Hard supply cap (validate)", "economics", S),
                ("Difficulty retarget", "consensus", M), ("Mempool (sharded)", "consensus", M),
                ("Block explorer", "ux", M), ("Lightweight tip-verify node", "consensus", M),
                ("Chronos deterministic test harness", "consensus", M), ("DEX / AMM pools", "defi", L),
                ("Price oracle", "defi", M), ("Native stablecoin", "defi", L),
                ("Staking + delegation", "consensus", L), ("On-chain governance", "governance", L),
                ("Cross-chain bridge (SPV proof)", "defi", L), ("Wallet (PQ keys)", "ux", M),
                ("Faucet (testnet)", "ux", S), ("Premine + treasury allocation", "economics", S),
                ("Dev-fee routing", "economics", S), ("Fee market", "economics", M),
                ("Coinbase maturity rules", "consensus", S), ("Reorg / fork handling", "consensus", L),
                ("Snapshot + fast sync", "infra", M), ("Miner payout pool", "consensus", M),
                ("Emission preview tooling", "economics", S), ("Node config scaffold (node.toml)", "infra", S),
                ("Tip-proof browser verify", "consensus", M), ("Multi-chain spawn (fleet)", "chain", S),
            ],
            &["scaffold sigil-spawn (DONE)", "wire Blake4 miner to spec", "chronos harness per spawned chain", "explorer + wallet UI", "DEX + oracle + stablecoin"],
        ),
        (
            "agora-market", "Agora Market", "🛒", "marketplace",
            "Trustless two-sided marketplace with escrow + on-chain reputation.",
            &[
                ("Listings + categories", "market", S), ("Faceted search + filters", "market", M),
                ("QUG escrow", "market", M), ("Dispute resolution + arbitration", "market", L),
                ("Reputation + reviews", "market", M), ("Seller storefronts", "market", M),
                ("Offers + bidding", "market", M), ("Auctions (timed)", "market", M),
                ("Order tracking", "market", S), ("Shipping integrations", "market", M),
                ("Commission / fee split", "economics", S), ("Wishlists + saved searches", "market", S),
                ("Recommendations", "ai", M), ("Fraud scoring", "ai", M),
                ("Verified-seller badges", "market", S), ("Bulk import (CSV)", "market", S),
                ("Refunds + chargebacks", "market", M), ("Digital goods delivery", "market", M),
                ("Affiliate / referral", "economics", M), ("Tax handling", "finance", M),
                ("Messaging buyer↔seller", "ux", M), ("Inventory sync", "erp", M),
                ("Promotions + featured slots", "market", S), ("Multi-vendor payouts", "economics", M),
                ("Watch / price-drop alerts", "ux", S), ("Categories taxonomy editor", "market", S),
                ("Return windows + policies", "market", S), ("Seller analytics", "finance", M),
                ("Geo / locale storefronts", "ux", M), ("Trust & safety moderation", "ai", M),
            ],
            &["model listings + escrow on flux-db", "search + filters via flux-search", "QUG escrow + dispute flow", "reputation engine", "seller + buyer UI"],
        ),
        (
            "pulse-social", "Pulse Social", "📡", "social",
            "Owned-by-everyone social graph: posts, feed, DMs, PQ identity.",
            &[
                ("Profiles + PQ identity", "social", M), ("Follow graph", "social", M),
                ("Posts + media", "social", S), ("Ranked feed", "social", L),
                ("Comments + threads", "social", M), ("Reactions + emoji", "social", S),
                ("Direct messages (E2E)", "social", L), ("Group chats", "social", M),
                ("Notifications", "ux", M), ("Hashtags + topics", "social", S),
                ("Mentions", "social", S), ("Bookmarks", "social", S),
                ("Content moderation (AI)", "ai", M), ("Reporting + appeals", "social", M),
                ("Creator monetization (QUG tips)", "economics", M), ("Subscriptions to creators", "economics", M),
                ("Stories / ephemeral", "social", M), ("Live + realtime presence", "ux", M),
                ("Polls", "social", S), ("Trends", "social", M),
                ("Search users + posts", "data", M), ("Block / mute", "social", S),
                ("Verified identity (DNS-anchored)", "security", M), ("Federation / portability", "infra", L),
                ("Recommendation of follows", "ai", M), ("Spam / sybil resistance", "security", M),
                ("Media transcoding", "infra", M), ("Accessibility (a11y)", "ux", S),
                ("Analytics for creators", "data", M), ("Algorithmic transparency toggle", "social", M),
            ],
            &["profiles + follow graph on flux-db", "feed ranking", "E2E DMs via flux-p2p", "QUG tipping", "moderation + UI"],
        ),
        (
            "quorum-dao", "Quorum DAO", "🏛️", "dao",
            "On-chain governance with proof-of-debate and quadratic voting.",
            &[
                ("Proposals + lifecycle", "governance", M), ("Voting (token-weighted)", "governance", M),
                ("Quadratic voting", "governance", L), ("Proof-of-debate (2-of-N audit)", "governance", L),
                ("Delegation", "governance", M), ("Treasury management", "finance", M),
                ("Streaming payouts", "finance", M), ("Multisig execution", "security", L),
                ("Timelock", "security", M), ("Snapshot voting", "governance", M),
                ("Discussion + threads", "social", M), ("Reputation-weighted votes", "governance", M),
                ("Slashing for ghost proposals", "security", L), ("Grants program", "finance", M),
                ("Budget / runway dashboard", "finance", M), ("On-chain audit trail", "security", M),
                ("Role + committee management", "governance", M), ("Vote privacy (ZK)", "security", L),
                ("Proposal templates", "governance", S), ("Notifications + reminders", "ux", S),
                ("Off-chain signaling", "governance", S), ("1-of-2 fast-track (low-risk)", "governance", S),
                ("Constitution + rules engine", "governance", M), ("Member onboarding", "ux", S),
                ("Contribution scoring", "governance", M), ("Bounty board", "finance", M),
                ("Token distribution / vesting", "economics", M), ("Analytics + turnout", "data", M),
                ("Emergency veto", "security", M), ("Cross-DAO federation", "infra", L),
            ],
            &["proposal + voting model", "quadratic + delegation", "proof-of-debate quorum", "treasury + streaming", "governance UI"],
        ),
        (
            "arena-gamefi", "Arena GameFi", "🎮", "gamefi",
            "Play-to-earn arena: PQ-owned assets, on-chain leaderboards, QUG rewards.",
            &[
                ("Asset / item registry (PQ-owned)", "gamefi", M), ("Inventory + loadouts", "gamefi", M),
                ("Deterministic game state", "gamefi", L), ("Leaderboards", "gamefi", M),
                ("Tournaments + brackets", "gamefi", L), ("Matchmaking", "gamefi", M),
                ("QUG rewards + payouts", "economics", M), ("Crafting + upgrades", "gamefi", M),
                ("Marketplace for assets", "market", M), ("Rentals / lending of assets", "defi", M),
                ("Anti-cheat (server-authoritative)", "security", L), ("Replays + proofs", "gamefi", M),
                ("Seasons + battle pass", "gamefi", M), ("Quests + achievements", "gamefi", S),
                ("Guilds / clans", "social", M), ("Spectator + streaming", "ux", M),
                ("Skill rating (ELO)", "gamefi", M), ("In-game chat", "social", M),
                ("Cosmetics + skins", "gamefi", S), ("Energy / stamina economy", "economics", S),
                ("Random-but-fair drops (VRF)", "consensus", M), ("Cross-platform identity", "security", M),
                ("Asset provenance", "security", M), ("Creator tools / modding", "gamefi", L),
                ("Wagering / staking matches", "defi", M), ("Referral rewards", "economics", S),
                ("Live ops dashboard", "ux", M), ("Telemetry + balancing", "data", M),
                ("Sponsorship / ad slots", "economics", M), ("Mobile + desktop clients", "ux", L),
            ],
            &["asset registry + ownership", "deterministic state + leaderboards", "QUG reward payouts", "tournaments + matchmaking", "game clients"],
        ),
        (
            "atlas-ai", "Atlas AI Platform", "🧠", "ai-platform",
            "Agent marketplace: register agents, set mandates, settle work in QUG.",
            &[
                ("Agent registry", "ai", M), ("Capability advertisement", "ai", M),
                ("Mandates (spend limits)", "ai", M), ("Task board + claims", "ai", M),
                ("Settlement in QUG", "economics", M), ("2-of-N verified execution gate", "security", L),
                ("Model serving (vLLM/ollama)", "ai", L), ("Prompt + tool registry", "ai", M),
                ("Tool-call corpus + eval", "ai", L), ("Honeypot / safety harness", "security", M),
                ("Reputation + scoring", "ai", M), ("Webhook coordination", "infra", S),
                ("Compute fabric (rent GPUs)", "infra", L), ("Cost guardrails + autostop", "infra", M),
                ("Provenance-signed artifacts", "security", M), ("Benchmark + leaderboard", "ai", M),
                ("Agent-to-agent payments", "economics", M), ("Memory / long-context store", "ai", M),
                ("Sandbox execution", "security", L), ("Audit logs", "security", M),
                ("Routing / MoE dispatch", "ai", L), ("Fine-tune pipeline (QLoRA)", "ai", L),
                ("Dataset management", "data", M), ("Streaming responses", "ux", M),
                ("Rate + quota per agent", "infra", S), ("Multi-model fallback", "ai", M),
                ("Human-in-the-loop approvals", "ux", M), ("Observability (traces)", "infra", M),
                ("Marketplace of skills", "ai", M), ("Constitution / policy engine", "security", M),
            ],
            &["agent registry + mandates", "task board + claims", "QUG settlement + 2-of-N gate", "model serving + tools", "benchmark + UI"],
        ),
        (
            "ledger-finance", "Ledger Finance", "💳", "fintech",
            "Neobank rails on SIGIL: accounts, cards, lending, on-chain audit.",
            &[
                ("Accounts + sub-ledgers", "finance", M), ("Double-entry GL", "finance", L),
                ("Payments + transfers (QUG)", "payments", M), ("Virtual cards", "fintech", L),
                ("Lending + credit lines", "fintech", L), ("Interest accrual", "finance", M),
                ("KYC / AML (PQ identity)", "security", L), ("Statements + exports", "finance", S),
                ("FX / multi-currency", "finance", M), ("Recurring + scheduled payments", "payments", M),
                ("Invoicing", "finance", M), ("Treasury + yield (LP)", "defi", M),
                ("Fraud detection", "ai", M), ("Disputes + chargebacks", "fintech", M),
                ("Savings goals / vaults", "fintech", S), ("Budgeting + categorization", "finance", M),
                ("On-chain audit + proofs", "security", M), ("Webhooks for txns", "infra", S),
                ("Limits + controls", "security", S), ("Beneficiaries + payees", "payments", S),
                ("Bill pay", "payments", M), ("Rewards / cashback (QUG)", "economics", M),
                ("Tax reporting", "finance", M), ("Statements API", "api", M),
                ("Card freeze / controls", "fintech", S), ("Notifications", "ux", S),
                ("Reconciliation engine", "finance", L), ("Ledger snapshots", "infra", M),
                ("Compliance reporting", "security", M), ("Open-banking connectors", "api", L),
            ],
            &["accounts + double-entry GL", "QUG payment rails", "lending + interest", "KYC + on-chain audit", "cards + UI"],
        ),
    ]
}

fn assemble(seed: &Seed) -> ProjectTemplate {
    let (id, name, icon, kind, tagline, domain, tasks) = seed;
    let mut features: Vec<Feature> = domain
        .iter()
        .map(|(n, c, e)| Feature::new(n, c, *e))
        .collect();
    for (n, c, e) in PLATFORM_SPINE {
        features.push(Feature::new(n, c, *e));
    }
    ProjectTemplate {
        id: id.to_string(),
        name: name.to_string(),
        tagline: tagline.to_string(),
        kind: kind.to_string(),
        icon: icon.to_string(),
        features,
        sigil_tasks: tasks.iter().map(|t| t.to_string()).collect(),
    }
}

/// Auto-propose `n` templates (cycles the seed set if `n` exceeds it).
pub fn propose(n: usize) -> Vec<ProjectTemplate> {
    let s = seeds();
    (0..n).map(|i| assemble(&s[i % s.len()])).collect()
}

/// The full curated catalog (one of each seed).
pub fn catalog() -> Catalog {
    let templates: Vec<ProjectTemplate> = seeds().iter().map(assemble).collect();
    let generated_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    Catalog { generated_unix, templates }
}

/// Save the catalog + per-template JSON under `dir`. Returns the catalog path.
pub fn save_local(dir: impl AsRef<std::path::Path>) -> std::io::Result<std::path::PathBuf> {
    let dir = dir.as_ref();
    std::fs::create_dir_all(dir)?;
    let cat = catalog();
    for t in &cat.templates {
        std::fs::write(
            dir.join(format!("{}.json", t.id)),
            serde_json::to_string_pretty(t).unwrap(),
        )?;
    }
    let path = dir.join("catalog.json");
    std::fs::write(&path, serde_json::to_string_pretty(&cat).unwrap())?;
    Ok(path)
}

/// Context-aware "what to build next" advisor. Given a template and how many of
/// its features are already Done, return ordered next-step suggestions
/// (low-effort + foundational first). This is the engine the UI polls every 5s
/// to refresh its Claude-style next-task commentary.
pub fn next_suggestions(t: &ProjectTemplate, done_feature_names: &[String]) -> Vec<String> {
    let pending: Vec<&Feature> = t
        .features
        .iter()
        .filter(|f| !done_feature_names.iter().any(|d| d == &f.name))
        .collect();

    if pending.is_empty() {
        return vec![format!(
            "🎉 {} is feature-complete — run the chronos harness, then ship via flux serve.",
            t.name
        )];
    }

    // order: S effort first (quick wins), then M, then L
    let mut ordered = pending.clone();
    ordered.sort_by_key(|f| match f.effort {
        Effort::S => 0,
        Effort::M => 1,
        Effort::L => 2,
    });

    let done = t.features.len() - pending.len();
    let pct = (done as f64 / t.features.len() as f64) * 100.0;
    let mut out = vec![format!(
        "📊 {} — {}/{} features done ({:.0}%). Next, in leverage order:",
        t.name, done, t.features.len(), pct
    )];
    for f in ordered.iter().take(3) {
        out.push(format!(
            "→ build “{}” [{}· {:?} effort] — then wire its MCP tool + a chronos test.",
            f.name, f.category, f.effort
        ));
    }
    if done < t.features.len() {
        out.push(format!(
            "💡 after that: 2-of-N audit the change, settle the task in QUG, and update {} status.",
            t.name
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_template_has_about_50_features() {
        for t in catalog().templates {
            let n = t.feature_count();
            assert!((45..=60).contains(&n), "{} has {} features (want ~50)", t.name, n);
            assert!(!t.sigil_tasks.is_empty());
        }
    }

    #[test]
    fn ids_are_unique() {
        let cat = catalog();
        let mut ids: Vec<_> = cat.templates.iter().map(|t| t.id.clone()).collect();
        let total = ids.len();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), total, "template ids must be unique");
        assert!(total >= 8, "expect at least 8 innovative templates");
    }

    #[test]
    fn propose_cycles_and_assembles() {
        let p = propose(3);
        assert_eq!(p.len(), 3);
        assert!(p.iter().all(|t| t.feature_count() >= 45));
    }

    #[test]
    fn advisor_orders_and_completes() {
        let t = &catalog().templates[0];
        // nothing done → suggestions start with progress line + arrows
        let s = next_suggestions(t, &[]);
        assert!(s[0].contains("0/"));
        assert!(s.iter().any(|l| l.starts_with("→")));
        // all done → completion message
        let all: Vec<String> = t.features.iter().map(|f| f.name.clone()).collect();
        let done = next_suggestions(t, &all);
        assert_eq!(done.len(), 1);
        assert!(done[0].contains("feature-complete"));
    }

    #[test]
    fn platform_spine_present_in_every_template() {
        for t in catalog().templates {
            assert!(t.features.iter().any(|f| f.name.contains("QUG-native payments")));
            assert!(t.features.iter().any(|f| f.name.contains("MCP tool surface")));
        }
    }
}
