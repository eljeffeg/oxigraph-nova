//! [`ShaclValidator`] — the pluggable "validate a data graph against a
//! shapes graph" seam.
//!
//! Mirrors `oxigraph_nova_reasoning::ReasoningEngine`: a single trait method
//! operating at the [`Dataset`] level (not raw `QuadStore`), so any storage
//! backend the SPARQL evaluator can already query can also be SHACL
//! validated, and so alternative backend implementations (e.g. a `rudof`
//! adapter, or a vendored `oxreason` fork) can be swapped in later without
//! callers needing to change.

use crate::report::ValidationReport;
use anyhow::Result;
use oxigraph_nova_core::Quad;
use oxigraph_nova_query::Dataset;

/// Validates a data graph (`data`) against a shapes graph (`shapes`),
/// producing a Nova-owned [`ValidationReport`].
///
/// `shapes` is passed as a plain `&[Quad]` (not a `Dataset`) because shapes
/// graphs are typically small and loaded once per validation call, whereas
/// `data` is the (potentially large) dataset being checked and benefits
/// from the lazy `Dataset::find_quads` query interface.
pub trait ShaclValidator: Send + Sync {
    fn validate(&self, shapes: &[Quad], data: &dyn Dataset) -> Result<ValidationReport>;
}
