//! Storage layer — flux-db backed, one column family per entity type,
//! matching the convention already used across sigil-node/sigil-top. This
//! is the port-and-rebuild foundation everything else (SMTP/IMAP servers,
//! the MTA delivery loop, the HTTP API) will sit on top of.
//!
//! Alias routing (Viktor's explicit "custom name as email" ask) is the one
//! piece of real logic in this file, mirroring the original's
//! `DomainManager::route_email` (`.../services/domain_manager.rs:427`):
//! an incoming address is first checked against a real account's primary
//! address, then against the alias table, and only then rejected as
//! non-local. Both checks are needed — the original had this exact
//! two-step shape for a reason: a plain lookup-by-alias-only would treat
//! every account's OWN primary address as "not found."

use flux_db::Database;

use crate::models::{
    BankBroadcast, CustomDomain, EmailAlias, MailAccount, Mailbox, Message, Notification, OutboundMessage,
    OutboundStatus,
};

const CF_ACCOUNTS: &str = "mail_accounts";
const CF_ACCOUNTS_BY_ADDRESS: &str = "mail_accounts_by_address";
const CF_ALIASES: &str = "mail_aliases";
const CF_ALIASES_BY_ADDRESS: &str = "mail_aliases_by_address";
const CF_DOMAINS: &str = "mail_domains";
const CF_MAILBOXES: &str = "mail_mailboxes";
/// Secondary index: `"{wallet_id}\0{mailbox_name}"` -> mailbox id. Lets
/// `get_or_create_inbox` avoid a full scan for the common "does this
/// account already have an INBOX" check.
const CF_MAILBOXES_BY_OWNER_NAME: &str = "mail_mailboxes_by_owner_name";
const CF_MESSAGES: &str = "mail_messages";
const CF_OUTBOUND: &str = "mail_outbound";
const CF_NOTIFICATIONS: &str = "mail_notifications";
/// Secondary index: `"{wallet_id}\0{created_at as 20-digit zero-padded
/// decimal}\0{id}"` -> id, so listing a wallet's notifications newest-first
/// is a bounded range scan, not a full-table scan filtered in process (the
/// notification volume is expected to be much higher-traffic than mailbox
/// contents — wallet events, mining rewards, etc. — so this one gets a real
/// index from the start rather than the "scan + filter" shortcut used
/// elsewhere in this file for lower-volume tables).
const CF_NOTIFICATIONS_BY_WALLET: &str = "mail_notifications_by_wallet";
const CF_BANK_BROADCASTS: &str = "mail_bank_broadcasts";

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("storage error: {0}")]
    Db(String),
    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("local_part {0:?} is already taken on domain {1:?}")]
    AddressTaken(String, String),
    #[error("wallet {0} already has a mail account")]
    AccountExists(String),
    #[error("no account found for wallet {0}")]
    AccountNotFound(String),
    #[error("alias {0:?} not found")]
    AliasNotFound(String),
    #[error("mailbox {0:?} not found")]
    MailboxNotFound(String),
}

impl From<String> for StoreError {
    fn from(s: String) -> Self {
        StoreError::Db(s)
    }
}

/// The result of resolving an inbound-mail recipient address: either it
/// belongs to a real account directly, it resolves through an alias, or
/// it isn't local to this server at all (the SMTP server should reject it,
/// not silently accept mail it can't deliver anywhere).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RouteResult {
    /// The address IS a real account's primary address.
    Account(MailAccount),
    /// The address is an active alias; here's the account it resolves to.
    Alias { alias: EmailAlias, account: MailAccount },
    /// Not a local address — a real mail server must reject this outright
    /// rather than accept-then-drop it (accepting mail you can't deliver is
    /// how you become a backscatter spam source).
    NotLocal,
}

pub struct MailStore {
    db: Database,
}

impl MailStore {
    pub fn open(path: impl Into<std::path::PathBuf>) -> Result<Self, StoreError> {
        let db = Database::open(path.into()).map_err(StoreError::Db)?;
        for cf in [
            CF_ACCOUNTS,
            CF_ACCOUNTS_BY_ADDRESS,
            CF_ALIASES,
            CF_ALIASES_BY_ADDRESS,
            CF_DOMAINS,
            CF_MAILBOXES,
            CF_MAILBOXES_BY_OWNER_NAME,
            CF_MESSAGES,
            CF_OUTBOUND,
            CF_NOTIFICATIONS,
            CF_NOTIFICATIONS_BY_WALLET,
            CF_BANK_BROADCASTS,
        ] {
            if db.cf(cf).is_none() {
                db.create_cf(cf).map_err(StoreError::Db)?;
            }
        }
        Ok(Self { db })
    }

