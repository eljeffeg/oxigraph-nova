//! In-memory QuadStore — a simple reference implementation.
//!
//! Uses a `Vec<Quad>` with linear-scan pattern matching. Fast enough for the
//! W3C conformance test suite (datasets are small). Replace with
//! `oxigraph-nova-storage-hypertrie` for production workloads.

use oxigraph_nova_core::{
    GraphName, LftjSource, NamedNode, Oxigraph, Quad, QuadStore, StoredQuad, Term,
};
use std::collections::HashSet;
use std::sync::RwLock;

pub struct MemoryStore {
    quads: RwLock<Vec<Quad>>,
    /// Named-graph IRIs explicitly registered via [`register_named_graph`].
    /// Tracks empty named graphs that have no quads (so `named_graphs()` still
    /// returns them even when the graph has zero triples).
    named_graph_iris: RwLock<HashSet<String>>,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self {
            quads: RwLock::new(Vec::new()),
            named_graph_iris: RwLock::new(HashSet::new()),
        }
    }
}

impl Default for MemoryStore {
    fn default() -> Self {
        Self::new()
    }
}

impl LftjSource for MemoryStore {}

impl QuadStore for MemoryStore {
    fn insert(&self, quad: &Quad) -> Result<bool, Oxigraph> {
        // Auto-register the named graph when a quad is inserted.
        if let GraphName::NamedNode(n) = &quad.graph_name {
            let mut set = self
                .named_graph_iris
                .write()
                .map_err(|e| Oxigraph::Storage(e.to_string()))?;
            set.insert(n.as_str().to_string());
        }
        let mut quads = self
            .quads
            .write()
            .map_err(|e| Oxigraph::Storage(e.to_string()))?;
        if quads.contains(quad) {
            return Ok(false);
        }
        quads.push(quad.clone());
        Ok(true)
    }

    fn remove(&self, quad: &Quad) -> Result<bool, Oxigraph> {
        let mut quads = self
            .quads
            .write()
            .map_err(|e| Oxigraph::Storage(e.to_string()))?;
        if let Some(pos) = quads.iter().position(|q| q == quad) {
            quads.swap_remove(pos);
            return Ok(true);
        }
        Ok(false)
    }

    fn quads_for_pattern(
        &self,
        subject: Option<&Term>,
        predicate: Option<&NamedNode>,
        object: Option<&Term>,
        graph_name: Option<&GraphName>,
    ) -> Result<Box<dyn Iterator<Item = Result<StoredQuad, Oxigraph>> + '_>, Oxigraph> {
        let quads = self
            .quads
            .read()
            .map_err(|e| Oxigraph::Storage(e.to_string()))?;

        let subject = subject.cloned();
        let predicate = predicate.cloned();
        let object = object.cloned();
        let graph_name = graph_name.cloned();

        // MemoryStore stores Quad (subject: NamedOrBlankNode); convert to Term for comparison.
        // A Term::Triple subject pattern will never match (MemoryStore can't store them),
        // but we handle it gracefully by returning empty instead of panicking.
        let results: Vec<StoredQuad> = quads
            .iter()
            .filter(|q| {
                let q_subj = Term::from(q.subject.clone());
                subject.as_ref().is_none_or(|s| q_subj == *s)
                    && predicate.as_ref().is_none_or(|p| q.predicate == *p)
                    && object.as_ref().is_none_or(|o| q.object == *o)
                    && graph_name.as_ref().is_none_or(|g| q.graph_name == *g)
            })
            .map(|q| StoredQuad::from(q.clone()))
            .collect();

