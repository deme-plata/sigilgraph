//! Bitfield DAG substrate — transitive past/future sets, anticone, O(1)
//! reachability, and a deterministic Kahn linearization.
//!
//! Ported from `q-narwhalknight/crates/q-dag-knight/src/simd_sets.rs` (the one
//! genuinely sound piece of that crate — see the lane design doc §1), with:
//!
//! - `VertexId` ([u8;32]) replaced by [`sigil_header::BlockHash`];
//! - per-vertex `round` replaced by `(height, producer)` so the ready-frontier
//!   can key on the braid tie-break;
//! - **the Kahn frontier fixed**: upstream re-sorted a `Vec` queue with an
//!   inconsistent reversed comparator (simd_sets.rs:392-431 — ascending sort +
//!   pop-from-end, then a reversed re-sort mid-loop), so output depended on
//!   arrival interleaving. Here the frontier is a `BTreeSet` keyed
//!   `(height, producer, hash)` and we always pop the minimum — the sort is a
//!   pure function of the DAG contents, insertion-order invariant;
//! - `cleanup_before_round` kept as [`BitfieldDag::cleanup_below_height`], the
//!   hard sliding window (never point this structure at the whole chain:
//!   memory is O(window²/8) bytes).
//!
//! Parents must already be present when a vertex is added — unknown parent
//! hashes are silently skipped, exactly like the upstream code. The pending /
//! backfill discipline that guarantees parents-first insertion lives one layer
//! up in `Braid` (lane B).

use std::collections::{BTreeSet, HashMap, VecDeque};

use sigil_header::BlockHash;

/// Compact bitfield representing a set of vertices by index.
#[derive(Clone, Debug)]
pub struct VertexBitfield {
    bits: Vec<u64>,
    capacity: usize, // Total number of bits (rounded up to 64)
}

impl VertexBitfield {
    /// Create an empty bitfield with given capacity (in vertices).
    pub fn new(capacity: usize) -> Self {
        let words = capacity.div_ceil(64);
        Self {
            bits: vec![0u64; words],
            capacity: words * 64,
        }
    }

    /// Set bit at index.
    #[inline]
    pub fn set(&mut self, idx: u32) {
        let word = idx as usize / 64;
        let bit = idx as usize % 64;
        if word < self.bits.len() {
            self.bits[word] |= 1u64 << bit;
        }
    }

    /// Clear bit at index.
    #[inline]
    pub fn clear(&mut self, idx: u32) {
        let word = idx as usize / 64;
        let bit = idx as usize % 64;
        if word < self.bits.len() {
            self.bits[word] &= !(1u64 << bit);
        }
    }

    /// Test if bit is set.
    #[inline]
    pub fn test(&self, idx: u32) -> bool {
        let word = idx as usize / 64;
        let bit = idx as usize % 64;
        word < self.bits.len() && (self.bits[word] & (1u64 << bit)) != 0
    }

    /// Bitwise OR (union) with another bitfield.
    #[inline]
    pub fn union_with(&mut self, other: &VertexBitfield) {
        let len = self.bits.len().min(other.bits.len());
        for i in 0..len {
            self.bits[i] |= other.bits[i];
        }
    }

    /// Bitwise AND (intersection) with another bitfield.
    #[inline]
    pub fn intersect_with(&mut self, other: &VertexBitfield) {
        let len = self.bits.len().min(other.bits.len());
        for i in 0..len {
            self.bits[i] &= other.bits[i];
        }
        // Zero out words beyond other's length
        for i in len..self.bits.len() {
            self.bits[i] = 0;
        }
    }

    /// Compute anticone: `active AND NOT(past) AND NOT(future)` with the
    /// self bit cleared — the set of vertices concurrent with `self_idx`.
    pub fn anticone(
        past: &VertexBitfield,
        future: &VertexBitfield,
        self_idx: u32,
        active: &VertexBitfield,
    ) -> VertexBitfield {
        let len = past.bits.len().min(future.bits.len()).min(active.bits.len());
        let mut result = VertexBitfield::new(len * 64);

        for i in 0..len {
            result.bits[i] = active.bits[i] & !past.bits[i] & !future.bits[i];
        }

        result.clear(self_idx);
        result
    }

