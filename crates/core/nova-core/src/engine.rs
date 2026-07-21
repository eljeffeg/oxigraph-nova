//! Product-level storage engine seam + self-registering backend registry.
//!
//! [`QuadStore`] is the query/update-facing CRUD + LFTJ trait. Product surfaces
//! (CLI, MCP, `nova-store`, `nova-server`) also need lifecycle operations that
//! are intentionally **not** on `QuadStore`: compact, backup, bulk-load,
//! fulltext, WAL sync policy. Those live on [`StorageEngine`].
//!
//! ## Self-registration
//!
//! Each storage backend crate submits a [`BackendFactory`] via
//! [`inventory::submit!`]. Product code never names a concrete store type:
//!
//! ```ignore
//! let store = oxigraph_nova_core::open_backend("louds", path)?;
//! // or
//! let store = oxigraph_nova_core::new_backend("ring")?;
//! ```
//!
//! Deleting an entire `nova-storage-xxx` crate (and its dependency edge) simply
//! removes that name from [`available_backends`]; everything else keeps
//! working. No match arms, no feature-gated type names in MCP/CLI/store.

use crate::{Oxigraph, Quad, QuadStore, TextSearch};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

// ── SyncPolicy ───────────────────────────────────────────────────────────────

/// WAL durability policy shared by every persistent backend.
///
/// - [`Always`](Self::Always): every write's WAL record(s) are `fsync`ed before
///   the call returns. Maximum durability, highest write latency.
/// - [`Interval`](Self::Interval) (default 500ms): group-commit fsync on a
///   background timer. Higher throughput; crash may lose up to one interval of
///   acknowledged writes (torn-tail recovery still prevents corruption).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyncPolicy {
    Always,
    Interval(Duration),
}

impl Default for SyncPolicy {
    fn default() -> Self {
        SyncPolicy::Interval(Duration::from_millis(500))
    }
}

// ── StorageEngine ────────────────────────────────────────────────────────────

/// Product lifecycle surface on top of [`QuadStore`].
///
/// Object-safe so CLI / MCP / `Store` can hold a single
/// `Arc<dyn StorageEngine>` and stay free of backend-typed match arms.
pub trait StorageEngine: QuadStore {
    /// Short registry name (`"louds"`, `"ring"`, …).
    fn engine_name(&self) -> &'static str;

    /// Merge any write-buffer/delta into the durable image (no-op if none).
    fn compact(&self) -> Result<(), Oxigraph>;

    /// Crash-consistent file-level backup of a **persistent** store into
    /// `destination`. In-memory engines return an error.
    fn backup(&self, destination: &Path) -> Result<(), Oxigraph>;

    /// Set WAL durability policy. No-op for pure in-memory engines.
    fn set_sync_policy(&self, policy: SyncPolicy);

    /// Auto-compact when the live delta reaches `threshold` entries.
    /// No-op for pure in-memory engines.
    fn set_auto_compact_threshold(&self, threshold: usize);

    /// Explicitly fsync the active WAL now. No-op when there is no WAL.
    fn flush_wal(&self) -> Result<(), Oxigraph> {
        Ok(())
    }

    /// Convenience: `len().unwrap_or(0)`.
    fn triple_count(&self) -> usize {
        self.len().unwrap_or(0)
    }

    /// Bulk-insert path used by loaders. Object-safe: progress callback is
    /// `Option<&mut dyn FnMut(u64)>` (count of quads **consumed**).
    ///
    /// See each backend's `bulk_load_with_progress` docs for cadence.
    fn bulk_load_boxed(
        &self,
        quads: Box<dyn Iterator<Item = Quad> + '_>,
        on_progress: Option<&mut dyn FnMut(u64)>,
    ) -> Result<usize, Oxigraph>;

    /// Enable Tantivy full-text indexing when the binary was built with the
    /// `fulltext` feature and this engine supports it. Default: unsupported.
    fn enable_fulltext(&self) -> Result<(), Oxigraph> {
        Err(Oxigraph::Storage(
            "full-text search is not available for this storage engine \
             (rebuild with `--features fulltext`, or this backend does not implement it)"
                .into(),
        ))
    }

    /// After [`enable_fulltext`](Self::enable_fulltext), return a
    /// [`TextSearch`] handle for `QueryOptions::with_text_search`. Default:
    /// `None`.
    fn as_text_search(self: Arc<Self>) -> Option<Arc<dyn TextSearch>> {
        None
    }
}

/// Ergonomic bulk-load helpers over [`StorageEngine`].
pub trait StorageEngineExt: StorageEngine {
    fn bulk_load(&self, quads: impl IntoIterator<Item = Quad>) -> Result<usize, Oxigraph> {
        self.bulk_load_boxed(Box::new(quads.into_iter()), None)
    }

    fn bulk_load_with_progress(
        &self,
        quads: impl IntoIterator<Item = Quad>,
        mut on_progress: impl FnMut(u64),
    ) -> Result<usize, Oxigraph> {
        self.bulk_load_boxed(Box::new(quads.into_iter()), Some(&mut on_progress))
    }
}

impl<T: StorageEngine + ?Sized> StorageEngineExt for T {}

// ── Backend registry (self-registration via inventory) ───────────────────────

/// One registered storage backend. Submitted by backend crates with
/// `inventory::submit! { BackendFactory { ... } }`.
pub struct BackendFactory {
    /// CLI / `--backend` name (`"louds"`, `"ring"`, …).
    pub name: &'static str,
    /// One-line human description for help text / errors.
    pub description: &'static str,
    /// Construct a pure in-memory engine.
    pub new_in_memory: fn() -> Arc<dyn StorageEngine>,
    /// Open or create a persistent engine at `path`.
    pub open: fn(&Path) -> Result<Arc<dyn StorageEngine>, Oxigraph>,
}

