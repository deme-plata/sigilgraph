//! flux-i18n — per-entity translation (Falang ⊕ Content Translation).
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
#[derive(Default, Serialize, Deserialize)]
pub struct I18n { pub by_locale: BTreeMap<String, BTreeMap<String, String>> }
impl I18n {
    pub fn new() -> Self { Self::default() }
    pub fn set(&mut self, locale: &str, key: &str, val: &str) { self.by_locale.entry(locale.into()).or_default().insert(key.into(), val.into()); }
    pub fn t(&self, locale: &str, key: &str) -> String { self.by_locale.get(locale).and_then(|m| m.get(key)).cloned().unwrap_or_else(|| key.to_string()) }
}

/// Genesis provenance stamp for this build.
pub fn stamp() -> flux_stamp::Stamp { flux_stamp::flux_stamp!() }

#[cfg(test)]
mod tests { use super::*;
 #[test] fn translate_with_fallback() { let mut i = I18n::new(); i.set("da","hello","hej"); assert_eq!(i.t("da","hello"),"hej"); assert_eq!(i.t("fr","hello"),"hello"); }
}