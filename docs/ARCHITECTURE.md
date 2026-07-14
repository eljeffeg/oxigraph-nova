# Architecture

This document maps the crate layout, the succinct storage engine's internal
design (CompactLTJ Ring + Leapfrog Triejoin), and the **trait seams** that
let a downstream project extend or replace pieces of oxigraph-nova without
forking it. If you're building on top of this project — a custom storage
backend, a custom SPARQL function, a federated `SERVICE` handler, or
embedding the HTTP server into a larger application — start here.

## Crate layout

```
nova-core              (vocabulary + trait definitions; no storage/query logic)
  ├── nova-storage-common   (shared WAL / manifest / dict-snapshot persistence plumbing)
  │     ├── nova-storage-memory  (MemoryStore: simple reference QuadStore impl)
  │     └── nova-storage-ring    (RingStore: LOUDS/CLTJ QuadStore impl, WAL + delta + compaction)
  ├── nova-fulltext         (Tantivy-backed TextSearch impl)
  ├── nova-reasoning        (LftjFixpointEngine: OWL 2 RL forward-chaining reasoner)
  └── nova-query            (Dataset trait, SPARQL evaluator, LFTJ/WCOJ join engine,
                              extension registry, SERVICE federation)
        ├── nova-shacl       (ShaclValidator trait, NativeValidator: SHACL Core validation)
        └── nova-server      (axum HTTP SPARQL 1.1 Protocol server, generic over QuadStore)
              └── nova-cli    (binary: wires RingStore + Server together)
```

Dependency direction is strictly top-to-bottom: `nova-core` depends on
nothing else in the workspace; `nova-query` depends only on `nova-core`;
storage backends depend on `nova-core` (+ `nova-storage-common` for the
persistent ones); `nova-server` depends on `nova-core` + `nova-query` but is
**generic** over the storage backend (`Server<S: QuadStore>`) rather than
hardcoding `RingStore`. `nova-cli` is the only crate that picks a concrete
storage backend.

This layering is what makes each seam below reusable: a downstream crate can
depend on just `nova-core` + `nova-query` and supply its own storage/service
implementations without touching (or forking) `nova-storage-ring` or
`nova-server` at all.

```
┌──────────────────────────────────────────────────────────────┐
│                   oxigraph-nova-server                       │
│             SPARQL 1.2 HTTP endpoint (axum)                  │
│             /sparql GET/POST · /update POST                  │
└───────────────────────────┬──────────────────────────────────┘
                            │
┌───────────────────────────▼──────────────────────────────────┐
│                   oxigraph-nova-query                        │
│    spargebra (parse) → sparopt (normalize) → evaluator       │
│    Dataset trait · Leapfrog Triejoin · ExtensionRegistry     │
└───────────────────────────┬──────────────────────────────────┘
                            │  QuadStore / Dataset traits
           ┌────────────────┴────────────────┐
           │                                 │
┌──────────▼──────────────┐   ┌──────────────▼──────────────────┐
│ oxigraph-nova-storage-  │   │  oxigraph-nova-storage-ring     │
│        memory           │   │  CompactLTJ LOUDS tries         │
│  Vec + linear scan      │   │  + BTreeMap<u128> LSM delta     │
│  testing / dev          │   │  + Leapfrog Triejoin            │
│  (no persistence)       │   │  + WAL/MANIFEST persistence     │
└─────────────────────────┘   └──────────────┬──────────────────┘
           │                                 │
           │                   ┌─────────────▼───────────────────┐
           │                   │ oxigraph-nova-storage-common    │
           │                   │ WAL + MANIFEST + mmap'd ε-serde │
           │                   │ dictionary/snapshot persistence │
           │                   │ (backend-agnostic, reusable)    │
           │                   └─────────────┬───────────────────┘
           └─────────────────┬───────────────┘
                             │
┌────────────────────────────▼──────────────────────────────────┐
│                      oxigraph-nova-core                       │
│    re-exports oxrdf types · QuadStore / TrieIterator traits   │
│    Dictionary (TermId / GraphId) · error types                │
└───────────────────────────────────────────────────────────────┘
```