inventory::collect!(BackendFactory);

/// All backends linked into this binary, sorted by name.
pub fn available_backends() -> Vec<&'static BackendFactory> {
    let mut v: Vec<_> = inventory::iter::<BackendFactory>.into_iter().collect();
    v.sort_by_key(|b| b.name);
    v
}

/// Look up a backend factory by name (case-sensitive).
pub fn lookup_backend(name: &str) -> Option<&'static BackendFactory> {
    inventory::iter::<BackendFactory>
        .into_iter()
        .find(|b| b.name == name)
}

/// Default backend name: prefer `"louds"` when registered, else the first
/// available name. Errors if **no** backend crate was linked.
pub fn default_backend_name() -> Result<&'static str, Oxigraph> {
    if lookup_backend("louds").is_some() {
        return Ok("louds");
    }
    available_backends().first().map(|b| b.name).ok_or_else(|| {
        Oxigraph::Storage(
            "no storage backends are registered in this binary \
                 (link at least one of oxigraph-nova-engine-louds / \
                 oxigraph-nova-engine-ring with cyclic-ring)"
                .into(),
        )
    })
}

/// Comma-separated list of registered backend names (for error messages).
pub fn backend_names_csv() -> String {
    let names: Vec<&str> = available_backends().iter().map(|b| b.name).collect();
    if names.is_empty() {
        "(none)".into()
    } else {
        names.join(", ")
    }
}

/// Construct an in-memory engine by registry name.
pub fn new_backend(name: &str) -> Result<Arc<dyn StorageEngine>, Oxigraph> {
    let f = lookup_backend(name).ok_or_else(|| unknown_backend(name))?;
    Ok((f.new_in_memory)())
}

/// Open/create a persistent engine by registry name.
pub fn open_backend(name: &str, path: &Path) -> Result<Arc<dyn StorageEngine>, Oxigraph> {
    let f = lookup_backend(name).ok_or_else(|| unknown_backend(name))?;
    (f.open)(path)
}

/// Resolve `--backend` (empty / omitted → default). Returns a clear error
/// listing registered names when unknown.
pub fn require_backend(name: &str) -> Result<&'static str, Oxigraph> {
    let name = if name.is_empty() {
        return default_backend_name();
    } else {
        name
    };
    if lookup_backend(name).is_some() {
        // Return the static name from the factory for a stable &'static str.
        Ok(lookup_backend(name).unwrap().name)
    } else {
        Err(unknown_backend(name))
    }
}

fn unknown_backend(name: &str) -> Oxigraph {
    Oxigraph::Storage(format!(
        "unknown storage backend {name:?}; available: {}",
        backend_names_csv()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{LftjSource, NamedNode, QuadStore, StoredQuad, Term};

    /// Minimal engine used only to exercise the registry helpers in isolation.
    struct DummyEngine;

    impl LftjSource for DummyEngine {}
    impl QuadStore for DummyEngine {
        fn insert(&self, _: &crate::Quad) -> Result<bool, Oxigraph> {
            Ok(true)
        }
        fn remove(&self, _: &crate::Quad) -> Result<bool, Oxigraph> {
            Ok(false)
        }
        fn quads_for_pattern(
            &self,
            _: Option<&Term>,
            _: Option<&NamedNode>,
            _: Option<&Term>,
            _: Option<&crate::GraphName>,
        ) -> Result<Box<dyn Iterator<Item = Result<StoredQuad, Oxigraph>> + '_>, Oxigraph> {
            Ok(Box::new(std::iter::empty()))
        }
        fn len(&self) -> Result<usize, Oxigraph> {
            Ok(0)
        }
        fn contains(&self, _: &crate::Quad) -> Result<bool, Oxigraph> {
            Ok(false)
        }
    }
    impl StorageEngine for DummyEngine {
        fn engine_name(&self) -> &'static str {
            "dummy-test-only"
        }
        fn compact(&self) -> Result<(), Oxigraph> {
            Ok(())
        }
        fn backup(&self, _: &Path) -> Result<(), Oxigraph> {
            Err(Oxigraph::Storage("in-memory".into()))
        }
        fn set_sync_policy(&self, _: SyncPolicy) {}
        fn set_auto_compact_threshold(&self, _: usize) {}
        fn bulk_load_boxed(
            &self,
            quads: Box<dyn Iterator<Item = Quad> + '_>,
            _: Option<&mut dyn FnMut(u64)>,
        ) -> Result<usize, Oxigraph> {
            Ok(quads.count())
        }
    }

    fn dummy_new() -> Arc<dyn StorageEngine> {
        Arc::new(DummyEngine)
    }
    fn dummy_open(_: &Path) -> Result<Arc<dyn StorageEngine>, Oxigraph> {
        Ok(Arc::new(DummyEngine))
    }

    inventory::submit! {
        BackendFactory {
            name: "dummy-test-only",
            description: "unit-test dummy engine",
            new_in_memory: dummy_new,
            open: dummy_open,
        }
    }

    #[test]
    fn registry_finds_dummy() {
        assert!(lookup_backend("dummy-test-only").is_some());
        let eng = new_backend("dummy-test-only").unwrap();
        assert_eq!(eng.engine_name(), "dummy-test-only");
        assert_eq!(eng.triple_count(), 0);
    }

    #[test]
    fn unknown_backend_lists_available() {
        let err = match new_backend("no-such-engine") {
            Ok(_) => panic!("expected unknown backend error"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("dummy-test-only"), "{err}");
    }
}