    /// Count set bits (popcount).
    #[inline]
    pub fn count_ones(&self) -> u32 {
        self.bits.iter().map(|w| w.count_ones()).sum()
    }

    /// Intersection count (popcount of AND).
    #[inline]
    pub fn intersection_count(&self, other: &VertexBitfield) -> u32 {
        let len = self.bits.len().min(other.bits.len());
        let mut count = 0u32;
        for i in 0..len {
            count += (self.bits[i] & other.bits[i]).count_ones();
        }
        count
    }

    /// Iterate over set bit indices, ascending.
    pub fn iter_set_bits(&self) -> impl Iterator<Item = u32> + '_ {
        self.bits.iter().enumerate().flat_map(|(word_idx, &word)| {
            let base = (word_idx * 64) as u32;
            BitIterator { word, base }
        })
    }

    /// Grow capacity to at least `new_capacity` bits.
    pub fn grow_to(&mut self, new_capacity: usize) {
        let new_words = new_capacity.div_ceil(64);
        if new_words > self.bits.len() {
            self.bits.resize(new_words, 0);
            self.capacity = new_words * 64;
        }
    }

    /// True iff no bit is set.
    pub fn is_empty(&self) -> bool {
        self.bits.iter().all(|&w| w == 0)
    }

    /// Total addressable bits (rounded up to the word size).
    pub fn capacity_bits(&self) -> usize {
        self.capacity
    }

    /// Bytes of backing storage (for stats).
    fn backing_bytes(&self) -> usize {
        self.bits.len() * 8
    }
}

/// Iterator over set bits in a single u64 word.
struct BitIterator {
    word: u64,
    base: u32,
}

impl Iterator for BitIterator {
    type Item = u32;

    #[inline]
    fn next(&mut self) -> Option<u32> {
        if self.word == 0 {
            return None;
        }
        let tz = self.word.trailing_zeros();
        self.word &= self.word - 1; // Clear lowest set bit
        Some(self.base + tz)
    }
}

/// Bidirectional mapping between [`BlockHash`] and compact u32 indices, with
/// index recycling so the sliding window keeps the index space (and therefore
/// bitfield width) bounded.
pub struct VertexIndexMap {
    vertex_to_index: HashMap<BlockHash, u32>,
    index_to_vertex: Vec<BlockHash>,
    free_list: VecDeque<u32>,
    next_index: u32,
}

impl Default for VertexIndexMap {
    fn default() -> Self {
        Self::new()
    }
}

impl VertexIndexMap {
    /// New empty map.
    pub fn new() -> Self {
        Self {
            vertex_to_index: HashMap::new(),
            index_to_vertex: Vec::new(),
            free_list: VecDeque::new(),
            next_index: 0,
        }
    }

    /// Get or assign an index for a vertex.
    pub fn get_or_insert(&mut self, vertex_id: BlockHash) -> u32 {
        if let Some(&idx) = self.vertex_to_index.get(&vertex_id) {
            return idx;
        }

        let idx = if let Some(recycled) = self.free_list.pop_front() {
            self.index_to_vertex[recycled as usize] = vertex_id;
            recycled
        } else {
            let idx = self.next_index;
            self.next_index += 1;
            if idx as usize >= self.index_to_vertex.len() {
                self.index_to_vertex.push(vertex_id);
            } else {
                self.index_to_vertex[idx as usize] = vertex_id;
            }
            idx
        };

        self.vertex_to_index.insert(vertex_id, idx);
        idx
    }

    /// Look up index for a vertex.
    pub fn get_index(&self, vertex_id: &BlockHash) -> Option<u32> {
        self.vertex_to_index.get(vertex_id).copied()
    }

    /// Look up vertex for an index.
    pub fn get_vertex(&self, idx: u32) -> Option<&BlockHash> {
        self.index_to_vertex.get(idx as usize)
    }

    /// Remove a vertex and recycle its index.
    pub fn remove(&mut self, vertex_id: &BlockHash) {
        if let Some(idx) = self.vertex_to_index.remove(vertex_id) {
            self.free_list.push_back(idx);
        }
    }

    /// Current index-space capacity (highest index ever assigned + 1).
    pub fn capacity(&self) -> usize {
        self.next_index as usize
    }

