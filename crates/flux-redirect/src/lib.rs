//! flux-redirect — 301 URL map (4SEO ⊕ Redirect).
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
#[derive(Default, Serialize, Deserialize)]
pub struct Redirects { map: BTreeMap<String, String> }
impl Redirects {
    pub fn new() -> Self { Self::default() }
    pub fn add(&mut self, from: &str, to: &str) { self.map.insert(from.trim_end_matches('/').into(), to.into()); }
    pub fn resolve(&self, path: &str) -> Option<&String> { self.map.get(path.trim_end_matches('/')) }
}

/// Genesis provenance stamp for this build.
pub fn stamp() -> flux_stamp::Stamp { flux_stamp::flux_stamp!() }

#[cfg(test)]
mod tests { use super::*;
 #[test] fn lookup() { let mut r = Redirects::new(); r.add("/old/","/new"); assert_eq!(r.resolve("/old").map(|s| s.as_str()), Some("/new")); assert!(r.resolve("/x").is_none()); }
}