#![expect(clippy::trivially_copy_pass_by_ref, clippy::unused_self)]

mod io;
mod model;
mod sparql;
mod store;

use crate::io::*;
use crate::model::*;
use crate::sparql::*;
use crate::store::*;
use pyo3::prelude::*;

/// Oxigraph Nova Python bindings.
///
/// This module mirrors the `pyoxigraph` API surface but is backed by the
/// Oxigraph Nova storage/query engine (`oxigraph-nova-store`) instead of
/// upstream Oxigraph's RocksDB-based store.
#[pymodule]
pub mod pyoxigraph {
    #[expect(non_upper_case_globals)]
    #[pymodule_export]
    const __version__: &str = env!("CARGO_PKG_VERSION");
    #[cfg(feature = "rdf-12")]
    #[pymodule_export]
    use super::PyBaseDirection;
    #[pymodule_export]
    use super::{
        PyBlankNode, PyDefaultGraph, PyLiteral, PyNamedNode, PyQuad, PyQuadParser, PyQueryBoolean,
        PyQueryResultsFormat, PyQuerySolution, PyQuerySolutions, PyQueryTriples, PyRdfFormat,
        PyStore, PyTriple, PyVariable, parse, parse_query_results, serialize,
    };
}