    /// Number of active vertices.
    pub fn len(&self) -> usize {
        self.vertex_to_index.len()
    }

    /// True iff no vertex is mapped.
    pub fn is_empty(&self) -> bool {
        self.vertex_to_index.is_empty()
    }

    /// All active vertex indices as a bitfield.
    pub fn active_bitfield(&self) -> VertexBitfield {
        let mut bf = VertexBitfield::new(self.capacity());
        for &idx in self.vertex_to_index.values() {
            bf.set(idx);
        }
        bf
    }
}

/// Bitfield-based DAG: transitive past/future sets per vertex, anticone,
/// O(1) `causally_precedes`, deterministic Kahn `topological_sort`.
///
/// Memory is O(window²/8) bytes — three N-bit fields per vertex. The caller
/// MUST run [`BitfieldDag::cleanup_below_height`] as its finalized height
/// advances (the hard sliding window).
pub struct BitfieldDag {
    index_map: VertexIndexMap,
    /// past_sets[i] = bitfield of vertices in i's causal past (transitively).
    past_sets: Vec<VertexBitfield>,
    /// future_sets[i] = bitfield of vertices that have i in their past.
    future_sets: Vec<VertexBitfield>,
    /// Direct parents for each vertex (for topological sort).
    parent_sets: Vec<VertexBitfield>,
    /// Height of each vertex index — first tie-break component.
    heights: Vec<u64>,
    /// Producer of each vertex index — second tie-break component.
    producers: Vec<[u8; 32]>,
}

impl Default for BitfieldDag {
    fn default() -> Self {
        Self::new()
    }
}

impl BitfieldDag {
    /// New empty DAG.
    pub fn new() -> Self {
        Self {
            index_map: VertexIndexMap::new(),
            past_sets: Vec::new(),
            future_sets: Vec::new(),
            parent_sets: Vec::new(),
            heights: Vec::new(),
            producers: Vec::new(),
        }
    }

    /// Ensure internal vectors are large enough for the given index.
    fn ensure_capacity(&mut self, idx: u32) {
        let needed = idx as usize + 1;
        let cap = self.index_map.capacity().max(needed);

        while self.past_sets.len() < needed {
            self.past_sets.push(VertexBitfield::new(cap));
        }
        while self.future_sets.len() < needed {
            self.future_sets.push(VertexBitfield::new(cap));
        }
        while self.parent_sets.len() < needed {
            self.parent_sets.push(VertexBitfield::new(cap));
        }
        while self.heights.len() < needed {
            self.heights.push(0);
        }
        while self.producers.len() < needed {
            self.producers.push([0u8; 32]);
        }

        if cap > 0 {
            for bf in self.past_sets.iter_mut() {
                bf.grow_to(cap);
            }
            for bf in self.future_sets.iter_mut() {
                bf.grow_to(cap);
            }
            for bf in self.parent_sets.iter_mut() {
                bf.grow_to(cap);
            }
        }
    }

    /// Add a vertex with its parent dependencies. Parents not yet in the DAG
    /// are skipped (the layer above must insert parents first). Returns the
    /// assigned compact index.
    pub fn add_vertex(
        &mut self,
        vertex_id: BlockHash,
        parents: &[BlockHash],
        height: u64,
        producer: [u8; 32],
    ) -> u32 {
        let idx = self.index_map.get_or_insert(vertex_id);
        self.ensure_capacity(idx);

        self.heights[idx as usize] = height;
        self.producers[idx as usize] = producer;

        // Build past set: union of all parents' past sets + parents themselves
        let cap = self.index_map.capacity();
        let mut past = VertexBitfield::new(cap);

        for parent_id in parents {
            if let Some(parent_idx) = self.index_map.get_index(parent_id) {
                self.parent_sets[idx as usize].set(parent_idx);
                past.set(parent_idx);
                if (parent_idx as usize) < self.past_sets.len() {
                    past.union_with(&self.past_sets[parent_idx as usize]);
                }
            }
        }

        self.past_sets[idx as usize] = past;

        // Update future sets: every vertex in our past gets us in their future
        for past_idx in self.past_sets[idx as usize]
            .iter_set_bits()
            .collect::<Vec<_>>()
        {
            if (past_idx as usize) < self.future_sets.len() {
                self.future_sets[past_idx as usize].set(idx);
            }
        }

        idx
    }