| Crate | Purpose |
|---|---|
| `oxigraph-nova-core` | RDF types (re-exports `oxrdf`), `QuadStore` trait, `TrieIterator` trait, `Dictionary` (40-bit `TermId`, 8-bit `GraphId`), error types |
| `oxigraph-nova-query` | SPARQL 1.2 evaluator, `Dataset` trait, Leapfrog Triejoin (`lftj.rs`), `ExtensionRegistry` |
| `oxigraph-nova-storage-memory` | In-memory backend — `Vec`-based linear scan; testing and development |
| `oxigraph-nova-storage-common` | Backend-agnostic WAL + MANIFEST + dictionary-persistence machinery, reusable by any `QuadStore` that wants crash-safe durability |
| `oxigraph-nova-storage-ring` | CompactLTJ LOUDS trie index (6 orderings, O(1) navigation) + `BTreeMap<u128>` LSM delta + Leapfrog Triejoin; live-write, WAL-backed persistent storage engine with mmap'd ε-serde snapshot loading |
| `oxigraph-nova-fulltext` | Tantivy-backed full-text index — opt-in via the `fulltext` cargo feature on `oxigraph-nova-storage-ring`; indexed incrementally on the compaction cycle |
| `oxigraph-nova-reasoning` | OWL 2 RL forward-chaining reasoner — LFTJ-native semi-naive fixpoint driver (`LftjFixpointEngine`), `ReasoningDataset` in-memory overlay decorator, opt-in via `--reasoning` on the server |
| `oxigraph-nova-shacl` | SHACL Core validation — `ShaclValidator` trait seam, Nova-owned `ValidationReport`/`ValidationResult` types, `NativeValidator` (dependency-free default implementation); always compiled in, not cargo-feature-gated |
| `oxigraph-nova-server` | SPARQL 1.2 HTTP endpoint (`axum`), SPARQL Query/Update, Graph Store Protocol |
| `oxigraph-nova-w3c-harness` | W3C SPARQL conformance test runner — fetches and caches real W3C manifests (test-only; not published) |
| `oxigraph-nova-bench` | Criterion benchmarks comparing Ring+LFTJ vs in-memory and vs. other RDF stores (not published) |

## Storage engine: the Ring

The main algorithmic contribution is `oxigraph-nova-storage-ring`'s compact
storage engine, which replaces the simple in-memory backend with a succinct
structure from recent research:

**CompactLTJ** (Arroyuelo, Navarro, Gómez-Brandón et al., "CompactLTJ: Space
and Time Efficient Leapfrog Triejoin on Graph Databases", VLDB Journal 2025)
— six explicit height-3 LOUDS tries (one per triple ordering: SPO, POS, OSP,
OPS, PSO, SOP). Each trie stores one bit per trie edge in a `T` bitvector
(Rank9 + SelectAdapt for O(1) `select1`) and ⌈log₂ U⌉ bits per label in an
`L` array (`sux::BitFieldVec`). Navigation is O(1) per step — simultaneously
the most space-efficient *and* the fastest known design for worst-case
optimal joins on RDF.

**Leapfrog Triejoin** (Veldhuizen, ICDT 2014) — worst-case optimal join (AGM
bound). Requires only a `seek`-capable sorted-order iterator. The LOUDS trie
interface satisfies that contract directly, which is why the two algorithms
compose naturally.

```rust
/// A depth-first iterator over one level of an ordered ID trie.
/// Implemented by both the compact Ring index and the BTreeMap live-write delta.
pub trait TrieIterator: Send {
    fn key(&self) -> u64;
    fn seek(&mut self, target: u64);
    fn open(&mut self) -> Box<dyn TrieIterator>;
    fn at_end(&self) -> bool;
}
```

**The live-write delta** — a `BTreeMap<u128, bool>` where each key packs a
complete named-graph quad as a single 128-bit integer:

```
(graph_id as u128) << 120 | (subject_id as u128) << 80 | (pred_id as u128) << 40 | object_id
```

8 + 40 + 40 + 40 = 128 bits exactly. Inserts and deletes land here in
O(log n). When the delta crosses a threshold, the store rebuilds the Ring
over the merged dataset and atomically swaps it in. Queries during the merge
read both layers — no read downtime.

**Term dictionary** — all internal computation runs over **40-bit integer
IDs** (`TermId`), not cloned `Term` objects. The 40-bit ceiling (~1.1
trillion distinct terms) comfortably exceeds Wikidata at current scale
(~200M distinct terms). Named graphs get a separate 8-bit `GraphId` (256
total graphs).

**RDF 1.2 / quoted triples** — a quoted triple (`<< s p o >>`) is assigned
its own `TermId` from the flat ID space, with a parallel side table mapping
`TermId → [s_id, p_id, o_id]`. The index and delta are completely
unaffected — they index flat 40-bit IDs regardless.

## Extension seams

### 1. `QuadStore` — the storage backend seam

Defined in `nova-core/src/store.rs`. Every storage backend (in-memory, Ring,
or a future sled/RocksDB/etc. backend) implements this trait; the query
evaluator only ever calls its methods — it never touches storage internals
directly.

```rust
pub trait QuadStore: Send + Sync + LftjSource {
    fn insert(&self, quad: &Quad) -> Result<bool, Oxigraph>;
    fn remove(&self, quad: &Quad) -> Result<bool, Oxigraph>;
    fn quads_for_pattern(&self, ...) -> Result<Box<dyn Iterator<Item = Result<StoredQuad, Oxigraph>> + '_>, Oxigraph>;
    fn len(&self) -> Result<usize, Oxigraph>;
    fn contains(&self, quad: &Quad) -> Result<bool, Oxigraph>;

    // Everything else below is defaulted — a minimal backend only implements
    // the five methods above.
    fn is_empty(&self) -> Result<bool, Oxigraph> { ... }
    fn known_named_graphs(&self) -> Result<...> { ... }         // default: empty
    fn register_named_graph(&self, _: &GraphName) -> Result<(), Oxigraph> { ... } // default: no-op
    fn extend_boxed(&self, quads: Box<dyn Iterator<Item = Quad> + '_>) -> Result<usize, Oxigraph> { ... } // default: loop over insert
    fn apply_batch(&self, ops: &[QuadOp]) -> Result<(usize, usize), Oxigraph> { ... } // default: loop over insert/remove, returns (inserted, removed) counts
    fn delta_len(&self) -> Option<usize> { None }
    fn compaction_count(&self) -> Option<u64> { None }
    fn compaction_duration_seconds_total(&self) -> Option<f64> { None }
}
```

**Object safety.** `QuadStore` is deliberately object-safe (usable as
`dyn QuadStore` / `Box<dyn QuadStore>`), which lets a caller select a backend
at runtime (e.g. switch between `MemoryStore` and `RingStore` behind a config
flag). This is why `extend_boxed` takes a `Box<dyn Iterator<...>>` instead of
a generic `impl IntoIterator` parameter — a generic method would make the
trait non-object-safe. The ergonomic generic wrapper lives on a separate,
blanket-implemented extension trait instead:

```rust
pub trait QuadStoreExt: QuadStore {
    fn extend(&self, quads: impl IntoIterator<Item = Quad>) -> Result<usize, Oxigraph> {
        self.extend_boxed(Box::new(quads.into_iter()))
    }
}
impl<T: QuadStore + ?Sized> QuadStoreExt for T {}
```

Call `store.extend(some_vec_or_iter)` exactly as you would a generic method —
`QuadStoreExt` is blanket-implemented for every `QuadStore`, so nothing extra
needs to be written to get it.

**Batch/transaction seam (`apply_batch`).** For callers that need to apply a
*mix* of inserts and removes as one logical unit (e.g. implementing a SPARQL
`DELETE { .. } INSERT { .. } WHERE { .. }` Update, or any bulk routine that
both adds and retracts facts), `apply_batch(&self, ops: &[QuadOp])` avoids N
independent lock acquisitions / WAL writes:

```rust
pub enum QuadOp { Insert(Quad), Remove(Quad) }
```

The default implementation just loops calling `insert`/`remove` per op — always
correct, but with no batching benefit. Backends with their own internal lock
and/or write-ahead log (like `RingStore`) should override it to acquire the
lock once, write every resulting WAL record in a single `fsync`, then apply
each op in-memory — see `RingStore`'s `apply_batch` override in
`nova-storage-ring/src/store.rs` (it mirrors the same single-lock,
single-batched-write pattern already used by its bulk-insert `extend_boxed`
override, generalized to mixed insert/remove ops).

**Writing a new backend.** At minimum, implement `insert`/`remove`/
`quads_for_pattern`/`len`/`contains`, plus an empty `impl LftjSource for
MyStore {}` (see below) — everything else defaults to sensible
fallback/no-op behavior. See `nova-storage-memory/src/lib.rs`'s `MemoryStore`
for the simplest possible reference implementation.

### 2. `LftjSource` — optional query-acceleration sub-trait

Also in `nova-core/src/store.rs`, and a **supertrait** of `QuadStore` rather
than being folded into it directly:

```rust
pub trait LftjSource: Send + Sync {
    fn supports_lftj(&self) -> bool { false }
    fn lftj_intern_term(&self, _term: &Term) -> Option<u64> { None }
    fn lftj_decode_term(&self, _id: u64) -> Option<Term> { None }
    fn lftj_graph_id(&self, _graph: &GraphName) -> Option<u8> { None }
    fn lftj_estimate_count(&self, ...) -> u64 { u64::MAX }
    fn lftj_join_scan(&self, ...) -> Option<Box<dyn TrieIterator>> { None }
    fn lftj_real_count(&self, ...) -> Option<u64> { None }
    fn supports_veo_estimates(&self) -> bool { false }
    fn lftj_has_delta(&self) -> bool { false }
}
```

Every method defaults to "unsupported", so a brand-new `QuadStore`
implementor only ever needs one extra line — `impl LftjSource for MyStore
{}` — and the SPARQL evaluator transparently falls back to nested-loop joins.
Only `RingStore`'s LOUDS/CLTJ index overrides these to enable the
Leapfrog-Triejoin / Worst-Case-Optimal-Join accelerated path. This is kept as
a **named, separate trait** (rather than inline methods on `QuadStore`) so
the 9-method acceleration surface is documented and discoverable as one
cohesive unit without cluttering `QuadStore`'s core CRUD/observability
methods.

### 3. `Dataset` / `DatasetLftjSource` — the evaluator's storage seam

Defined in `nova-query/src/dataset.rs`. This is a separate (but parallel)
seam from `QuadStore`: the SPARQL evaluator (`nova-query/src/evaluator.rs`)
only ever talks to a `Dataset`, never a `QuadStore` directly. `StoreDataset<S:
QuadStore>` is the bridge that adapts any `QuadStore` into a `Dataset`,
translating `GraphSelector` (the evaluator's graph-scoping concept) to/from
the store's `u8 graph_id`.

```rust
pub trait Dataset: Send + Sync + DatasetLftjSource {
    fn find_quads<'a>(&'a self, pattern: &QuadPattern) -> Result<QuadIter<'a>>;
    fn contains_quad(&self, s: &Term, p: &Term, o: &Term, g: &GraphName) -> Result<bool> { .. } // default via find_quads
    fn named_graphs<'a>(&'a self) -> Result<Box<dyn Iterator<Item = Result<GraphName>> + 'a>>;
}
```

`DatasetLftjSource` mirrors `LftjSource`'s "everything optional, defaults to
unsupported" shape, keyed on `&GraphSelector` instead of `u8`. Implement
`Dataset` directly (bypassing `QuadStore` entirely) if you want to plug a
completely custom/in-memory/derived data source into the evaluator without
writing a full storage backend — see `InMemoryDataset` for a from-scratch
example, or `StoreDataset<S>` for the "adapt an existing `QuadStore`"
pattern.

`oxigraph-nova-reasoning`'s `ReasoningDataset<D>` (opt-in OWL 2 RL support,
see the README's "OWL 2 RL reasoning" section) is a third pattern built on
this same seam — a **decorator** that wraps an existing `Dataset` rather than
adapting a `QuadStore` or implementing one from scratch:

```rust
/// Wraps any Dataset, transparently merging base facts with an in-memory
/// materialized OWL 2 RL closure. The evaluator only ever sees Dataset — it
/// never knows reasoning happened.
pub struct ReasoningDataset<D: Dataset> {
    inner: D,
    inferred: Vec<(Term, Term, Term)>, // in-memory overlay; never written back into the store
    diagnostics: Vec<Diagnostic>,
}
impl<D: Dataset> Dataset for ReasoningDataset<D> { … }
```

`find_quads` over `GraphSelector::Default`/`Union` transparently unions the
wrapped dataset's matches with the inferred overlay; `GraphSelector::Named`
queries are untouched, since nothing is ever persisted into a named graph.
LFTJ acceleration is intentionally disabled on the wrapped view
(`supports_lftj() == false`), so the evaluator always falls back to its
nested-loop path against `find_quads` — which is what correctly unions the
overlay in. The overlay itself is computed by a pluggable `ReasoningEngine`
trait; the default `LftjFixpointEngine` computes it via a **semi-naive
Datalog-style fixpoint** (see "Storage engine: the Ring" above — same
`TrieIterator`/Leapfrog Triejoin machinery the query evaluator uses for
joins, reused rather than duplicated by a from-scratch triple-scan reasoner).

### 4. `TextSearch` — full-text search backend seam

Defined in `nova-core/src/text_search.rs`. Backs the `text:query`/
`text:contains` SPARQL extension functions. `nova-fulltext`'s Tantivy-backed
`FulltextIndex` is the only current implementation (wired into `RingStore`
behind the `fulltext` cargo feature — see `nova-storage-ring/src/fulltext.rs`),
but any backend (Meilisearch, Elasticsearch, a custom inverted index) can
implement this trait and be wired in the same way.

### 5. `CustomFunction` / `CustomOperator` / `CustomAggregate` — SPARQL extension functions

Defined in `nova-query/src/extensions.rs`, registered at runtime via
`ExtensionRegistry`:

```rust
pub trait CustomFunction: Send + Sync + Debug {
    fn name(&self) -> &str;
    fn call(&self, args: &[Term]) -> Result<Term, ...>;
    // ...
}
pub trait CustomOperator: Send + Sync + Debug { fn symbol(&self) -> &str; ... }
pub trait CustomAggregate: Send + Sync + Debug { fn name(&self) -> &str; ... }

pub struct ExtensionRegistry {
    pub functions: Arc<RwLock<HashMap<String, Box<dyn CustomFunction>>>>,
    // ...
}
```

Register custom IRIs (e.g. `http://example.org/my-function`) so `FILTER`,
`BIND`, and aggregate expressions in SPARQL queries can call out to
application-specific logic without modifying the evaluator itself.

### 6. `ServiceHandler` — SPARQL 1.1 federated `SERVICE` seam

Defined in `nova-query/src/service.rs`, wired into `QueryOptions` and
dispatched from the evaluator's `SERVICE` clause handling:

```rust
pub trait ServiceHandler: Send + Sync {
    fn handle(
        &self,
        service_name: &NamedNode,
        pattern: &GraphPattern,
        base_iri: Option<&str>,
    ) -> anyhow::Result<Solutions>;
}
```

Implement this to route `SERVICE <endpoint> { ... }` clauses to a real HTTP
SPARQL endpoint, an in-process federation layer, a mock for testing, or
anything else — the evaluator has no built-in networking and treats every
`SERVICE` clause as an opaque callout. Wire it in via
`QueryOptions::with_service_handler` (library use) or
`Server::with_service_handler` (HTTP server use, see below).

### 7. `Server<S: QuadStore>` — embedding/composition seam

Defined in `nova-server/src/lib.rs`. Generic over any `QuadStore`
implementation, so it is not tied to `RingStore`:

```rust
pub struct Server<S: QuadStore + 'static> { ... }

impl<S: QuadStore + Send + Sync + 'static> Server<S> {
    pub fn new(store: Arc<S>) -> Self { ... }
    pub fn with_service_handler(mut self, handler: Arc<dyn ServiceHandler>) -> Self { ... }
    pub fn into_router(self) -> Router { ... }  // returns a plain axum::Router
}
```

`into_router()` returns a standard `axum::Router`, which can be nested inside
a larger application's own router, wrapped with additional `tower` middleware
(auth, rate-limiting, tracing, CORS, ...), or served directly. This is the
seam to use when embedding SPARQL protocol support into an existing axum-based
service rather than running `nova-cli`'s standalone binary.

### 8. `ShaclValidator` — SHACL validation seam

Defined in `nova-shacl/src/validator.rs`. Mirrors section 3's `ReasoningEngine`
seam in shape: a single trait method operating at the `Dataset` level (not
raw `QuadStore`), so any storage backend the SPARQL evaluator can already
query can also be SHACL validated:

```rust
/// Validates a data graph (`data`) against a shapes graph (`shapes`),
/// producing a Nova-owned ValidationReport.
pub trait ShaclValidator: Send + Sync {
    fn validate(&self, shapes: &[Quad], data: &dyn Dataset) -> Result<ValidationReport>;
}
```

`shapes` is passed as a plain `&[Quad]` rather than a `Dataset` because
shapes graphs are typically small and loaded once per validation call,
whereas `data` is the (potentially large) dataset being checked and
benefits from the lazy `Dataset::find_quads` query interface — the same
`StoreDataset<S>` adapter from section 3 is what bridges a `QuadStore` into
the `data` parameter here (see `nova-server`'s `validate_post` handler and
`nova-cli`'s `run_validate` for both call sites).

`NativeValidator` is the default, always-available, zero-external-dependency
implementation — a compiled-shapes SHACL Core subset (see
`crates/nova-shacl/src/lib.rs`'s module doc comment for exactly which
targets/constraints are currently supported). Because the trait is a plain,
object-safe seam, alternative implementations (a heavier SHACL-SPARQL-capable
backend, a differential-testing oracle wrapping another validator) can be
swapped in later without any caller needing to change — the same
substitutability principle behind `ReasoningEngine`, `TextSearch`, and
`ServiceHandler` above. Unlike those, `oxigraph-nova-shacl` is not
cargo-feature-gated: it is a plain dependency of `nova-server`/`nova-cli`,
always compiled in.

## Putting it together

A downstream project typically depends on:

- `nova-core` + `nova-query` — always, for the trait definitions and the
  evaluator.
- One storage backend crate (`nova-storage-memory`, `nova-storage-ring`, or
  its own `impl QuadStore for MyStore`).
- `nova-server` — only if HTTP SPARQL Protocol support is wanted; otherwise
  the evaluator can be driven directly via `nova-query`'s `Evaluator` API for
  in-process query execution.

Each seam above defaults to "do nothing extra" so that adopting oxigraph-nova
incrementally — starting with just `QuadStore` + `Dataset`, and opting into
`LftjSource`/`TextSearch`/`ServiceHandler`/extension functions only as
needed — requires no upfront commitment to the more advanced capabilities.