        Ok(Box::new(results.into_iter().map(Ok)))
    }

    fn len(&self) -> Result<usize, Oxigraph> {
        Ok(self
            .quads
            .read()
            .map_err(|e| Oxigraph::Storage(e.to_string()))?
            .len())
    }

    fn contains(&self, quad: &Quad) -> Result<bool, Oxigraph> {
        Ok(self
            .quads
            .read()
            .map_err(|e| Oxigraph::Storage(e.to_string()))?
            .contains(quad))
    }

    fn known_named_graphs(
        &self,
    ) -> Result<Box<dyn Iterator<Item = Result<GraphName, Oxigraph>> + '_>, Oxigraph> {
        let set = self
            .named_graph_iris
            .read()
            .map_err(|e| Oxigraph::Storage(e.to_string()))?;
        let graphs: Vec<GraphName> = set
            .iter()
            .map(|iri| GraphName::NamedNode(NamedNode::new_unchecked(iri.clone())))
            .collect();
        Ok(Box::new(graphs.into_iter().map(Ok)))
    }

    /// Explicitly register a named graph IRI in this store.
    ///
    /// Necessary for empty named graphs that would otherwise be invisible to
    /// `GRAPH ?g { }` evaluation (which enumerates graphs via `known_named_graphs`).
    fn register_named_graph(&self, graph: &GraphName) -> Result<(), Oxigraph> {
        if let GraphName::NamedNode(n) = graph {
            let mut set = self
                .named_graph_iris
                .write()
                .map_err(|e| Oxigraph::Storage(e.to_string()))?;
            set.insert(n.as_str().to_string());
        }
        Ok(())
    }

    // No `apply_batch` override: `MemoryStore` has no WAL and its
    // `RwLock<Vec<Quad>>` is already cheap to (re-)acquire per call, so
    // there is no single-lock-acquisition/single-fsync win to be had from
    // batching — it deliberately relies on `QuadStore::apply_batch`'s
    // default (loop over `insert`/`remove`) rather than duplicating it here.
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxigraph_nova_core::{Literal, NamedNode, Subject};

    fn nn(s: &str) -> Subject {
        Subject::NamedNode(NamedNode::new(s).unwrap())
    }
    fn pred(s: &str) -> NamedNode {
        NamedNode::new(s).unwrap()
    }
    fn lit(s: &str) -> Term {
        Term::Literal(Literal::new_simple_literal(s))
    }
    fn default_graph() -> GraphName {
        GraphName::DefaultGraph
    }

    #[test]
    fn insert_and_len() {
        let store = MemoryStore::new();
        let quad = Quad::new(
            nn("http://ex/s"),
            pred("http://ex/p"),
            lit("hello"),
            default_graph(),
        );
        assert!(store.insert(&quad).unwrap());
        assert!(!store.insert(&quad).unwrap()); // duplicate
        assert_eq!(store.len().unwrap(), 1);
    }

    #[test]
    fn pattern_wildcard() {
        let store = MemoryStore::new();
        let p = pred("http://ex/p");
        store
            .insert(&Quad::new(
                nn("http://ex/s1"),
                p.clone(),
                lit("a"),
                default_graph(),
            ))
            .unwrap();
        store
            .insert(&Quad::new(
                nn("http://ex/s2"),
                p.clone(),
                lit("b"),
                default_graph(),
            ))
            .unwrap();
        let results: Vec<_> = store
            .quads_for_pattern(None, Some(&p), None, None)
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn remove() {
        let store = MemoryStore::new();
        let quad = Quad::new(
            nn("http://ex/s"),
            pred("http://ex/p"),
            lit("v"),
            default_graph(),
        );
        store.insert(&quad).unwrap();
        assert!(store.remove(&quad).unwrap());
        assert_eq!(store.len().unwrap(), 0);
        assert!(!store.remove(&quad).unwrap()); // already gone
    }

    #[test]
    fn register_empty_named_graph() {
        let store = MemoryStore::new();
        let g = GraphName::NamedNode(NamedNode::new_unchecked("http://ex/empty"));
        store.register_named_graph(&g).unwrap();
        let graphs: Vec<_> = store
            .known_named_graphs()
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(graphs.len(), 1);
        assert!(graphs.contains(&g));
    }
}