    /// Compute anticone of a vertex (active vertices neither in its past nor
    /// its future).
    pub fn anticone(&self, vertex_id: &BlockHash) -> Option<VertexBitfield> {
        let idx = self.index_map.get_index(vertex_id)?;
        let active = self.index_map.active_bitfield();

        if (idx as usize) >= self.past_sets.len() || (idx as usize) >= self.future_sets.len() {
            return None;
        }

        Some(VertexBitfield::anticone(
            &self.past_sets[idx as usize],
            &self.future_sets[idx as usize],
            idx,
            &active,
        ))
    }

    /// Compute anticone size.
    pub fn anticone_size(&self, vertex_id: &BlockHash) -> Option<u32> {
        self.anticone(vertex_id).map(|bf| bf.count_ones())
    }

    /// Deterministic tie-break key for a vertex index:
    /// `(height, producer, hash)` — the braid ordering key.
    #[inline]
    fn frontier_key(&self, idx: u32) -> (u64, [u8; 32], BlockHash, u32) {
        let hash = self
            .index_map
            .get_vertex(idx)
            .copied()
            .unwrap_or([0u8; 32]);
        (
            self.heights.get(idx as usize).copied().unwrap_or(0),
            self.producers.get(idx as usize).copied().unwrap_or([0u8; 32]),
            hash,
            idx,
        )
    }

    /// Deterministic Kahn topological sort of ALL active vertices.
    ///
    /// The ready-frontier is a `BTreeSet` keyed `(height, producer, hash)` and
    /// the minimum is always emitted first, so the output is a pure function
    /// of the DAG contents — identical across instances regardless of
    /// insertion order or index assignment. Parents (spine + merge) always
    /// precede children. This fixes the upstream inconsistent
    /// sort-then-reversed-re-sort frontier.
    pub fn topological_sort(&self) -> Vec<BlockHash> {
        let active_indices: Vec<u32> = self.index_map.vertex_to_index.values().copied().collect();
        if active_indices.is_empty() {
            return Vec::new();
        }
        let active = self.index_map.active_bitfield();

        // In-degree over ACTIVE parents only + explicit child adjacency so the
        // main loop is O(V log V + E) instead of O(V²).
        let mut in_degree: HashMap<u32, usize> = HashMap::with_capacity(active_indices.len());
        let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
        for &idx in &active_indices {
            let mut deg = 0usize;
            if (idx as usize) < self.parent_sets.len() {
                for parent_idx in self.parent_sets[idx as usize].iter_set_bits() {
                    if active.test(parent_idx) {
                        deg += 1;
                        children.entry(parent_idx).or_default().push(idx);
                    }
                }
            }
            in_degree.insert(idx, deg);
        }

        let mut frontier: BTreeSet<(u64, [u8; 32], BlockHash, u32)> = in_degree
            .iter()
            .filter(|(_, &deg)| deg == 0)
            .map(|(&idx, _)| self.frontier_key(idx))
            .collect();

        let mut result = Vec::with_capacity(active_indices.len());
        while let Some(key) = frontier.pop_first() {
            let (_, _, hash, idx) = key;
            result.push(hash);

            if let Some(kids) = children.get(&idx) {
                for &child in kids {
                    if let Some(deg) = in_degree.get_mut(&child) {
                        *deg = deg.saturating_sub(1);
                        if *deg == 0 {
                            frontier.insert(self.frontier_key(child));
                        }
                    }
                }
            }
        }

        // A cycle can't be built through add_vertex (parents must pre-exist),
        // so result.len() == active count always holds here.
        result
    }

    /// Check if vertex A causally precedes vertex B — O(1) bit test on B's
    /// transitive past set.
    pub fn causally_precedes(&self, a: &BlockHash, b: &BlockHash) -> bool {
        if let (Some(idx_a), Some(idx_b)) =
            (self.index_map.get_index(a), self.index_map.get_index(b))
        {
            if (idx_b as usize) < self.past_sets.len() {
                return self.past_sets[idx_b as usize].test(idx_a);
            }
        }
        false
    }

