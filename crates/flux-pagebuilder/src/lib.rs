//! flux-pagebuilder — drag-drop block/layout builder (SP PageBuilder ⊕ Layout Builder+Paragraphs).
use serde::{Deserialize, Serialize};
#[derive(Clone, Serialize, Deserialize)]
pub struct Block { pub kind: String, pub content: String }
#[derive(Default, Serialize, Deserialize)]
pub struct Page { pub blocks: Vec<Block> }
impl Page {
    pub fn new() -> Self { Self::default() }
    pub fn add(&mut self, kind: &str, content: &str) -> usize { self.blocks.push(Block { kind: kind.into(), content: content.into() }); self.blocks.len() - 1 }
    pub fn move_block(&mut self, from: usize, to: usize) -> bool { if from >= self.blocks.len() || to >= self.blocks.len() { return false } let b = self.blocks.remove(from); self.blocks.insert(to, b); true }
    pub fn render(&self) -> String { self.blocks.iter().map(|b| format!("<section data-kind=\"{}\">{}</section>", b.kind, b.content)).collect() }
}

/// Genesis provenance stamp for this build.
pub fn stamp() -> flux_stamp::Stamp { flux_stamp::flux_stamp!() }

#[cfg(test)]
mod tests { use super::*;
 #[test] fn build_and_render() { let mut p = Page::new(); p.add("hero", "Hi"); p.add("text", "Body"); assert!(p.render().contains("data-kind=\"hero\"")); assert_eq!(p.blocks.len(), 2); }
 #[test] fn reorder() { let mut p = Page::new(); p.add("a","1"); p.add("b","2"); assert!(p.move_block(1,0)); assert_eq!(p.blocks[0].kind, "b"); }
}