//! flux-forms — universal form builder (Convert Forms ⊕ Webform): fields, required, validate.
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
#[derive(Clone, Serialize, Deserialize)]
pub struct Field { pub name: String, pub kind: String, pub required: bool }
#[derive(Default, Serialize, Deserialize)]
pub struct Form { pub fields: Vec<Field> }
impl Form {
    pub fn new() -> Self { Self::default() }
    pub fn field(mut self, name: &str, kind: &str, required: bool) -> Self { self.fields.push(Field { name: name.into(), kind: kind.into(), required }); self }
    pub fn validate(&self, input: &BTreeMap<String, String>) -> Result<(), Vec<String>> {
        let missing: Vec<String> = self.fields.iter().filter(|f| f.required && input.get(&f.name).map(|v| v.is_empty()).unwrap_or(true)).map(|f| f.name.clone()).collect();
        if missing.is_empty() { Ok(()) } else { Err(missing) }
    }
}

/// Genesis provenance stamp for this build.
pub fn stamp() -> flux_stamp::Stamp { flux_stamp::flux_stamp!() }

#[cfg(test)]
mod tests { use super::*;
 #[test] fn required_missing_fails() { let f = Form::new().field("email","email",true); let mut i = BTreeMap::new(); assert!(f.validate(&i).is_err()); i.insert("email".into(),"a@b.c".into()); assert!(f.validate(&i).is_ok()); }
}