    /// True iff the vertex is in the active window.
    pub fn contains(&self, vertex_id: &BlockHash) -> bool {
        self.index_map.get_index(vertex_id).is_some()
    }

    /// Height of an active vertex.
    pub fn height_of(&self, vertex_id: &BlockHash) -> Option<u64> {
        let idx = self.index_map.get_index(vertex_id)?;
        self.heights.get(idx as usize).copied()
    }

    /// Producer of an active vertex.
    pub fn producer_of(&self, vertex_id: &BlockHash) -> Option<[u8; 32]> {
        let idx = self.index_map.get_index(vertex_id)?;
        self.producers.get(idx as usize).copied()
    }

    /// Remove vertices below the cutoff height, recycling indices — the hard
    /// sliding window. MUST run as finalization advances; never point this
    /// structure at unbounded history.
    pub fn cleanup_below_height(&mut self, cutoff_height: u64) -> usize {
        let to_remove: Vec<(BlockHash, u32)> = self
            .index_map
            .vertex_to_index
            .iter()
            .filter_map(|(&vid, &idx)| {
                if (idx as usize) < self.heights.len() && self.heights[idx as usize] < cutoff_height
                {
                    Some((vid, idx))
                } else {
                    None
                }
            })
            .collect();

        let removed_count = to_remove.len();

        for (vid, idx) in to_remove {
            // Clear this vertex's bitfield data
            if (idx as usize) < self.past_sets.len() {
                self.past_sets[idx as usize] = VertexBitfield::new(self.index_map.capacity());
            }
            if (idx as usize) < self.future_sets.len() {
                self.future_sets[idx as usize] = VertexBitfield::new(self.index_map.capacity());
            }
            if (idx as usize) < self.parent_sets.len() {
                self.parent_sets[idx as usize] = VertexBitfield::new(self.index_map.capacity());
            }

            // Clear this vertex's bit from all other past/future/parent sets
            for bf in self.past_sets.iter_mut() {
                bf.clear(idx);
            }
            for bf in self.future_sets.iter_mut() {
                bf.clear(idx);
            }
            for bf in self.parent_sets.iter_mut() {
                bf.clear(idx);
            }

            self.index_map.remove(&vid);
        }

        removed_count
    }

    /// Number of active vertices.
    pub fn vertex_count(&self) -> usize {
        self.index_map.len()
    }

    /// Structure statistics (window occupancy + memory).
    pub fn stats(&self) -> BitfieldDagStats {
        let memory_bytes = self
            .past_sets
            .iter()
            .map(VertexBitfield::backing_bytes)
            .sum::<usize>()
            + self
                .future_sets
                .iter()
                .map(VertexBitfield::backing_bytes)
                .sum::<usize>()
            + self
                .parent_sets
                .iter()
                .map(VertexBitfield::backing_bytes)
                .sum::<usize>();

        BitfieldDagStats {
            vertex_count: self.index_map.len(),
            index_capacity: self.index_map.capacity(),
            free_indices: self.index_map.free_list.len(),
            memory_bytes,
        }
    }
}

/// Occupancy + memory statistics for a [`BitfieldDag`].
#[derive(Debug, Clone)]
pub struct BitfieldDagStats {
    /// Active vertices in the window.
    pub vertex_count: usize,
    /// Index-space capacity (bitfield width, bits).
    pub index_capacity: usize,
    /// Recycled indices awaiting reuse.
    pub free_indices: usize,
    /// Bytes held by the three bitfield families.
    pub memory_bytes: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(n: u8) -> BlockHash {
        [n; 32]
    }

    const P0: [u8; 32] = [0u8; 32];

    // ── the 10 ported upstream tests (simd_sets.rs test mod, adapted) ──────

    #[test]
    fn test_bitfield_basic_ops() {
        let mut bf = VertexBitfield::new(256);
        assert!(!bf.test(42));
        bf.set(42);
        assert!(bf.test(42));
        bf.clear(42);
        assert!(!bf.test(42));
    }

    #[test]
    fn test_bitfield_union() {
        let mut a = VertexBitfield::new(128);
        let mut b = VertexBitfield::new(128);
        a.set(1);
        a.set(3);
        b.set(2);
        b.set(3);
        a.union_with(&b);
        assert!(a.test(1));
        assert!(a.test(2));
        assert!(a.test(3));
        assert_eq!(a.count_ones(), 3);
    }

