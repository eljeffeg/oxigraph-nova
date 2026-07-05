# Oxigraph Nova

**A Rust-native RDF 1.2 / SPARQL 1.2 triple store with a novel succinct index and worst-case optimal joins.**

Oxigraph Nova reuses the RDF ecosystem's battle-tested parsing and serialization crates while implementing a fresh storage layer and query engine from scratch. The goal is a store that is simultaneously W3C-conformant, live-writable, and algorithmically competitive with the fastest static stores — a combination existing options don't offer: forking Oxigraph means inheriting its sealed `Store` type, and forking OxiRS means inheriting a broken test suite and unaudited code.

---

## Trusted community crates at the core

Oxigraph Nova does **not** re-implement RDF parsing, SPARQL parsing, result serialization, or XSD type semantics. Those are solved problems — Nova uses the same crates that Oxigraph uses:

| Crate | From | Role |
|---|---|---|
| [`oxrdf`](https://crates.io/crates/oxrdf) | Oxigraph project | RDF term types — `NamedNode`, `Literal`, `Quad`, etc. |
| [`oxttl`](https://crates.io/crates/oxttl) | Oxigraph project | Turtle / N-Triples / N-Quads / TriG parser and serializer |
| [`spargebra`](https://crates.io/crates/spargebra) | Oxigraph project | SPARQL 1.1 / 1.2 parser → algebra tree |
| [`sparesults`](https://crates.io/crates/sparesults) | Oxigraph project | SPARQL result I/O (`.srx` XML, `.srj` JSON, `.tsv`) |
| [`oxsdatatypes`](https://crates.io/crates/oxsdatatypes) | Oxigraph project | Correct XSD typed-value semantics (decimal/double/dateTime/duration) |
| [`sparopt`](https://crates.io/crates/sparopt) | Oxigraph project | SPARQL algebra normalizer — filter pushdown, join reordering |
| [`axum`](https://crates.io/crates/axum) | Tokio project | Async HTTP server for the SPARQL endpoint |
| [`tantivy`](https://crates.io/crates/tantivy) | community | Full-text search engine (planned) |
| [`sux`](https://crates.io/crates/sux) | Sebastiano Vigna | Rank9 + SelectAdapt bitvectors and `BitFieldVec` — the LOUDS trie substrate |

All `rdf-12` / `sparql-12` feature flags are enabled across the parsing stack from day one, giving full RDF-star / quoted-triple support throughout.

---

## What is new here

### Storage is a trait, not a type

```rust
pub trait QuadStore: Send + Sync {
    fn insert(&self, quad: &Quad) -> Result<bool, Oxigraph>;
    fn remove(&self, quad: &Quad) -> Result<bool, Oxigraph>;
    fn quads_for_pattern(
        &self,
        subject:    Option<&Subject>,
        predicate:  Option<&NamedNode>,
        object:     Option<&Term>,
        graph_name: Option<&GraphName>,
    ) -> Result<Box<dyn Iterator<Item = Result<Quad, Oxigraph>> + '_>, Oxigraph>;
    fn len(&self) -> Result<usize, Oxigraph>;
    fn contains(&self, quad: &Quad) -> Result<bool, Oxigraph>;
}
```

Any backend — in-memory, compact trie + delta, sled, RocksDB — implements this. The query evaluator only ever calls `quads_for_pattern`; it has no knowledge of what is underneath.

### The evaluator is decoupled from storage via a `Dataset` trait

A `StoreDataset` adapter bridges any `QuadStore` into the evaluator. The evaluator only sees the `Dataset` abstraction:

```rust
pub trait Dataset: Send + Sync {
    fn find_quads<'a>(&'a self, pattern: &QuadPattern) -> Result<QuadIter<'a>>;
    fn contains_quad(&self, s: &Term, p: &Term, o: &Term, g: &GraphName) -> Result<bool>;
    fn named_graphs<'a>(&'a self) -> Result<Box<dyn Iterator<Item = Result<GraphName>> + 'a>>;
}
```

These two traits are the architectural seam that makes the compact storage engine possible without touching the evaluator.

### The Ring index: CompactLTJ LOUDS tries + Leapfrog Triejoin

The main algorithmic contribution is the compact storage engine, which replaces the simple in-memory backend with a succinct structure from recent research:

**CompactLTJ** (Arroyuelo, Navarro, Gómez-Brandón et al., "CompactLTJ: Space and Time Efficient Leapfrog Triejoin on Graph Databases", VLDB Journal 2025) — six explicit height-3 LOUDS tries (one per triple ordering: SPO, POS, OSP, OPS, PSO, SOP). Each trie stores one bit per trie edge in a `T` bitvector (Rank9 + SelectAdapt for O(1) `select1`) and ⌈log₂ U⌉ bits per label in an `L` array (`sux::BitFieldVec`). Navigation is O(1) per step — simultaneously the most space-efficient *and* the fastest known design for worst-case optimal joins on RDF.

**Leapfrog Triejoin** (Veldhuizen, ICDT 2014) — worst-case optimal join (AGM bound). Requires only a `seek`-capable sorted-order iterator. The LOUDS trie interface satisfies that contract directly, which is why the two algorithms compose naturally.

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

**The live-write delta** — a `BTreeMap<u128, bool>` where each key packs a complete named-graph quad as a single 128-bit integer:

```
(graph_id as u128) << 120 | (subject_id as u128) << 80 | (pred_id as u128) << 40 | object_id
```

8 + 40 + 40 + 40 = 128 bits exactly. Inserts and deletes land here in O(log n). When the delta crosses a threshold, the store rebuilds the Ring over the merged dataset and atomically swaps it in. Queries during the merge read both layers — no read downtime.

**Term dictionary** — all internal computation runs over **40-bit integer IDs** (`TermId`), not cloned `Term` objects. The 40-bit ceiling (~1.1 trillion distinct terms) comfortably exceeds Wikidata at current scale (~200M distinct terms). Named graphs get a separate 8-bit `GraphId`. Several `GraphId` values are reserved for system use:

| `GraphId` | Meaning |
|---|---|
| `0` | Default graph (always present; SPARQL default) |
| `1` | Ontology graph — TBox (OWL class/property definitions loaded by the user) |
| `2–253` | User named graphs |
| `254` | Reserved (future system use) |
| `255` | Inference graph — materialized OWL 2 RL entailment closure (planned) |

The ontology graph (`GraphId(1)`) is the input to the planned OWL 2 RL reasoner. Load OWL axioms here via a standard named-graph INSERT; the reasoner reads from it and writes results to `GraphId(255)`.

**RDF 1.2 / quoted triples** — a quoted triple (`<< s p o >>`) is assigned its own `TermId` from the flat ID space, with a parallel side table mapping `TermId → [s_id, p_id, o_id]`. The index and delta are completely unaffected — they index flat 40-bit IDs regardless.

---

## Architecture

```
┌──────────────────────────────────────────────────────────────┐
│                     oxigraph-nova-server                      │
│          SPARQL 1.2 HTTP endpoint (axum)                      │
│          /sparql GET/POST · /update POST                      │
└───────────────────────────┬──────────────────────────────────┘
                            │
┌───────────────────────────▼──────────────────────────────────┐
│                     oxigraph-nova-query                       │
│    spargebra (parse) → sparopt (normalize) → evaluator        │
│    Dataset trait · Leapfrog Triejoin · ExtensionRegistry      │
└───────────────────────────┬──────────────────────────────────┘
                            │  QuadStore / Dataset traits
           ┌────────────────┴────────────────┐
           │                                 │
┌──────────▼──────────────┐   ┌─────────────▼──────────────────┐
│ oxigraph-nova-storage-  │   │  oxigraph-nova-storage-ring     │
│        memory           │   │  CompactLTJ LOUDS tries         │
│  Vec + linear scan      │   │  + BTreeMap<u128> LSM delta     │
│  testing / dev          │   │  + Leapfrog Triejoin            │
└─────────────────────────┘   └────────────────────────────────┘
           │                                 │
           └─────────────────┬───────────────┘
                             │
┌────────────────────────────▼─────────────────────────────────┐
│                      oxigraph-nova-core                       │
│    re-exports oxrdf types · QuadStore / TrieIterator traits   │
│    Dictionary (TermId / GraphId) · error types                │
└───────────────────────────────────────────────────────────────┘
```

### Crates

| Crate | Purpose |
|---|---|
| `oxigraph-nova-core` | RDF types (re-exports `oxrdf`), `QuadStore` trait, `TrieIterator` trait, `Dictionary` (40-bit `TermId`, 8-bit `GraphId`), error types |
| `oxigraph-nova-query` | SPARQL 1.2 evaluator, `Dataset` trait, Leapfrog Triejoin (`lftj.rs`), `ExtensionRegistry` |
| `oxigraph-nova-storage-memory` | In-memory backend — `Vec`-based linear scan; testing and development |
| `oxigraph-nova-storage-ring` | CompactLTJ LOUDS trie index (6 orderings, O(1) navigation) + `BTreeMap<u128>` LSM delta; live-write, persistent storage engine |
| `oxigraph-nova-server` | SPARQL 1.2 HTTP endpoint (`axum`), SPARQL Query/Update, Graph Store Protocol |
| `oxigraph-nova-w3c-harness` | W3C SPARQL conformance test runner — fetches and caches real W3C manifests (test-only; not published) |
| `oxigraph-nova-bench` | Criterion benchmarks comparing Ring+LFTJ vs in-memory and vs. other RDF stores (not published) |

---

## Design trade-offs vs. QLever

QLever (C++) is a high-performance RDF store optimized for bulk-loaded static datasets. It uses six sorted compressed integer arrays and merge joins — an excellent approach for read-heavy analytical workloads over large, stable graphs. The table below shows how the two stores differ across a few dimensions; each row reflects a deliberate design choice, not a deficiency in either system.

| Dimension | QLever | Oxigraph Nova |
|---|---|---|
| Sequential predicate scan | Fast | Fast (Ring POS/PSO traversal — same orderings, near-optimal space) |
| 2–3 way join, selective | Fast | Fast (dictionary integer IDs, no Term clone overhead) |
| Cyclic joins | Merge-join based | LFTJ: worst-case optimal over Ring `TrieIterator`s |
| Live writes | SPARQL UPDATE via offline diff/merge | O(log n) per write into `BTreeMap` delta; Ring rebuild with no read downtime |
| Full-text + SPARQL | Integrated | Tantivy binding injection (planned) |
| Reasoning | None | OWL 2 RL reasoning via `reasonable` (planned) |
| Memory footprint | Six sorted compressed arrays | Single compact Ring (tens of bytes/triple at benchmark scale) |

---

## Conformance and compatibility

Oxigraph Nova targets full conformance with the W3C SPARQL 1.1 and (Working Draft) SPARQL 1.2 test suites, run against the live, up-to-date W3C test manifests rather than a fixed snapshot — see `tests/w3c/` to run the harness yourself. RDF 1.2 features (quoted triples, `TRIPLE()`, base-direction literals) are supported end-to-end since Nova enables the `rdf-12`/`sparql-12` feature flags across the whole parsing stack from day one.

Because Nova reuses the Oxigraph project's own parsing crates (`spargebra`, `oxrdf`, etc. — see the table above), any gap in those crates shows up here too.

### Other RDF ecosystems evaluated

Several other RDF-adjacent crates and projects were evaluated as potential building blocks:

- **[`rdf-reader-jelly`](https://crates.io/crates/rdf-reader-jelly)** — a reader for [Jelly](https://jelly-rdf.github.io), a binary/protobuf RDF format, evaluated as a possible additional bulk-load format. Not yet viable — the published crate has no actual decoding implementation.
- **[`rdf-canon`](https://crates.io/crates/rdf-canon)** — the closest available crate for RDF Dataset Canonicalization (URDNA2015/RDFC-1.0), evaluated for stable content-hashing of query results. Not yet adoptable — it pins an old, pre-RDF-1.2 version of `oxrdf`.
- **[`omnigraph`](https://github.com/ModernRelay/omnigraph)** — a lakehouse-native graph engine with git-style workflows; an interesting adjacent project worth learning from.
- **[`OxiRS`](https://github.com/cool-japan/oxirs)** — has some genuinely interesting ideas and features, though its own codebase and test suite aren't in a state this project could safely build on.

---

## Planned: OWL 2 RL reasoning via `reasonable`

The planned reasoning layer adds forward-chaining OWL 2 RL inference as an **opt-in `Dataset` decorator** — zero changes to the evaluator or storage layer:

```rust
/// Wraps any Dataset, transparently merging base facts with the materialized
/// OWL 2 RL closure. The evaluator only ever sees Dataset — it never knows reasoning happened.
pub struct ReasoningDataset<D: Dataset> {
    base:     Arc<D>,
    inferred: Arc<dyn QuadStore>,  // holds the materialized closure in GraphId(255)
}
impl<D: Dataset> Dataset for ReasoningDataset<D> { … }
```

**Engine:** [`reasonable`](https://github.com/gtfierro/reasonable) — pure-Rust OWL 2 RL reasoner. OWL 2 RL covers `rdfs:subClassOf` transitivity, `owl:sameAs`, property chains, inverse/symmetric/transitive properties, domain/range, and more — the pragmatic decidable profile. Neither QLever nor Tentris reasons; this is a genuine differentiator.

**Materialization policy:** the OWL 2 RL closure is recomputed as part of the same merge cycle that rebuilds the Ring, running `reasonable` over the ontology (`GraphId(1)`) plus base facts and writing inferred triples into the inference graph (`GraphId(255)`). Between merges, the live delta is treated as un-inferred — sound but incomplete; full inference catches up at the next merge. The reasoner is **never on the per-write hot path** — write throughput is unaffected.

**Workflow:**
```sparql
-- 1. Load your OWL ontology into the reserved ontology graph
INSERT DATA { GRAPH <oxigraph-nova:ontology> { <Ex:Dog> rdfs:subClassOf <Ex:Animal> . } }

-- 2. Query with reasoning enabled (server flag or ?reasoning=true)
SELECT ?animal WHERE { ?animal a <Ex:Animal> }
-- → returns both explicit Ex:Animal instances and inferred ones (dogs, etc.)
```

---

## Building

```sh
cargo build
cargo test
```

Requires Rust 1.85+. All dependencies are on `crates.io`; no vendored C++ or patched crates.

To run the full W3C conformance suite (fetches test files on first run, caches locally):

```sh
cargo test -p oxigraph-nova-w3c-harness
```

To run benchmarks:

```sh
cargo bench -p oxigraph-nova-bench                   # all benchmark groups
cargo bench -p oxigraph-nova-bench -- query/triangle # cyclic join only
cargo run -p oxigraph-nova-bench --example memory_report --release  # memory footprint table
```

---

## Design papers

The compact storage and join algorithms are described in published research. Listed in reading order:

1. **CompactLTJ** — "CompactLTJ: Space and Time Efficient Leapfrog Triejoin on Graph Databases" (VLDB Journal 2025), Arroyuelo, Navarro, Gómez-Brandón et al. The compact trie storage engine implemented here.
2. **Leapfrog Triejoin** — "Leapfrog Triejoin: A Simple, Worst-Case Optimal Join Algorithm" (ICDT 2014), Veldhuizen. The join evaluator.
3. **The Ring** — "Worst-Case Optimal Graph Joins in Almost No Space" (SIGMOD 2021 / ACM TODS 2024), Hogan et al. The BWT-based succinct index that motivated the architecture; CompactLTJ builds on the same orderings with faster O(1) navigation.
4. **Wavelet Trees** — "Wavelet Trees for All" (ALENEX 2012), Claude & Navarro. Foundational rank/select primitives.

Context / prior art (read, not implemented):

5. **Tentris / Hypertrie** — "Tentris — A Tensor-Based Triple Store" (ISWC 2020). Prior WCOJ state of the art; higher memory, C++ only.
6. **HoneyComb** — "HoneyComb: A Parallel Worst-Case Optimal Join" (ACM PODS 2025). Future parallelism strategy if LFTJ becomes CPU-bound at Wikidata scale.

---

## License

MIT
