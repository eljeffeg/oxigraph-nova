# oxigraph-nova-python

Python bindings for Oxigraph Nova, exposing the same API surface as
upstream [pyoxigraph](https://github.com/oxigraph/oxigraph/tree/main/python)
but backed by `oxigraph-nova-store`'s `RingStore` + Leapfrog Triejoin
evaluator instead of `oxigraph::store::Store`/RocksDB. The Rust source
(`model.rs`, `io.rs`, `sparql.rs`, `store.rs`) and the `tests/` directory
were vendored from upstream pyoxigraph (MIT/Apache-2.0, see `LICENSE-MIT`/
`LICENSE-APACHE`) and adapted in place — `model.rs`/`io.rs`/`sparql.rs` are
close to verbatim (they only wrap `oxrdf`/`oxrdfio`/`spargebra`/`sparesults`
types, which Nova depends on unchanged), while `store.rs`'s `PyStore`
wrapper was rewritten method-by-method against `oxigraph-nova-store`'s
`Store` facade. `dataset.rs`/`PyDataset`/`PyCanonicalizationAlgorithm` were
dropped entirely — Nova has no equivalent in-memory dataset/canonicalization
type to bind to.

## Building

The crate builds a `cdylib` named `pyoxigraph` via [maturin](https://www.maturin.rs/),
same as upstream. There is no prebuilt wheel; build into a virtualenv:

```sh
python3 -m venv /tmp/nova-py-venv
/tmp/nova-py-venv/bin/pip install --quiet --upgrade pip maturin

source /tmp/nova-py-venv/bin/activate
cd crates/nova-python
maturin develop --release
```

This installs an editable `pyoxigraph` wheel into the active virtualenv,
linked against the workspace's current Rust sources. Re-run
`maturin develop --release` after changing any Rust code under
`crates/nova-python/src/` (or any of its Nova dependencies) to pick up the
change.

## Running the tests

The vendored test suite (`tests/test_store.py`, `test_model.py`, `test_io.py`,
`test_doc.py`) is opt-in, like the W3C conformance harness — it requires the
maturin-built extension above to be importable, so it isn't part of the
default `cargo test` workspace run:

```sh
source /tmp/nova-py-venv/bin/activate
cd crates/nova-python
python -m pytest tests/ -v
```

Expected result: **106 passed, 9 skipped**. The 9 skips are all deliberate,
documented divergences from upstream pyoxigraph's fuller feature set (see
"Unsupported features" below) — not bugs. Each skipped test carries a
`unittest.skip("...")` reason string pointing at exactly what's unsupported
and, where relevant, which Nova source file is responsible.

## Unsupported features

Because `oxigraph-nova-store`'s `Store` facade is deliberately a thin
wrapper around Nova's `RingStore` + `nova-query` evaluator (see
`crates/nova-store/src/lib.rs`) rather than a reimplementation of
`oxigraph::store::Store`'s full surface, a handful of upstream pyoxigraph
`Store` features have no Nova-backed equivalent yet and raise
`ValueError`/are skipped in the test suite:

| Feature | `Store` method(s) | Notes |
|---|---|---|
| `use_default_graph_as_union` | `query()` | Not supported by `oxigraph-nova-query`'s dataset seam |
| `default_graph` | `query()` | Query always runs over the store's own default graph |
| `named_graphs` | `query()` | No named-graph restriction set on the evaluator |
| `substitutions` | `query()` | No pre-bound-variable substitution support |
| `custom_functions` | `query()`, `update()` | No Python-callback SPARQL extension function hook |
| `custom_aggregate_functions` | `query()`, `update()` | No Python-callback SPARQL extension aggregate hook |
| `lenient` | `load()`, `bulk_load()` | Nova's RDF parsers are always run in strict mode |
| `LOAD <url>` (SPARQL Update) | `update()` | Requires the `http-client` feature, not compiled into this build |
| `read_only()` | `Store` (class method) | No read-only storage mode in `nova-store`/`nova-storage-ring` |
| `remove_graph()` unregistration | `remove_graph()` | Implemented as `DROP GRAPH`, which Nova's SPARQL Update executor (`crates/nova-query/src/update.rs`) currently treats identically to `CLEAR GRAPH` — the graph quads are removed but the graph name isn't unregistered from `known_named_graphs()` |
| blank-node-named graphs | `add_graph()`, `clear_graph()`, `remove_graph()` | Nova's graph-name model only supports IRI-named graphs for these operations |

All of the above raise a `ValueError` with a message identifying the
unsupported parameter (rather than silently ignoring it), except
`read_only()` and the `remove_graph()` unregistration gap, which are
functional but behave differently from upstream (documented in the
respective test's skip reason).

## Known performance characteristic: eager result materialization

`quads_for_pattern()`/`__iter__()` and the `SELECT`/`CONSTRUCT`/`DESCRIBE`
branches of `query()` fully collect their results into an in-memory `Vec`
before returning an iterator to Python (see `store.rs`'s `quads_for_pattern`
and `sparql.rs`'s `query_results_to_python`), rather than streaming lazily
from the underlying storage scan the way upstream pyoxigraph's
`QuadIter`/`QuerySolutionIter` do. For very large result sets this means the
full result is buffered in memory (and the first row is not available until
the whole query has finished evaluating), rather than results being yielded
incrementally as they're produced.

This is a deliberate, documented trade-off rather than an oversight: fixing
it would require `oxigraph_nova_core::QuadStore::quads_for_pattern`'s
iterator (which borrows from the store) to be turned into something
`'static`-safe to hold inside a `pyo3` `#[pyclass]`, which is a
storage-layer/hot-path change to `nova-core`/`nova-storage-ring`'s scan
machinery — out of scope for this binding layer, and risky to the query
engine's hot paths for a benefit that only matters for very large result
sets. If lazy streaming is ever needed here, it should be designed and
reviewed as a `nova-core`/`nova-storage-ring` change, not a `nova-python`
one.

## Relationship to `crates/nova-store`


`PyStore` in `store.rs` holds a single `oxigraph_nova_store::Store` and
translates each pyoxigraph `Store` method 1:1 onto it — see
`crates/nova-store/src/lib.rs` for the facade this binding sits on top of,
and `ARCHITECTURE.md` for how that facade composes `RingStore` +
`StoreDataset`/`Evaluator` + `execute_update` + `spargebra::SparqlParser`.
