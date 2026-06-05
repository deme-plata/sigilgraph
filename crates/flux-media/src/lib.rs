//! flux-media — DAM + responsive variants (JoomUnited ⊕ Media Library).
use serde::{Deserialize, Serialize};
#[derive(Clone, Serialize, Deserialize)]
pub struct Variant { pub w: u32, pub url: String }
#[derive(Serialize, Deserialize)]
pub struct MediaItem { pub id: String, pub mime: String, pub variants: Vec<Variant> }
impl MediaItem {
    pub fn new(id: &str, mime: &str) -> Self { Self { id: id.into(), mime: mime.into(), variants: vec![] } }
    pub fn variant(mut self, w: u32, url: &str) -> Self { self.variants.push(Variant { w, url: url.into() }); self.variants.sort_by_key(|v| v.w); self }
    pub fn srcset(&self) -> String { self.variants.iter().map(|v| format!("{} {}w", v.url, v.w)).collect::<Vec<_>>().join(", ") }
}

/// Genesis provenance stamp for this build.
pub fn stamp() -> flux_stamp::Stamp { flux_stamp::flux_stamp!() }

#[cfg(test)]
mod tests { use super::*;
 #[test] fn srcset() { let m = MediaItem::new("img1","image/webp").variant(800,"/a.webp").variant(400,"/b.webp"); assert_eq!(m.srcset(), "/b.webp 400w, /a.webp 800w"); }
}