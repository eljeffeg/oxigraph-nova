//! [`StoreAtomSource`] — an [`AtomSource`] adapter over
//! `oxigraph_nova_core::LftjSource`, so rule-body atoms can scan a real
//! store's compacted LOUDS tries directly instead of first copying base
//! facts into a `Vec` (which is what the original single-rule spike test
//! did).
//!
//! This mirrors `oxigraph-nova-query`'s `StoreDataset<S: QuadStore>` adapter
//! (`crates/nova-query/src/dataset.rs`) — same idea (thin wrapper resolving
//! a graph selector down to a raw `u8` graph id, then delegating scans to
//! the store) — but keyed on a single, already-resolved `graph_id: u8`
//! rather than a `GraphSelector`, since `nova-reasoning` deliberately has no
//! dependency on `nova-query`'s SPARQL-facing types.

use crate::join::AtomSource;
use oxigraph_nova_core::{EmptyTrieIter, LftjSource, TrieIterator};

/// Scans one graph of a real [`LftjSource`]-backed store as an
/// [`AtomSource`] — the "Total"/EDB side of a fixpoint round, backed by the
/// stable, compacted LOUDS index rather than an in-memory copy.
///
/// **Precondition (caller's responsibility):** the store must have no
/// pending LSM delta for the scanned graph when this is used inside a
/// fixpoint round (see `sorted_vec_trie`'s module docs) — i.e. call
/// `store.compact()` before running the fixpoint driver. `StoreAtomSource`
/// itself does not check `lftj_has_delta()`; it simply calls
/// `lftj_join_scan` as-is, exactly like `nova-query`'s LFTJ evaluator does.
pub struct StoreAtomSource<'a, S: LftjSource> {
    store: &'a S,
    graph_id: u8,
}

impl<'a, S: LftjSource> StoreAtomSource<'a, S> {
    pub fn new(store: &'a S, graph_id: u8) -> Self {
        Self { store, graph_id }
    }
}

impl<S: LftjSource> AtomSource for StoreAtomSource<'_, S> {
    fn scan(
        &self,
        s: Option<u64>,
        p: Option<u64>,
        o: Option<u64>,
        target_field: usize,
    ) -> Box<dyn TrieIterator> {
        self.store
            .lftj_join_scan(s, p, o, target_field, self.graph_id)
            .unwrap_or_else(|| Box::new(EmptyTrieIter))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxigraph_nova_core::{GraphName, NamedNode, Quad, QuadStore, Term};
    use oxigraph_nova_storage_ring::LoudsStore;

    fn nn(s: &str) -> NamedNode {
        NamedNode::new(s).unwrap()
    }

    #[test]
    fn scans_compacted_ringstore_graph() {
        let store = LoudsStore::new();
        store
            .insert(&Quad::new(
                nn("http://example.org/a"),
                nn("http://example.org/p"),
                Term::NamedNode(nn("http://example.org/b")),
                GraphName::DefaultGraph,
            ))
            .unwrap();
        store.compact().unwrap();

        let p_id = store
            .lftj_intern_term(&Term::NamedNode(nn("http://example.org/p")))
            .unwrap();
        let a_id = store
            .lftj_intern_term(&Term::NamedNode(nn("http://example.org/a")))
            .unwrap();
        let b_id = store
            .lftj_intern_term(&Term::NamedNode(nn("http://example.org/b")))
            .unwrap();

        let default_graph_id = store.lftj_graph_id(&GraphName::DefaultGraph).unwrap();
        let src = StoreAtomSource::new(&store, default_graph_id);

        // subject bound, predicate bound, target = object.
        let mut scan = src.scan(Some(a_id), Some(p_id), None, 2);
        assert_eq!(scan.key(), b_id);
        scan.advance();
        assert!(scan.at_end());
    }

    #[test]
    fn scans_empty_graph_as_exhausted() {
        let store = LoudsStore::new();
        store.compact().unwrap();
        // No graph registered yet — lftj_graph_id would return None for an
        // unregistered graph, so exercise the "graph exists but has no Ring
        // entry" path via the default graph instead (registered implicitly
        // by LoudsStore::new(), always compacted-empty here).
        let default_graph_id = store.lftj_graph_id(&GraphName::DefaultGraph).unwrap();
        let src = StoreAtomSource::new(&store, default_graph_id);
        let scan = src.scan(None, None, None, 0);
        assert!(scan.at_end());
    }
}
