//! Per-query execution limits: cancellation and result-size caps.
//!
//! [`CancellationToken`] is a cheap `Arc<AtomicBool>` handle that the HTTP
//! layer flips on a query timeout (via a watchdog thread) or on client
//! disconnect; the evaluator polls it periodically in its hot loops (BGP
//! join evaluation, property-path BFS) via [`CancellationToken::check`].
//! [`QueryOptions`] bundles this together with an optional result-row cap.

use oxigraph_nova_core::TextSearch;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};


/// A cheap-to-clone flag that signals a running query should stop.
///
/// Cloning shares the same underlying flag (it's an `Arc<AtomicBool>`), so
/// the HTTP layer can hold one clone and flip it from another thread (a
/// timeout watchdog, or a client-disconnect handler) while the evaluator —
/// running on a different thread — holds another clone and polls it.
#[derive(Clone, Default)]
pub struct CancellationToken(Arc<AtomicBool>);

impl CancellationToken {
    /// Create a new, not-yet-cancelled token.
    pub fn new() -> Self {
        Self(Arc::new(AtomicBool::new(false)))
    }

    /// Signal cancellation. Idempotent; safe to call from any thread.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    /// Has this token been cancelled?
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }

    /// Convenience: return `Err(EvalLimitError::Cancelled)` if cancelled.
    pub fn check(&self) -> Result<(), EvalLimitError> {
        if self.is_cancelled() {
            Err(EvalLimitError::Cancelled)
        } else {
            Ok(())
        }
    }
}

/// Distinguishable evaluation-abort reasons, so the HTTP layer can map them
/// to the right status code (e.g. 408/504 for `Cancelled`, 400 for
/// `ResultLimitExceeded`) rather than a generic 500.
///
/// Carried as the payload of an `anyhow::Error` (the evaluator's error type
/// throughout); callers that need to distinguish it use
/// `anyhow::Error::downcast_ref::<EvalLimitError>()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvalLimitError {
    /// The query was cancelled — either a configured timeout elapsed, or
    /// the HTTP client disconnected before the query finished.
    Cancelled,
    /// The query produced more solution rows than `QueryOptions::max_results`
    /// allows.
    ResultLimitExceeded(usize),
}

impl std::fmt::Display for EvalLimitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EvalLimitError::Cancelled => {
                write!(
                    f,
                    "query cancelled (timeout exceeded or client disconnected)"
                )
            }
            EvalLimitError::ResultLimitExceeded(n) => {
                write!(f, "query exceeded the result limit of {n} row(s)")
            }
        }
    }
}

impl std::error::Error for EvalLimitError {}

/// Per-query execution limits passed to [`crate::Evaluator::with_options`].
///
/// The default (`QueryOptions::default()`, equivalent to what
/// `Evaluator::new` uses internally) applies no limits at all — identical
/// behavior to the evaluator before this type existed.
#[derive(Clone, Default)]
pub struct QueryOptions {
    /// Checked periodically in hot evaluation loops; `None` means "never
    /// cancelled" (no per-iteration overhead beyond an `Option` match).
    pub cancellation: Option<CancellationToken>,
    /// Maximum number of solution rows to produce. Once exceeded, evaluation
    /// stops early and returns `Err` carrying
    /// `EvalLimitError::ResultLimitExceeded`. `None` means unlimited.
    pub max_results: Option<usize>,
    /// Optional storage-backed full-text search capability (see
    /// [`oxigraph_nova_core::TextSearch`]). `None` means the store has no
    /// full-text index configured (the default) — `text:query`/
    /// `text:contains` calls then evaluate as unbound rather than
    /// performing a search.
    pub text_search: Option<Arc<dyn TextSearch>>,
}

impl QueryOptions {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_cancellation(mut self, token: CancellationToken) -> Self {
        self.cancellation = Some(token);
        self
    }

    pub fn with_max_results(mut self, max: usize) -> Self {
        self.max_results = Some(max);
        self
    }

    /// Attach a full-text search backend, enabling `text:query`/
    /// `text:contains` extension-function dispatch in the evaluator.
    pub fn with_text_search(mut self, ts: Arc<dyn TextSearch>) -> Self {
        self.text_search = Some(ts);
        self
    }

    /// Cheap check for use inside hot loops: cancellation only (no
    /// allocation, no branch on `max_results`).
    pub(crate) fn check_cancelled(&self) -> Result<(), EvalLimitError> {
        match &self.cancellation {
            Some(token) => token.check(),
            None => Ok(()),
        }
    }

    /// Check both cancellation and whether `current_len` has already
    /// exceeded `max_results`. Used at result-accumulation points.
    pub(crate) fn check(&self, current_len: usize) -> Result<(), EvalLimitError> {
        self.check_cancelled()?;
        if let Some(max) = self.max_results
            && current_len > max
        {
            return Err(EvalLimitError::ResultLimitExceeded(max));
        }
        Ok(())
    }
}