    #[test]
    fn test_bitfield_anticone() {
        let mut past = VertexBitfield::new(64);
        let mut future = VertexBitfield::new(64);
        let mut active = VertexBitfield::new(64);

        for i in 0..5 {
            active.set(i);
        }

        // Vertex 2's past: {0, 1}; future: {3}
        past.set(0);
        past.set(1);
        future.set(3);

        // Anticone of vertex 2 should be {4}
        let ac = VertexBitfield::anticone(&past, &future, 2, &active);
        assert!(ac.test(4));
        assert!(!ac.test(0));
        assert!(!ac.test(1));
        assert!(!ac.test(2));
        assert!(!ac.test(3));
        assert_eq!(ac.count_ones(), 1);
    }

    #[test]
    fn test_bitfield_iter_set_bits() {
        let mut bf = VertexBitfield::new(256);
        bf.set(0);
        bf.set(63);
        bf.set(64);
        bf.set(127);
        bf.set(200);

        let bits: Vec<u32> = bf.iter_set_bits().collect();
        assert_eq!(bits, vec![0, 63, 64, 127, 200]);
    }

    #[test]
    fn test_vertex_index_map_recycle() {
        let mut map = VertexIndexMap::new();

        let idx0 = map.get_or_insert(h(0));
        let idx1 = map.get_or_insert(h(1));
        let idx2 = map.get_or_insert(h(2));
        assert_eq!(idx0, 0);
        assert_eq!(idx1, 1);
        assert_eq!(idx2, 2);

        map.remove(&h(1));
        assert_eq!(map.len(), 2);

        // New vertex should recycle index 1
        let idx3 = map.get_or_insert(h(3));
        assert_eq!(idx3, 1);
    }

    #[test]
    fn test_dag_causal_chain() {
        let mut dag = BitfieldDag::new();

        // Chain: v0 -> v1 -> v2
        dag.add_vertex(h(0), &[], 1, P0);
        dag.add_vertex(h(1), &[h(0)], 2, P0);
        dag.add_vertex(h(2), &[h(1)], 3, P0);

        assert!(dag.causally_precedes(&h(0), &h(1)));
        assert!(dag.causally_precedes(&h(0), &h(2)));
        assert!(dag.causally_precedes(&h(1), &h(2)));
        assert!(!dag.causally_precedes(&h(2), &h(0)));
    }

    #[test]
    fn test_dag_anticone() {
        let mut dag = BitfieldDag::new();

        // Diamond: v0 -> v1, v0 -> v2, v1 -> v3, v2 -> v3
        dag.add_vertex(h(0), &[], 1, P0);
        dag.add_vertex(h(1), &[h(0)], 2, P0);
        dag.add_vertex(h(2), &[h(0)], 2, P0);
        dag.add_vertex(h(3), &[h(1), h(2)], 3, P0);

        // v1 and v2 are concurrent
        let ac_v1 = dag.anticone(&h(1)).unwrap();
        assert!(ac_v1.test(dag.index_map.get_index(&h(2)).unwrap()));

        let ac_v2 = dag.anticone(&h(2)).unwrap();
        assert!(ac_v2.test(dag.index_map.get_index(&h(1)).unwrap()));

        // v0 is in v1's past, not anticone
        assert!(!ac_v1.test(dag.index_map.get_index(&h(0)).unwrap()));
    }

    #[test]
    fn test_dag_cleanup() {
        let mut dag = BitfieldDag::new();

        dag.add_vertex(h(0), &[], 1, P0);
        dag.add_vertex(h(1), &[h(0)], 2, P0);
        dag.add_vertex(h(2), &[h(1)], 3, P0);
        dag.add_vertex(h(3), &[h(2)], 4, P0);

        assert_eq!(dag.vertex_count(), 4);

        dag.cleanup_below_height(3);
        assert_eq!(dag.vertex_count(), 2); // Only heights 3, 4 remain
    }