    fn cf(&self, name: &str) -> Database {
        self.db
            .cf(name)
            .unwrap_or_else(|| panic!("column family {name:?} missing — open() should have created it"))
    }

    /// Create a new account, claiming `local_part@domain` as its primary
    /// address. Fails if the wallet already has an account, or the address
    /// is already taken by a DIFFERENT wallet (either as a primary address
    /// or as someone else's alias — checked via [`Self::route_address`], so
    /// the two namespaces can never collide).
    pub fn create_account(
        &self,
        wallet_id: &str,
        local_part: &str,
        domain: &str,
        display_name: Option<String>,
        now_ms: u64,
    ) -> Result<MailAccount, StoreError> {
        let accounts = self.cf(CF_ACCOUNTS);
        if accounts.get(wallet_id.as_bytes())?.is_some() {
            return Err(StoreError::AccountExists(wallet_id.to_string()));
        }

        let address = format!("{local_part}@{domain}");
        if !matches!(self.route_address(&address)?, RouteResult::NotLocal) {
            return Err(StoreError::AddressTaken(local_part.to_string(), domain.to_string()));
        }

        let account = MailAccount {
            wallet_id: wallet_id.to_string(),
            local_part: local_part.to_string(),
            domain: domain.to_string(),
            display_name,
            created_at: now_ms,
            updated_at: now_ms,
        };
        self.put_account(&account)?;
        Ok(account)
    }

    fn put_account(&self, account: &MailAccount) -> Result<(), StoreError> {
        let bytes = serde_json::to_vec(account)?;
        self.cf(CF_ACCOUNTS).put(account.wallet_id.as_bytes(), &bytes)?;
        self.cf(CF_ACCOUNTS_BY_ADDRESS)
            .put(account.primary_address().to_lowercase().as_bytes(), account.wallet_id.as_bytes())?;
        Ok(())
    }

