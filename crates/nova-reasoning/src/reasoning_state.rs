//! [`ReasoningState`] — a cache for the OWL 2 RL reasoning overlay
//! ([`ReasoningDataset`]), computed lazily and rebuilt only when the
//! backing store's compaction generation has advanced since the last
//! build.
//!
//! ## Why a cache at all
//!
//! `ReasoningDataset::wrap` runs a [`ReasoningEngine::infer`] pass eagerly,
//! at construction time — and that pass is O(closure), potentially far
//! above normal query latency (`reasonable` takes ~19s to materialize a
//! ~15K-triple TBox). Re-wrapping on every request would make every single
//! query pay that cost. Instead, the overlay is built once and held; it's
//! only rebuilt when the store's data has actually changed underneath it.
//!
//! ## Staleness policy
//!
//! [`oxigraph_nova_core::QuadStore::compaction_count`] is a ready-made,
//! already-existing generation counter: every persistent write path
//! (`compact()`, `bulk_load()`'s internal `commit_compaction`) bumps it.
//! `ReasoningState::current` compares the store's *current* compaction
//! count against the count recorded when the cached overlay was built; a
//! mismatch means writes have been compacted in since, so the cached
//! overlay may be missing newly-derivable facts (or, more rarely, still
//! contain now-incorrect ones) — it is discarded and rebuilt.
//!
//! Backends that don't report a compaction count at all (`compaction_count`
//! returns `None` — no LSM delta/compaction cycle to key off of) never
//! trigger a rebuild after the first: there is no cheap generation signal
//! to detect staleness for them, so behavior degrades gracefully to
//! `ReasoningDataset`'s own base "wrap once" contract (a caller who mutates
//! such a store after enabling reasoning won't see updated inferences until
//! a new `ReasoningState` is constructed). This mirrors exactly how
//! `LoudsStore::enable_fulltext`'s generation-marker staleness check works
//! (see its doc comment) — both are "eventually consistent, recompute keyed
//! to the compaction cycle" designs.
//!
//! Rebuilding is **not** triggered proactively by a background task; it's
//! checked lazily, on the next call that needs the overlay (a query, or a
//! diagnostics poll) after a compaction has happened. This keeps the design
//! simple (no extra worker thread/task to manage) at the cost of one call
//! per compaction cycle paying the rebuild cost inline — acceptable given
//! compactions are already an infrequent, expensive event compared to
//! steady-state query traffic.
//!
//! This type lives in `nova-reasoning` (rather than e.g. `nova-server`) so
//! any embedder of the reasoning overlay — the HTTP server, the monolithic
//! `Store` facade, or a future language binding — shares the exact same
//! caching/staleness policy instead of re-implementing it.

use crate::{ReasoningDataset, ReasoningEngine};
use anyhow::Result;
use oxigraph_nova_core::QuadStore;
use oxigraph_nova_query::StoreDataset;
use parking_lot::RwLock;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Sentinel `built_at_compaction_count` value meaning "the overlay has
/// never been built yet" — guaranteed to differ from any real
/// `compaction_count()` value on the very first request.
const NEVER_BUILT: u64 = u64::MAX;

/// Caches an in-memory [`ReasoningDataset`] overlay for a `QuadStore` `S`,
/// rebuilding it on demand when stale. See the module doc comment for the
/// full staleness policy.
pub struct ReasoningState<S: QuadStore + 'static> {
    engine: Arc<dyn ReasoningEngine>,
    overlay: RwLock<Option<Arc<ReasoningDataset<StoreDataset<S>>>>>,
    built_at_compaction_count: AtomicU64,
}

impl<S: QuadStore + 'static> ReasoningState<S> {
    pub fn new(engine: Arc<dyn ReasoningEngine>) -> Self {
        Self {
            engine,
            overlay: RwLock::new(None),
            built_at_compaction_count: AtomicU64::new(NEVER_BUILT),
        }
    }

    /// Return the current in-memory reasoning overlay for `store`,
    /// rebuilding it if this is the first call, or if `store`'s compaction
    /// generation has advanced since the cached overlay was built.
    ///
    /// **Blocking.** On a cache miss this calls the engine's
    /// (potentially expensive) `infer()` — callers on an async runtime must
    /// invoke this from a blocking context (`tokio::task::spawn_blocking`),
    /// never directly on an async runtime worker thread.
    pub fn current(&self, store: &Arc<S>) -> Result<Arc<ReasoningDataset<StoreDataset<S>>>> {
        let current_gen = store.compaction_count();

        // Fast path: an up-to-date overlay already exists.
        if let Some(overlay) = self.overlay.read().as_ref()
            && !self.is_stale(current_gen)
        {
            return Ok(Arc::clone(overlay));
        }

        // Slow path: rebuild. Re-check after acquiring the write lock in
        // case another concurrent caller already rebuilt while we were
        // waiting for it (avoids a redundant `infer()` pass under
        // contention).
        let mut guard = self.overlay.write();
        if let Some(overlay) = guard.as_ref()
            && !self.is_stale(current_gen)
        {
            return Ok(Arc::clone(overlay));
        }

        let dataset = StoreDataset::new(Arc::clone(store));
        let wrapped = ReasoningDataset::wrap(dataset, self.engine.as_ref())?;
        let arc = Arc::new(wrapped);
        *guard = Some(Arc::clone(&arc));
        if let Some(g) = current_gen {
            self.built_at_compaction_count.store(g, Ordering::Release);
        }
        Ok(arc)
    }

    /// `true` iff the cached overlay (if any) is out of date w.r.t.
    /// `current_gen` — see the module doc comment's "Staleness policy".
    fn is_stale(&self, current_gen: Option<u64>) -> bool {
        match current_gen {
            Some(g) => g != self.built_at_compaction_count.load(Ordering::Acquire),
            None => false,
        }
    }
}