    #[test]
    fn test_dag_topological_sort() {
        let mut dag = BitfieldDag::new();

        dag.add_vertex(h(1), &[], 1, P0);
        dag.add_vertex(h(2), &[], 1, P0);
        dag.add_vertex(h(3), &[h(1)], 2, P0);

        let sorted = dag.topological_sort();
        assert_eq!(sorted.len(), 3);

        // v1 must come before v3
        let pos_v1 = sorted.iter().position(|v| *v == h(1)).unwrap();
        let pos_v3 = sorted.iter().position(|v| *v == h(3)).unwrap();
        assert!(pos_v1 < pos_v3);
    }

    #[test]
    fn test_intersection_count() {
        let mut a = VertexBitfield::new(128);
        let mut b = VertexBitfield::new(128);

        a.set(1);
        a.set(2);
        a.set(3);
        b.set(2);
        b.set(3);
        b.set(4);

        assert_eq!(a.intersection_count(&b), 2); // {2, 3}
    }

    #[test]
    fn test_bitfield_grow() {
        let mut bf = VertexBitfield::new(64);
        bf.set(10);
        bf.grow_to(256);
        assert!(bf.test(10)); // Old data preserved
        bf.set(200);
        assert!(bf.test(200));
    }

    // ── frontier-determinism tests (the BTreeSet-frontier fix, new) ────────
    //
    // Same DAG fed to independent instances in different VALID insertion
    // orders (parents always inserted before children, as the Braid layer
    // guarantees) must produce byte-identical topological_sort output even
    // though the u32 index assignment differs per instance.

    /// (hash, parents, height, producer) — a test-DAG block spec.
    type Spec = (BlockHash, Vec<BlockHash>, u64, [u8; 32]);