    pub fn get_account(&self, wallet_id: &str) -> Result<Option<MailAccount>, StoreError> {
        match self.cf(CF_ACCOUNTS).get(wallet_id.as_bytes())? {
            Some(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
            None => Ok(None),
        }
    }

    pub fn get_account_by_address(&self, address: &str) -> Result<Option<MailAccount>, StoreError> {
        match self.cf(CF_ACCOUNTS_BY_ADDRESS).get(address.to_lowercase().as_bytes())? {
            Some(wallet_id_bytes) => {
                let wallet_id = String::from_utf8_lossy(&wallet_id_bytes).into_owned();
                self.get_account(&wallet_id)
            }
            None => Ok(None),
        }
    }

    /// Register `alias@domain` as an additional address for `wallet_id`,
    /// routing to that wallet's primary address. Rejects the alias if the
    /// requested address is already claimed (as either a primary address OR
    /// someone else's alias) — the two-namespace check via
    /// [`Self::route_address`] again.
    pub fn add_alias(&self, wallet_id: &str, alias_address: &str, now_ms: u64) -> Result<EmailAlias, StoreError> {
        let account = self
            .get_account(wallet_id)?
            .ok_or_else(|| StoreError::AccountNotFound(wallet_id.to_string()))?;

        let alias_lower = alias_address.to_lowercase();
        if !matches!(self.route_address(&alias_lower)?, RouteResult::NotLocal) {
            let (local, domain) = split_address(&alias_lower).unwrap_or((alias_lower.clone(), String::new()));
            return Err(StoreError::AddressTaken(local, domain));
        }

        let alias = EmailAlias {
            id: new_id("alias", &alias_lower),
            wallet_id: wallet_id.to_string(),
            alias: alias_lower.clone(),
            destination: account.primary_address(),
            active: true,
            created_at: now_ms,
        };
        let bytes = serde_json::to_vec(&alias)?;
        self.cf(CF_ALIASES).put(alias.id.as_bytes(), &bytes)?;
        self.cf(CF_ALIASES_BY_ADDRESS).put(alias_lower.as_bytes(), alias.id.as_bytes())?;
        Ok(alias)
    }

    /// Deactivate an alias without deleting its history — mirrors the
    /// original's `active` flag rather than a hard delete, so a routing
    /// table snapshot from before deactivation stays explainable.
    pub fn deactivate_alias(&self, alias_id: &str) -> Result<(), StoreError> {
        let bytes = self
            .cf(CF_ALIASES)
            .get(alias_id.as_bytes())?
            .ok_or_else(|| StoreError::AliasNotFound(alias_id.to_string()))?;
        let mut alias: EmailAlias = serde_json::from_slice(&bytes)?;
        alias.active = false;
        let bytes = serde_json::to_vec(&alias)?;
        self.cf(CF_ALIASES).put(alias.id.as_bytes(), &bytes)?;
        Ok(())
    }

    /// The routing decision every inbound SMTP `RCPT TO` and every outbound
    /// "is this recipient local or does it need real MX delivery" check
    /// goes through. See [`RouteResult`] for what each outcome means.
    pub fn route_address(&self, address: &str) -> Result<RouteResult, StoreError> {
        let address = address.to_lowercase();

        if let Some(account) = self.get_account_by_address(&address)? {
            return Ok(RouteResult::Account(account));
        }

        if let Some(alias_id) = self.cf(CF_ALIASES_BY_ADDRESS).get(address.as_bytes())? {
            let alias_id = String::from_utf8_lossy(&alias_id).into_owned();
            if let Some(bytes) = self.cf(CF_ALIASES).get(alias_id.as_bytes())? {
                let alias: EmailAlias = serde_json::from_slice(&bytes)?;
                if alias.active {
                    if let Some(account) = self.get_account_by_address(&alias.destination)? {
                        return Ok(RouteResult::Alias { alias, account });
                    }
                }
            }
        }

        Ok(RouteResult::NotLocal)
    }

    pub fn is_local_address(&self, address: &str) -> Result<bool, StoreError> {
        Ok(!matches!(self.route_address(address)?, RouteResult::NotLocal))
    }

    pub fn list_aliases_for_wallet(&self, wallet_id: &str) -> Result<Vec<EmailAlias>, StoreError> {
        // A CF scan filtered in-process — the alias count per account is
        // small (mailbox-vanity-names, not a bulk table), so this doesn't
        // need a dedicated by-wallet index the way address routing does.
        let mut out = Vec::new();
        for (_, bytes) in self.cf(CF_ALIASES).iter_from(&[]) {
            let alias: EmailAlias = serde_json::from_slice(&bytes)?;
            if alias.wallet_id == wallet_id {
                out.push(alias);
            }
        }
        Ok(out)
    }

    pub fn register_domain(&self, domain: CustomDomain) -> Result<(), StoreError> {
        let bytes = serde_json::to_vec(&domain)?;
        self.cf(CF_DOMAINS).put(domain.domain.to_lowercase().as_bytes(), &bytes)?;
        Ok(())
    }

    pub fn get_domain(&self, domain: &str) -> Result<Option<CustomDomain>, StoreError> {
        match self.cf(CF_DOMAINS).get(domain.to_lowercase().as_bytes())? {
            Some(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
            None => Ok(None),
        }
    }

    // ── Mailboxes / messages / outbound queue ──────────────────────────

    fn owner_name_key(wallet_id: &str, name: &str) -> Vec<u8> {
        let mut k = wallet_id.as_bytes().to_vec();
        k.push(0);
        k.extend_from_slice(name.as_bytes());
        k
    }

    fn put_mailbox(&self, mailbox: &Mailbox) -> Result<(), StoreError> {
        let bytes = serde_json::to_vec(mailbox)?;
        self.cf(CF_MAILBOXES).put(mailbox.id.as_bytes(), &bytes)?;
        self.cf(CF_MAILBOXES_BY_OWNER_NAME)
            .put(&Self::owner_name_key(&mailbox.wallet_id, &mailbox.name), mailbox.id.as_bytes())?;
        Ok(())
    }

    pub fn get_mailbox(&self, wallet_id: &str, name: &str) -> Result<Option<Mailbox>, StoreError> {
        match self.cf(CF_MAILBOXES_BY_OWNER_NAME).get(&Self::owner_name_key(wallet_id, name))? {
            Some(id) => {
                let id = String::from_utf8_lossy(&id).into_owned();
                match self.cf(CF_MAILBOXES).get(id.as_bytes())? {
                    Some(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
                    None => Ok(None),
                }
            }
            None => Ok(None),
        }
    }

    /// Every account needs an INBOX to receive into — create it on first
    /// use rather than at account-creation time, so `create_account` stays
    /// a pure identity operation and doesn't need to know about mailboxes
    /// at all (the same separation of concerns the original had, per its
    /// `deliver_local_message`'s "get or create" pattern).
    pub fn get_or_create_inbox(&self, wallet_id: &str, now_ms: u64) -> Result<Mailbox, StoreError> {
        if let Some(mailbox) = self.get_mailbox(wallet_id, "INBOX")? {
            return Ok(mailbox);
        }
        let mailbox = Mailbox {
            id: new_id("mailbox", wallet_id),
            wallet_id: wallet_id.to_string(),
            name: "INBOX".to_string(),
            uid_validity: 1,
            uid_next: 1,
            created_at: now_ms,
            updated_at: now_ms,
        };
        self.put_mailbox(&mailbox)?;
        Ok(mailbox)
    }

    pub fn create_message(&self, message: &Message) -> Result<(), StoreError> {
        let bytes = serde_json::to_vec(message)?;
        self.cf(CF_MESSAGES).put(message.id.as_bytes(), &bytes)?;
        Ok(())
    }

    pub fn get_message(&self, id: &str) -> Result<Option<Message>, StoreError> {
        match self.cf(CF_MESSAGES).get(id.as_bytes())? {
            Some(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
            None => Ok(None),
        }
    }

    /// Every message in a given mailbox, newest first — an in-process scan
    /// filtered by `mailbox_id`. Fine at this stage (no per-mailbox index
    /// yet); revisit if/when a real account's mailbox grows large enough
    /// for this to matter.
    pub fn list_messages(&self, mailbox_id: &str) -> Result<Vec<Message>, StoreError> {
        let mut out = Vec::new();
        for (_, bytes) in self.cf(CF_MESSAGES).iter_from(&[]) {
            let m: Message = serde_json::from_slice(&bytes)?;
            if m.mailbox_id == mailbox_id {
                out.push(m);
            }
        }
        out.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        Ok(out)
    }

    pub fn queue_outbound(&self, outbound: &OutboundMessage) -> Result<(), StoreError> {
        let bytes = serde_json::to_vec(outbound)?;
        self.cf(CF_OUTBOUND).put(outbound.id.as_bytes(), &bytes)?;
        Ok(())
    }

    pub fn get_outbound(&self, id: &str) -> Result<Option<OutboundMessage>, StoreError> {
        match self.cf(CF_OUTBOUND).get(id.as_bytes())? {
            Some(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
            None => Ok(None),
        }
    }

    pub fn update_outbound(&self, outbound: &OutboundMessage) -> Result<(), StoreError> {
        self.queue_outbound(outbound)
    }

    /// Messages still waiting to go out — `Pending` or `Retrying` whose
    /// `next_retry` has arrived. This is what the MTA delivery loop polls;
    /// it's a plain scan (the outbound queue is bounded by real traffic
    /// volume, not something that needs a dedicated ready-time index yet).
    pub fn claim_due_outbound(&self, now_ms: u64, limit: usize) -> Result<Vec<OutboundMessage>, StoreError> {
        let mut out = Vec::new();
        for (_, bytes) in self.cf(CF_OUTBOUND).iter_from(&[]) {
            let m: OutboundMessage = serde_json::from_slice(&bytes)?;
            let ready = matches!(m.status, OutboundStatus::Pending | OutboundStatus::Retrying) && m.next_retry <= now_ms;
            if ready {
                out.push(m);
                if out.len() >= limit {
                    break;
                }
            }
        }
        Ok(out)
    }

    // ── Notifications ───────────────────────────────────────────────────

    /// `"{wallet_id}\0{created_at:020}\0{id}"` — the `020`-zero-padded
    /// timestamp keeps lexicographic byte order equal to chronological
    /// order, so a prefix-bounded ascending scan is also a time-ordered
    /// scan; the trailing id disambiguates two notifications created in
    /// the same millisecond.
    fn notification_index_key(wallet_id: &str, created_at: u64, id: &str) -> Vec<u8> {
        let mut k = wallet_id.as_bytes().to_vec();
        k.push(0);
        k.extend_from_slice(format!("{created_at:020}").as_bytes());
        k.push(0);
        k.extend_from_slice(id.as_bytes());
        k
    }

    pub fn create_notification(&self, notification: &Notification) -> Result<(), StoreError> {
        let bytes = serde_json::to_vec(notification)?;
        self.cf(CF_NOTIFICATIONS).put(notification.id.as_bytes(), &bytes)?;
        let index_key =
            Self::notification_index_key(&notification.wallet_id, notification.created_at, &notification.id);
        self.cf(CF_NOTIFICATIONS_BY_WALLET).put(&index_key, notification.id.as_bytes())?;
        Ok(())
    }

    /// Newest-first, up to `limit`. A real prefix-bounded scan (not a
    /// filter-everything scan) via [`Self::notification_index_key`] — see
    /// that constant's doc for why this table gets a real index.
    pub fn list_notifications(&self, wallet_id: &str, limit: usize) -> Result<Vec<Notification>, StoreError> {
        let prefix = {
            let mut p = wallet_id.as_bytes().to_vec();
            p.push(0);
            p
        };
        let mut ids = Vec::new();
        for (key, id_bytes) in self.cf(CF_NOTIFICATIONS_BY_WALLET).iter_from(&prefix) {
            if !key.starts_with(&prefix) {
                break; // past this wallet's range — index keys sort contiguously by wallet_id prefix
            }
            ids.push(String::from_utf8_lossy(&id_bytes).into_owned());
        }
        ids.reverse(); // index scan is oldest-first within the prefix; caller wants newest-first
        ids.truncate(limit);

        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(bytes) = self.cf(CF_NOTIFICATIONS).get(id.as_bytes())? {
                out.push(serde_json::from_slice(&bytes)?);
            }
        }
        Ok(out)
    }

    pub fn mark_notification_read(&self, id: &str) -> Result<(), StoreError> {
        if let Some(bytes) = self.cf(CF_NOTIFICATIONS).get(id.as_bytes())? {
            let mut n: Notification = serde_json::from_slice(&bytes)?;
            n.read = true;
            let bytes = serde_json::to_vec(&n)?;
            self.cf(CF_NOTIFICATIONS).put(id.as_bytes(), &bytes)?;
        }
        Ok(())
    }

    // ── Bank broadcasts ─────────────────────────────────────────────────

    pub fn create_bank_broadcast(&self, broadcast: &BankBroadcast) -> Result<(), StoreError> {
        let bytes = serde_json::to_vec(broadcast)?;
        self.cf(CF_BANK_BROADCASTS).put(broadcast.id.as_bytes(), &bytes)?;
        Ok(())
    }

    pub fn get_bank_broadcast(&self, id: &str) -> Result<Option<BankBroadcast>, StoreError> {
        match self.cf(CF_BANK_BROADCASTS).get(id.as_bytes())? {
            Some(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
            None => Ok(None),
        }
    }

    pub fn update_bank_broadcast(&self, broadcast: &BankBroadcast) -> Result<(), StoreError> {
        self.create_bank_broadcast(broadcast)
    }

    /// Every account's primary address — what a bank broadcast fans out to.
    /// A full scan of the accounts table; broadcasts are rare, deliberate,
    /// operator-authorized events, not a hot path, so this doesn't need the
    /// same indexing care as notifications.
    pub fn all_account_addresses(&self) -> Result<Vec<String>, StoreError> {
        let mut out = Vec::new();
        for (_, bytes) in self.cf(CF_ACCOUNTS).iter_from(&[]) {
            let a: MailAccount = serde_json::from_slice(&bytes)?;
            out.push(a.primary_address());
        }
        Ok(out)
    }
}

fn split_address(address: &str) -> Option<(String, String)> {
    let (local, domain) = address.split_once('@')?;
    Some((local.to_string(), domain.to_string()))
}

/// A short, content-addressed id — BLAKE3(kind || seed || a few random-ish
/// bytes from the current time) truncated to 16 hex chars. Not a UUID crate
/// dependency for the sake of one; this workspace already leans on BLAKE3
/// everywhere for exactly this "give me a short unique id" need.
fn new_id(kind: &str, seed: &str) -> String {
    let mut h = blake3::Hasher::new();
    h.update(kind.as_bytes());
    h.update(seed.as_bytes());
    h.update(&std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .to_le_bytes());
    hex::encode(&h.finalize().as_bytes()[..8])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open_test_store(name: &str) -> MailStore {
        let dir = std::env::temp_dir().join(format!("sigil-mail-store-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        MailStore::open(dir).expect("open test store")
    }

    #[test]
    fn create_account_then_look_up_by_wallet_and_by_address() {
        let store = open_test_store("account-lookup");
        let account = store
            .create_account("wallet-viktor", "viktor", "sigilgraph.org", Some("Viktor".into()), 1000)
            .expect("create account");
        assert_eq!(account.primary_address(), "viktor@sigilgraph.org");

        let by_wallet = store.get_account("wallet-viktor").unwrap().expect("found by wallet");
        assert_eq!(by_wallet, account);

        let by_address = store
            .get_account_by_address("VIKTOR@SigilGraph.org") // case-insensitivity check
            .unwrap()
            .expect("found by address");
        assert_eq!(by_address, account);
    }

    #[test]
    fn a_second_wallet_cannot_claim_the_same_primary_address() {
        let store = open_test_store("address-collision");
        store
            .create_account("wallet-a", "viktor", "sigilgraph.org", None, 1000)
            .expect("first claim succeeds");
        let err = store
            .create_account("wallet-b", "viktor", "sigilgraph.org", None, 1000)
            .expect_err("second claim of the same address must fail");
        assert!(matches!(err, StoreError::AddressTaken(_, _)));
    }

    #[test]
    fn alias_routes_to_the_owning_account_and_the_primary_address_still_works() {
        let store = open_test_store("alias-routing");
        let account = store
            .create_account("wallet-viktor", "viktor", "sigilgraph.org", None, 1000)
            .expect("create account");

        let alias = store
            .add_alias("wallet-viktor", "hello@sigilgraph.org", 2000)
            .expect("add alias");
        assert_eq!(alias.destination, "viktor@sigilgraph.org");

        match store.route_address("hello@sigilgraph.org").unwrap() {
            RouteResult::Alias { account: routed, .. } => assert_eq!(routed, account),
            other => panic!("expected Alias route, got {other:?}"),
        }
        // The primary address must STILL route directly — this is the exact
        // two-step check the doc comment calls out: alias-only lookup would
        // wrongly report an account's own primary address as not-local.
        match store.route_address("viktor@sigilgraph.org").unwrap() {
            RouteResult::Account(routed) => assert_eq!(routed, account),
            other => panic!("expected Account route, got {other:?}"),
        }
        assert!(store.is_local_address("hello@sigilgraph.org").unwrap());
        assert!(!store.is_local_address("nobody@sigilgraph.org").unwrap());
    }

    #[test]
    fn an_alias_cannot_steal_an_existing_account_or_alias_address() {
        let store = open_test_store("alias-collision");
        store
            .create_account("wallet-a", "viktor", "sigilgraph.org", None, 1000)
            .expect("create account a");
        store
            .create_account("wallet-b", "bob", "sigilgraph.org", None, 1000)
            .expect("create account b");

        // Can't alias onto another account's PRIMARY address.
        let err = store
            .add_alias("wallet-b", "viktor@sigilgraph.org", 2000)
            .expect_err("must reject aliasing over an existing account");
        assert!(matches!(err, StoreError::AddressTaken(_, _)));

        // Can't alias onto an address someone else already aliased.
        store.add_alias("wallet-a", "hello@sigilgraph.org", 2000).expect("first alias claim");
        let err = store
            .add_alias("wallet-b", "hello@sigilgraph.org", 3000)
            .expect_err("must reject aliasing over an existing alias");
        assert!(matches!(err, StoreError::AddressTaken(_, _)));
    }

    #[test]
    fn deactivated_alias_no_longer_routes() {
        let store = open_test_store("alias-deactivate");
        store
            .create_account("wallet-viktor", "viktor", "sigilgraph.org", None, 1000)
            .expect("create account");
        let alias = store.add_alias("wallet-viktor", "hello@sigilgraph.org", 2000).expect("add alias");

        store.deactivate_alias(&alias.id).expect("deactivate");
        assert!(matches!(store.route_address("hello@sigilgraph.org").unwrap(), RouteResult::NotLocal));
    }

    #[test]
    fn list_aliases_for_wallet_only_returns_that_wallets_aliases() {
        let store = open_test_store("alias-listing");
        store.create_account("wallet-a", "alice", "sigilgraph.org", None, 1000).unwrap();
        store.create_account("wallet-b", "bob", "sigilgraph.org", None, 1000).unwrap();
        store.add_alias("wallet-a", "hello@sigilgraph.org", 2000).unwrap();
        store.add_alias("wallet-a", "sales@sigilgraph.org", 2000).unwrap();
        store.add_alias("wallet-b", "support@sigilgraph.org", 2000).unwrap();

        let a_aliases = store.list_aliases_for_wallet("wallet-a").unwrap();
        assert_eq!(a_aliases.len(), 2);
        assert!(a_aliases.iter().all(|a| a.wallet_id == "wallet-a"));
    }
}
