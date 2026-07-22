//! [`RocksDbStore`] — Nova [`QuadStore`] / [`StorageEngine`] over Oxigraph RocksDB.

use oxigraph::store::Store as OxStore;
use oxigraph_nova_core::{
    GraphName, LftjSource, NamedNode, NamedOrBlankNode, Oxigraph, Quad, QuadOp, QuadStore,
    StorageEngine, StoredQuad, SyncPolicy, Term,
};
use std::path::Path;
use std::sync::Arc;
use tempfile::TempDir;

/// Oxigraph-compatible RocksDB-backed store.
///
/// Thin adapter over [`oxigraph::store::Store`]. Persistent instances opened
/// with [`RocksDbStore::open`] share the on-disk format with stock Oxigraph.
///
/// In-memory instances ([`RocksDbStore::new`]) use Oxigraph's memory engine
/// when no path is needed; registry `new_in_memory` uses a temp-dir RocksDB
/// so backup/flush APIs remain meaningful in tests.
pub struct RocksDbStore {
    inner: OxStore,
    /// When set, this store owns a temporary directory (registry in-memory path).
    /// Kept alive for the lifetime of the store so RocksDB files remain valid.
    _tempdir: Option<TempDir>,
    /// True when backed by an on-disk RocksDB directory (open path or tempdir).
    persistent: bool,
}

impl RocksDbStore {
    /// In-process Oxigraph memory store (not RocksDB). Useful for unit tests
    /// that do not need on-disk format fidelity.
    pub fn new() -> Result<Self, Oxigraph> {
        let inner = OxStore::new().map_err(map_err)?;
        Ok(Self {
            inner,
            _tempdir: None,
            persistent: false,
        })
    }

    /// Open or create an Oxigraph RocksDB directory at `path`.
    ///
    /// Accepts directories previously written by Oxigraph `Store::open`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, Oxigraph> {
        let path = path.as_ref();
        // Refuse obvious Nova LOUDS/Ring layouts so users get a clear error.
        if path.join("MANIFEST").is_file() && !looks_like_rocksdb(path) {
            return Err(Oxigraph::Storage(format!(
                "path {} looks like a Nova LOUDS/Ring store (found MANIFEST without RocksDB markers); \
                 use --backend louds or --backend ring",
                path.display()
            )));
        }
        let inner = OxStore::open(path).map_err(map_err)?;
        Ok(Self {
            inner,
            _tempdir: None,
            persistent: true,
        })
    }

    /// Ephemeral on-disk RocksDB in a temp directory (deleted on drop).
    pub fn new_temp() -> Result<Self, Oxigraph> {
        let tempdir = TempDir::new().map_err(|e| Oxigraph::Storage(e.to_string()))?;
        let inner = OxStore::open(tempdir.path()).map_err(map_err)?;
        Ok(Self {
            inner,
            _tempdir: Some(tempdir),
            persistent: true,
        })
    }

    /// Access the underlying Oxigraph store (advanced / tests).
    pub fn as_oxigraph(&self) -> &OxStore {
        &self.inner
    }
}

impl Default for RocksDbStore {
    fn default() -> Self {
        Self::new().expect("Oxigraph in-memory Store::new")
    }
}

fn looks_like_rocksdb(path: &Path) -> bool {
    // RocksDB writes CURRENT; Oxigraph also uses standard SST layout.
    path.join("CURRENT").is_file()
        || path.join("IDENTITY").is_file()
        || path
            .read_dir()
            .map(|mut d| {
                d.any(|e| {
                    e.map(|e| {
                        let n = e.file_name();
                        let s = n.to_string_lossy();
                        s.starts_with("MANIFEST-") || s.ends_with(".sst") || s == "OPTIONS-000007"
                    })
                    .unwrap_or(false)
                })
            })
            .unwrap_or(false)
}

fn map_err(e: impl std::fmt::Display) -> Oxigraph {
    Oxigraph::Storage(e.to_string())
}

/// Convert a pattern subject `Term` into owned `NamedOrBlankNode` for pattern
/// scans. Quoted-triple subjects cannot be expressed on Oxigraph's subject
/// type in oxrdf 0.3 — return `None` so the scan yields empty.
fn subject_as_named_or_blank(term: &Term) -> Option<NamedOrBlankNode> {
    match term {
        Term::NamedNode(n) => Some(NamedOrBlankNode::NamedNode(n.clone())),
        Term::BlankNode(b) => Some(NamedOrBlankNode::BlankNode(b.clone())),
        #[allow(unreachable_patterns)]
        _ => None,
    }
}

