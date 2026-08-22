//! merkle.rs — a real, tested Merkle tree over pre-hashed leaves, used for
//! `BatchHeaderV1::tx_root` (canonical.rs). RFC6962-style domain separation
//! between leaf and internal-node hashing (`0x00`/`0x01` prefix) so a leaf
//! hash can never be replayed as an internal-node hash or vice versa — the
//! classic Merkle second-preimage attack this construction avoids.

const LEAF_PREFIX: u8 = 0x00;
const NODE_PREFIX: u8 = 0x01;

fn leaf_hash(data: &[u8; 32]) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(&[LEAF_PREFIX]);
    h.update(data);
    h.finalize().into()
}

fn node_hash(left: &[u8; 32], right: &[u8; 32]) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(&[NODE_PREFIX]);
    h.update(left);
    h.update(right);
    h.finalize().into()
}

/// Merkle root over already-canonical 32-byte leaf values (e.g. per-tx
/// digests). Empty input returns an all-zero root — a sealed batch is never
/// actually empty in practice, but the function stays total rather than
/// panicking. Odd node counts promote the unpaired node UNCHANGED to the next
/// level (not duplicated) — duplicate-leaf padding is a known way to
/// accidentally make two different-length leaf sets produce the same root;
/// promoting avoids that without needing a leaf-count commitment.
pub fn merkle_root(leaves: &[[u8; 32]]) -> [u8; 32] {
    if leaves.is_empty() {
        return [0u8; 32];
    }
    let mut level: Vec<[u8; 32]> = leaves.iter().map(leaf_hash).collect();
    while level.len() > 1 {
        let mut next = Vec::with_capacity(level.len().div_ceil(2));
        let mut i = 0;
        while i < level.len() {
            if i + 1 < level.len() {
                next.push(node_hash(&level[i], &level[i + 1]));
            } else {
                next.push(level[i]);
            }
            i += 2;
        }
        level = next;
    }
    level[0]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_root_is_zero() {
        assert_eq!(merkle_root(&[]), [0u8; 32]);
    }

    #[test]
    fn single_leaf_root_is_leaf_hash_not_the_raw_leaf() {
        let leaf = [7u8; 32];
        let root = merkle_root(&[leaf]);
        assert_ne!(root, leaf, "the root must be domain-separated, not the bare leaf value");
        assert_eq!(root, leaf_hash(&leaf));
    }

    #[test]
    fn order_matters() {
        let a = [1u8; 32];
        let b = [2u8; 32];
        assert_ne!(
            merkle_root(&[a, b]),
            merkle_root(&[b, a]),
            "leaf order must affect the root — otherwise reordering txs would be undetectable"
        );
    }

    #[test]
    fn deterministic_across_calls() {
        let leaves: Vec<[u8; 32]> = (0..7u8).map(|i| [i; 32]).collect();
        assert_eq!(merkle_root(&leaves), merkle_root(&leaves));
    }

    #[test]
    fn different_leaf_counts_produce_different_roots() {
        let leaves: Vec<[u8; 32]> = (0..5u8).map(|i| [i; 32]).collect();
        let root5 = merkle_root(&leaves);
        let root4 = merkle_root(&leaves[..4]);
        assert_ne!(root5, root4);
    }

    /// A leaf value can never collide with an internal-node hash: leaf_hash
    /// always starts from a 0x00-prefixed message, node_hash from 0x01. If
    /// this test ever caught a collision it would mean the domain separation
    /// itself was broken (e.g. someone removed the prefix bytes), not just an
    /// unlucky hash collision.
    #[test]
    fn leaf_and_node_domains_are_separated() {
        let a = [3u8; 32];
        let b = [4u8; 32];
        let lh = leaf_hash(&a);
        let nh = node_hash(&a, &b);
        assert_ne!(lh, nh);
    }

    /// Golden vector: BLAKE3-based, so pinned to catch any accidental change
    /// to the hash function, prefixes, or pairing order. If this ever needs
    /// to change, it means the encoding genuinely changed — treat that as a
    /// breaking/versioned change (bump BATCH_HEADER_VERSION in canonical.rs),
    /// not a test to quietly update. Value computed once by this exact
    /// implementation and pinned below.
    #[test]
    fn golden_three_leaf_root() {
        let leaves = [[0xAAu8; 32], [0xBBu8; 32], [0xCCu8; 32]];
        let root = merkle_root(&leaves);
        let hex: String = root.iter().map(|b| format!("{b:02x}")).collect();
        const GOLDEN_HEX: &str = "1a612f9d9ebfbfb897111c2f461f1fe614b90eff1474d464af8116b2f553d293";
        assert_eq!(hex, GOLDEN_HEX);
    }
}
