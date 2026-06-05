//! flux-groups — membership + permissions (Community Builder ⊕ Group).
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
#[derive(Default, Serialize, Deserialize)]
pub struct Groups { members: BTreeMap<String, BTreeSet<String>>, perms: BTreeMap<String, BTreeSet<String>> }
impl Groups {
    pub fn new() -> Self { Self::default() }
    pub fn join(&mut self, group: &str, user: &str) { self.members.entry(group.into()).or_default().insert(user.into()); }
    pub fn grant(&mut self, group: &str, perm: &str) { self.perms.entry(group.into()).or_default().insert(perm.into()); }
    pub fn can(&self, user: &str, perm: &str) -> bool { self.members.iter().any(|(g, us)| us.contains(user) && self.perms.get(g).map(|p| p.contains(perm)).unwrap_or(false)) }
}

/// Genesis provenance stamp for this build.
pub fn stamp() -> flux_stamp::Stamp { flux_stamp::flux_stamp!() }

#[cfg(test)]
mod tests { use super::*;
 #[test] fn permission_check() { let mut g = Groups::new(); g.join("editors","ann"); g.grant("editors","publish"); assert!(g.can("ann","publish")); assert!(!g.can("bob","publish")); assert!(!g.can("ann","delete")); }
}