impl LftjSource for RocksDbStore {
    // Defaults: no LFTJ — Nova evaluator uses nested pattern scans via
    // quads_for_pattern (Oxigraph multi-index iterators underneath).
}

impl QuadStore for RocksDbStore {
    fn insert(&self, quad: &Quad) -> Result<bool, Oxigraph> {
        let existed = self.inner.contains(quad).map_err(map_err)?;
        if existed {
            return Ok(false);
        }
        // Oxigraph 0.5 takes `impl Into<QuadRef<'_>>` — pass `&Quad`.
        self.inner.insert(quad).map_err(map_err)?;
        Ok(true)
    }

    fn remove(&self, quad: &Quad) -> Result<bool, Oxigraph> {
        let existed = self.inner.contains(quad).map_err(map_err)?;
        if !existed {
            return Ok(false);
        }
        self.inner.remove(quad).map_err(map_err)?;
        Ok(true)
    }

    fn quads_for_pattern(
        &self,
        subject: Option<&Term>,
        predicate: Option<&NamedNode>,
        object: Option<&Term>,
        graph_name: Option<&GraphName>,
    ) -> Result<Box<dyn Iterator<Item = Result<StoredQuad, Oxigraph>> + '_>, Oxigraph> {
        // Quoted-triple subject pattern: Oxigraph's API cannot express it → empty.
        // Keep owned NamedOrBlankNode so `.as_ref()` can produce NamedOrBlankNodeRef.
        let subject_nb = match subject {
            None => None,
            Some(t) => match subject_as_named_or_blank(t) {
                Some(nb) => Some(nb),
                None => {
                    return Ok(Box::new(std::iter::empty()));
                }
            },
        };

        // Oxigraph 0.5 takes owned Option<*Ref<'_>> (not Option<&T>).
        let iter = self.inner.quads_for_pattern(
            subject_nb.as_ref().map(|s| s.as_ref()),
            predicate.map(|p| p.as_ref()),
            object.map(|o| o.as_ref()),
            graph_name.map(|g| g.as_ref()),
        );

        // Collect eagerly: Oxigraph's QuadIter borrows the snapshot; boxing a
        // streaming iterator with a complex lifetime is awkward behind dyn.
        // Pattern scans for SPARQL BGP are typically selective; bulk dumps
        // use extend/bulk_load paths separately.
        let mut out = Vec::new();
        for item in iter {
            let q = item.map_err(map_err)?;
            out.push(Ok(StoredQuad::from(q)));
        }
        Ok(Box::new(out.into_iter()))
    }

    fn len(&self) -> Result<usize, Oxigraph> {
        self.inner.len().map_err(map_err)
    }

    fn is_empty(&self) -> Result<bool, Oxigraph> {
        self.inner.is_empty().map_err(map_err)
    }

    fn contains(&self, quad: &Quad) -> Result<bool, Oxigraph> {
        self.inner.contains(quad).map_err(map_err)
    }

    fn known_named_graphs(
        &self,
    ) -> Result<Box<dyn Iterator<Item = Result<GraphName, Oxigraph>> + '_>, Oxigraph> {
        let mut graphs = Vec::new();
        for g in self.inner.named_graphs() {
            let name = g.map_err(map_err)?;
            // NamedOrBlankNode → GraphName
            let gn = match name {
                NamedOrBlankNode::NamedNode(n) => GraphName::NamedNode(n),
                NamedOrBlankNode::BlankNode(b) => GraphName::BlankNode(b),
            };
            graphs.push(Ok(gn));
        }
        Ok(Box::new(graphs.into_iter()))
    }

    fn register_named_graph(&self, graph: &GraphName) -> Result<(), Oxigraph> {
        match graph {
            GraphName::DefaultGraph => Ok(()),
            // NamedNode / BlankNode implement Into<NamedOrBlankNodeRef> via as_ref.
            GraphName::NamedNode(n) => self.inner.insert_named_graph(n).map_err(map_err),
            GraphName::BlankNode(b) => self.inner.insert_named_graph(b).map_err(map_err),
            #[allow(unreachable_patterns)]
            _ => Ok(()),
        }
    }

    fn extend_boxed(&self, quads: Box<dyn Iterator<Item = Quad> + '_>) -> Result<usize, Oxigraph> {
        // Collect then single transaction via extend for atomicity + fewer commits.
        let batch: Vec<Quad> = quads.collect();
        if batch.is_empty() {
            return Ok(0);
        }
        // Count how many are new (best-effort; race-free under single writer).
        let mut inserted = 0usize;
        for q in &batch {
            if !self.inner.contains(q).map_err(map_err)? {
                inserted += 1;
            }
        }
        self.inner.extend(batch).map_err(map_err)?;
        Ok(inserted)
    }

    fn apply_batch(&self, ops: &[QuadOp]) -> Result<(usize, usize), Oxigraph> {
        // Oxigraph transactions keep all changes in memory; use one transaction
        // when available for atomic multi-op SPARQL Update.
        let mut transaction = self.inner.start_transaction().map_err(map_err)?;
        let mut inserted = 0usize;
        let mut removed = 0usize;
        for op in ops {
            match op {
                QuadOp::Insert(q) => {
                    let existed = transaction.contains(q).map_err(map_err)?;
                    if !existed {
                        inserted += 1;
                    }
                    transaction.insert(q);
                }
                QuadOp::Remove(q) => {
                    let existed = transaction.contains(q).map_err(map_err)?;
                    if existed {
                        removed += 1;
                    }
                    transaction.remove(q);
                }
            }
        }
        transaction.commit().map_err(map_err)?;
        Ok((inserted, removed))
    }
}