    struct XorShift(u64);
    impl XorShift {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }
    }

    /// Seeded random VALID insertion order (random Kahn linear extension).
    fn shuffled_valid_order(specs: &[Spec], seed: u64) -> Vec<Spec> {
        let mut rng = XorShift(seed | 1);
        let by_hash: HashMap<BlockHash, &Spec> =
            specs.iter().map(|s| (s.0, s)).collect();
        let mut emitted: std::collections::HashSet<BlockHash> = Default::default();
        let mut remaining: Vec<&Spec> = specs.iter().collect();
        let mut out = Vec::with_capacity(specs.len());
        while !remaining.is_empty() {
            let ready: Vec<usize> = remaining
                .iter()
                .enumerate()
                .filter(|(_, s)| {
                    s.1.iter()
                        .all(|p| emitted.contains(p) || !by_hash.contains_key(p))
                })
                .map(|(i, _)| i)
                .collect();
            assert!(!ready.is_empty(), "test DAG has a cycle");
            let pick = ready[(rng.next() as usize) % ready.len()];
            let spec = remaining.remove(pick);
            emitted.insert(spec.0);
            out.push(spec.clone());
        }
        out
    }

    fn build(order: &[Spec]) -> BitfieldDag {
        let mut dag = BitfieldDag::new();
        for (hash, parents, height, producer) in order {
            dag.add_vertex(*hash, parents, *height, *producer);
        }
        dag
    }

    fn assert_parents_before_children(specs: &[Spec], sorted: &[BlockHash]) {
        let pos: HashMap<BlockHash, usize> =
            sorted.iter().enumerate().map(|(i, h)| (*h, i)).collect();
        for (hash, parents, _, _) in specs {
            for p in parents {
                if let (Some(&pp), Some(&cp)) = (pos.get(p), pos.get(hash)) {
                    assert!(pp < cp, "parent emitted after child");
                }
            }
        }
    }

    #[test]
    fn test_frontier_determinism_diamond_merge() {
        // Two-producer diamond with a merge edge: the shape live dag_mode makes.
        let pa = [0xAA; 32];
        let pb = [0xBB; 32];
        let specs: Vec<Spec> = vec![
            (h(0), vec![], 0, pa),
            (h(1), vec![h(0)], 1, pa),
            (h(2), vec![h(0)], 1, pb),
            (h(3), vec![h(1), h(2)], 2, pa), // merge block
            (h(4), vec![h(2)], 2, pb),
            (h(5), vec![h(3), h(4)], 3, pb),
        ];
        let reference = build(&specs).topological_sort();
        assert_eq!(reference.len(), specs.len());
        assert_parents_before_children(&specs, &reference);
        for seed in [1u64, 2, 3, 4, 5, 6, 7, 8] {
            let order = shuffled_valid_order(&specs, seed);
            let sorted = build(&order).topological_sort();
            assert_eq!(sorted, reference, "seed {seed} diverged");
        }
    }

    #[test]
    fn test_frontier_determinism_same_height_tiebreak() {
        // Three concurrent blocks at the same height, distinct producers:
        // frontier must emit them by (height, producer, hash) — ascending
        // producer id — regardless of insertion order.
        let specs: Vec<Spec> = vec![
            (h(9), vec![], 0, [0x00; 32]),
            (h(3), vec![h(9)], 1, [0x30; 32]),
            (h(2), vec![h(9)], 1, [0x20; 32]),
            (h(1), vec![h(9)], 1, [0x10; 32]),
        ];
        let expected = vec![h(9), h(1), h(2), h(3)];
        for seed in [11u64, 22, 33, 44] {
            let order = shuffled_valid_order(&specs, seed);
            let sorted = build(&order).topological_sort();
            assert_eq!(sorted, expected, "seed {seed} broke the tie-break");
        }
    }

    #[test]
    fn test_frontier_determinism_random_braid() {
        // Seeded 3-producer braid, 60 blocks, random cross-merges: all shuffled
        // valid insertion orders converge on one topologically-valid order.
        let producers = [[0x01; 32], [0x02; 32], [0x03; 32]];
        let mut rng = XorShift(0xD46_1701);
        let mut specs: Vec<Spec> = vec![(h(0), vec![], 0, producers[0])];
        let mut tips: Vec<BlockHash> = vec![h(0)];
        for n in 1..60u8 {
            let producer = producers[(rng.next() as usize) % 3];
            let spine = tips[(rng.next() as usize) % tips.len()];
            let spine_height = specs.iter().find(|s| s.0 == spine).unwrap().2;
            let mut parents = vec![spine];
            // Random extra merge parent from the existing DAG (dedup'd).
            if rng.next() % 3 == 0 {
                let mp = specs[(rng.next() as usize) % specs.len()].0;
                if mp != spine {
                    parents.push(mp);
                }
            }
            let hash = h(n);
            specs.push((hash, parents.clone(), spine_height + 1, producer));
            tips.retain(|t| !parents.contains(t));
            tips.push(hash);
        }
        let reference = build(&specs).topological_sort();
        assert_eq!(reference.len(), specs.len());
        assert_parents_before_children(&specs, &reference);
        for seed in [7u64, 77, 777, 7777, 77777, 777777] {
            let order = shuffled_valid_order(&specs, seed);
            let sorted = build(&order).topological_sort();
            assert_eq!(sorted, reference, "seed {seed} diverged");
        }
    }

    #[test]
    fn test_frontier_determinism_survives_cleanup_and_recycling() {
        // Index recycling (cleanup + re-insert) must not perturb the order:
        // the frontier keys on (height, producer, hash), never on raw indices.
        let build_windowed = |orders_seed: u64| -> Vec<BlockHash> {
            let mut dag = BitfieldDag::new();
            // Prefix chain that gets finalized away.
            dag.add_vertex(h(100), &[], 0, P0);
            dag.add_vertex(h(101), &[h(100)], 1, P0);
            dag.add_vertex(h(102), &[h(101)], 2, P0);
            dag.cleanup_below_height(3); // recycles 3 indices
            // Window DAG inserted in a shuffled valid order.
            let specs: Vec<Spec> = vec![
                (h(10), vec![], 3, [0x01; 32]),
                (h(11), vec![h(10)], 4, [0x01; 32]),
                (h(12), vec![h(10)], 4, [0x02; 32]),
                (h(13), vec![h(11), h(12)], 5, [0x02; 32]),
            ];
            for (hash, parents, height, producer) in shuffled_valid_order(&specs, orders_seed) {
                dag.add_vertex(hash, &parents, height, producer);
            }
            dag.topological_sort()
        };
        let reference = build_windowed(1);
        assert_eq!(reference.len(), 4);
        for seed in [2u64, 3, 4, 5] {
            assert_eq!(build_windowed(seed), reference, "seed {seed} diverged");
        }
    }
}
