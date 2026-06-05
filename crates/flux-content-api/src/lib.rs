//! flux-content-api — headless content delivery: model once, serve as JSON to any frontend (API-first).
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
#[derive(Clone, Serialize, Deserialize)]
pub struct Item { pub id: String, pub kind: String, pub fields: BTreeMap<String, String>, pub published: bool }
#[derive(Default, Serialize, Deserialize)]
pub struct Store { items: BTreeMap<String, Item> }
impl Store {
    pub fn new() -> Self { Self::default() }
    pub fn put(&mut self, id: &str, kind: &str, fields: BTreeMap<String, String>, published: bool) { self.items.insert(id.into(), Item { id: id.into(), kind: kind.into(), fields, published }); }
    pub fn get(&self, id: &str) -> Option<&Item> { self.items.get(id) }
    /// Query by kind; `published_only` mirrors a public delivery API.
    pub fn query(&self, kind: &str, published_only: bool) -> Vec<&Item> { self.items.values().filter(|i| i.kind == kind && (!published_only || i.published)).collect() }
    /// JSON delivery response for a query (what a headless frontend fetches).
    pub fn deliver(&self, kind: &str) -> String { let live: Vec<&Item> = self.query(kind, true); serde_json::to_string(&live).unwrap_or_else(|_| "[]".into()) }
}

/// Genesis provenance stamp for this build.
pub fn stamp() -> flux_stamp::Stamp { flux_stamp::flux_stamp!() }

#[cfg(test)]
mod tests { use super::*;
 #[test] fn put_get_query_publish() { let mut s=Store::new(); let mut f=BTreeMap::new(); f.insert("title".into(),"Hi".into()); s.put("a","article",f.clone(),true); s.put("b","article",f,false); assert_eq!(s.get("a").unwrap().fields["title"],"Hi"); assert_eq!(s.query("article",false).len(),2); assert_eq!(s.query("article",true).len(),1); assert!(s.deliver("article").contains("Hi")); }
}