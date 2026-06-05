//! flux-cck — structured content types, no code (K2/Seblod ⊕ Paragraphs+EntityRef).
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
#[derive(Default, Serialize, Deserialize)]
pub struct ContentType { pub name: String, pub fields: Vec<(String, String)> }
impl ContentType {
    pub fn new(name: &str) -> Self { Self { name: name.into(), fields: vec![] } }
    pub fn field(mut self, n: &str, ty: &str) -> Self { self.fields.push((n.into(), ty.into())); self }
    pub fn instantiate(&self, vals: BTreeMap<String, String>) -> Result<BTreeMap<String, String>, String> {
        for (n, _) in &self.fields { if !vals.contains_key(n) { return Err(format!("missing field {}", n)) } }
        Ok(vals)
    }
}

/// Genesis provenance stamp for this build.
pub fn stamp() -> flux_stamp::Stamp { flux_stamp::flux_stamp!() }

#[cfg(test)]
mod tests { use super::*;
 #[test] fn define_and_instantiate() { let ct = ContentType::new("article").field("title","text").field("body","richtext"); let mut v = BTreeMap::new(); v.insert("title".into(),"Hi".into()); assert!(ct.instantiate(v.clone()).is_err()); v.insert("body".into(),"...".into()); assert!(ct.instantiate(v).is_ok()); }
}