impl StorageEngine for RocksDbStore {
    fn engine_name(&self) -> &'static str {
        "rocksdb"
    }

    fn compact(&self) -> Result<(), Oxigraph> {
        if self.persistent {
            self.inner.optimize().map_err(map_err)
        } else {
            Ok(())
        }
    }

    fn backup(&self, destination: &Path) -> Result<(), Oxigraph> {
        if !self.persistent {
            return Err(Oxigraph::Storage(
                "backup is only supported for on-disk RocksDB stores (open with a path)".into(),
            ));
        }
        self.inner.backup(destination).map_err(map_err)
    }

    fn set_sync_policy(&self, _policy: SyncPolicy) {
        // Oxigraph RocksDB manages WAL durability internally; no public knob
        // matching Nova's SyncPolicy. No-op by design (documented).
    }

    fn set_auto_compact_threshold(&self, _threshold: usize) {
        // RocksDB has its own compaction triggers; Nova's delta threshold N/A.
    }

    fn flush_wal(&self) -> Result<(), Oxigraph> {
        if self.persistent {
            self.inner.flush().map_err(map_err)
        } else {
            Ok(())
        }
    }

    fn bulk_load_boxed(
        &self,
        quads: Box<dyn Iterator<Item = Quad> + '_>,
        mut on_progress: Option<&mut dyn FnMut(u64)>,
    ) -> Result<usize, Oxigraph> {
        let mut loader = self.inner.bulk_loader().without_atomicity();
        let mut count = 0u64;
        let mut batch = Vec::with_capacity(64_000);
        for q in quads {
            batch.push(q);
            count += 1;
            if batch.len() >= 64_000 {
                loader.load_quads(batch.drain(..)).map_err(map_err)?;
                if let Some(cb) = on_progress.as_mut() {
                    cb(count);
                }
            }
        }
        if !batch.is_empty() {
            loader.load_quads(batch).map_err(map_err)?;
            if let Some(cb) = on_progress.as_mut() {
                cb(count);
            }
        }

        loader.commit().map_err(map_err)?;
        Ok(count as usize)
    }
}

// ── Backend registry ──────────────────────────────────────────────────────────

fn rocks_new_in_memory() -> Arc<dyn StorageEngine> {
    // Tempdir RocksDB so open/backup paths match production semantics in tests.
    match RocksDbStore::new_temp() {
        Ok(s) => Arc::new(s),
        Err(e) => {
            // Fall back to pure memory if tempdir fails (extremely rare).
            tracing::warn!("rocksdb new_temp failed ({e}); falling back to Oxigraph memory store");
            Arc::new(RocksDbStore::new().expect("Oxigraph Store::new"))
        }
    }
}

fn rocks_open(path: &Path) -> Result<Arc<dyn StorageEngine>, Oxigraph> {
    Ok(Arc::new(RocksDbStore::open(path)?))
}

