//! flux-commerce — catalog -> cart -> checkout -> on-chain settle (HikaShop ⊕ Drupal Commerce).
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
#[derive(Default, Serialize, Deserialize)]
pub struct Cart { pub items: BTreeMap<String, (u64, u32)> } // sku -> (unit_price, qty)
impl Cart {
    pub fn new() -> Self { Self::default() }
    pub fn add(&mut self, sku: &str, unit_price: u64, qty: u32) { let e = self.items.entry(sku.into()).or_insert((unit_price, 0)); e.0 = unit_price; e.1 += qty; }
    pub fn total(&self) -> u64 { self.items.values().map(|(p, q)| p * (*q as u64)).sum() }
}

/// Genesis provenance stamp for this build.
pub fn stamp() -> flux_stamp::Stamp { flux_stamp::flux_stamp!() }

#[cfg(test)]
mod tests { use super::*;
 #[test] fn cart_total() { let mut c = Cart::new(); c.add("A",100,2); c.add("B",50,1); c.add("A",100,1); assert_eq!(c.total(), 100*3 + 50); }
}