inventory::submit! {
    oxigraph_nova_core::BackendFactory {
        name: "rocksdb",
        description: "Oxigraph-compatible RocksDB (drop-in data directory)",
        new_in_memory: rocks_new_in_memory,
        open: rocks_open,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxigraph_nova_core::{Literal, NamedOrBlankNode, QuadStoreExt};
    use oxrdf::NamedNode;

    fn nn(s: &str) -> NamedOrBlankNode {
        NamedOrBlankNode::NamedNode(NamedNode::new(s).unwrap())
    }
    fn pred(s: &str) -> NamedNode {
        NamedNode::new(s).unwrap()
    }
    fn lit(s: &str) -> Term {
        Term::Literal(Literal::new_simple_literal(s))
    }

    #[test]
    fn memory_insert_contains_pattern() {
        let store = RocksDbStore::new().unwrap();
        let q = Quad::new(
            nn("http://ex/s"),
            pred("http://ex/p"),
            lit("hello"),
            GraphName::DefaultGraph,
        );
        assert!(store.insert(&q).unwrap());
        assert!(!store.insert(&q).unwrap());
        assert!(store.contains(&q).unwrap());
        assert_eq!(store.len().unwrap(), 1);

        let p = pred("http://ex/p");
        let results: Vec<_> = store
            .quads_for_pattern(None, Some(&p), None, None)
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn temp_disk_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("oxdata");
        {
            let store = RocksDbStore::open(&path).unwrap();
            let q = Quad::new(
                nn("http://ex/s"),
                pred("http://ex/p"),
                lit("disk"),
                GraphName::DefaultGraph,
            );
            store.insert(&q).unwrap();
            store.flush_wal().unwrap();
        }
        let store2 = RocksDbStore::open(&path).unwrap();
        assert_eq!(store2.len().unwrap(), 1);
        let q = Quad::new(
            nn("http://ex/s"),
            pred("http://ex/p"),
            lit("disk"),
            GraphName::DefaultGraph,
        );
        assert!(store2.contains(&q).unwrap());
    }

    #[test]
    fn named_graph_registration() {
        let store = RocksDbStore::new().unwrap();
        let g = GraphName::NamedNode(NamedNode::new("http://ex/g").unwrap());
        store.register_named_graph(&g).unwrap();
        let graphs: Vec<_> = store
            .known_named_graphs()
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert!(graphs.contains(&g));
    }

    #[test]
    fn apply_batch_insert_remove() {
        let store = RocksDbStore::new().unwrap();
        let q1 = Quad::new(
            nn("http://ex/s1"),
            pred("http://ex/p"),
            lit("a"),
            GraphName::DefaultGraph,
        );
        let q2 = Quad::new(
            nn("http://ex/s2"),
            pred("http://ex/p"),
            lit("b"),
            GraphName::DefaultGraph,
        );
        let (ins, rem) = store
            .apply_batch(&[QuadOp::Insert(q1.clone()), QuadOp::Insert(q2.clone())])
            .unwrap();
        assert_eq!(ins, 2);
        assert_eq!(rem, 0);
        assert_eq!(store.len().unwrap(), 2);

        let (ins2, rem2) = store.apply_batch(&[QuadOp::Remove(q1)]).unwrap();
        assert_eq!(ins2, 0);
        assert_eq!(rem2, 1);
        assert_eq!(store.len().unwrap(), 1);
        assert!(store.contains(&q2).unwrap());
    }

    #[test]
    fn extend_and_bulk_load() {
        let store = RocksDbStore::new().unwrap();
        let quads: Vec<Quad> = (0..100)
            .map(|i| {
                Quad::new(
                    nn(&format!("http://ex/s{i}")),
                    pred("http://ex/p"),
                    lit(&format!("v{i}")),
                    GraphName::DefaultGraph,
                )
            })
            .collect();
        let n = store.extend(quads.clone()).unwrap();
        assert_eq!(n, 100);
        assert_eq!(store.len().unwrap(), 100);

        let store2 = RocksDbStore::new().unwrap();
        let n2 = store2
            .bulk_load_boxed(Box::new(quads.into_iter()), None)
            .unwrap();
        assert_eq!(n2, 100);
        assert_eq!(store2.len().unwrap(), 100);
    }

    #[test]
    fn registry_name() {
        let store = RocksDbStore::new().unwrap();
        assert_eq!(store.engine_name(), "rocksdb");
        assert!(
            oxigraph_nova_core::lookup_backend("rocksdb").is_some(),
            "BackendFactory must self-register"
        );
    }
}
