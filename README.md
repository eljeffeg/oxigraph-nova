# Oxigraph Nova

**A Rust-native RDF 1.2 / SPARQL 1.2 triple store with a novel succinct index and worst-case optimal joins.**

Oxigraph Nova was built as a sibling to the [Oxigraph](https://github.com/oxigraph/oxigraph) project, sharing deep roots in the Rust RDF ecosystem but pursue **complementary goals**.

**Oxigraph** has established itself as a production-grade, standards-compliant graph database. Its focus is on delivering a safe, correct, maintainable, and practically fast SPARQL implementation built on the mature RocksDB storage engine. It prioritizes broad ecosystem support (excellent Python and JavaScript bindings, CLI, multiple serialization formats), long-term stability, and reliable behavior for real-world workloads. The foundational crates it maintains (`oxrdf`, `oxttl`, `spargebra`, `sparesults`, `sparopt`, `oxsdatatypes`, etc.) have become trusted infrastructure across the Rust RDF community.

**Oxigraph Nova** has a different charter: to aggressively explore the *algorithmic and standards frontier*. It targets full native support for RDF 1.2 (quoted triples, `TRIPLE()`, base direction, etc.) and SPARQL 1.2 from day one, while implementing advanced techniques from recent database research — most notably CompactLTJ (succinct LOUDS tries) combined with Leapfrog Triejoin for worst-case optimal joins. The goal is a store that is simultaneously W3C-conformant, live-writable, and competitive with the fastest static analytical engines on complex queries — a combination that is difficult to achieve when extending a codebase optimized for different constraints.

Because Nova reuses Oxigraph’s battle-tested parsing, serialization, and algebra crates unchanged, it inherits years of correctness investment and full ecosystem compatibility “for free.” All innovation is isolated behind clean seams (`QuadStore` and `Dataset` traits) in the storage layer and query evaluator. This design makes Nova a natural experimental platform whose successful techniques can later inform or be upstreamed into Oxigraph without compromising the latter’s stability guarantees.

In short:

| Dimension              | Oxigraph                                      | Oxigraph Nova                                      |
|------------------------|-----------------------------------------------|----------------------------------------------------|
| **Primary Goal**       | Production excellence, stability, compliance  | Algorithmic innovation + latest standards          |
| **Storage Engine**     | RocksDB (mature, battle-tested)               | CompactLTJ Ring + LSM delta (research-oriented)    |
| **Join Evaluation**    | Traditional (actively being optimized)        | Leapfrog Triejoin (worst-case optimal)             |
| **RDF / SPARQL Level** | Full RDF 1.1 + preliminary 1.2                | Full RDF 1.2 / SPARQL 1.2            |
| **Stability Profile**  | High — ready for production use               | Experimental / bleeding-edge                       |
| **Ideal For**          | General deployments, broad adoption           | Research, high-performance analytics, standards work |

I envision both projects coexisting comfortably as alternative storage backends behind a common `QuadStore` abstraction. Oxigraph continues to deliver the reliable, widely-supported option the community needs today. Nova serves as the laboratory where we can push performance boundaries, validate emerging standards, and prototype what a next-generation high-performance RDF engine could look like.

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
| [`sux`](https://crates.io/crates/sux) | Sebastiano Vigna | Rank9 + SelectAdapt bitvectors and `BitFieldVec` — the LOUDS trie substrate |
| [`epserde`](https://crates.io/crates/epserde) | Sebastiano Vigna | ε-copy serialization — mmap'd, near-zero-copy load of the Ring and dictionary snapshots |
| [`tantivy`](https://crates.io/crates/tantivy) | community | Full-text search engine (planned — not yet a workspace dependency) |
| [`reasonable`](https://github.com/gtfierro/reasonable) | Gabe Fierro | OWL 2 RL Reasoner (planned — not yet a workspace dependency) |


All `rdf-12` / `sparql-12` feature flags are enabled across the parsing stack from day one, giving full RDF-star / quoted-triple support throughout.

---

## What is new here

### Storage is a trait, not a type

```rust
// Simplified for illustration — the real trait (crates/nova-core/src/store.rs)
// also carries a family of default `lftj_*` / `supports_*` methods that let a
// backend opt into Leapfrog Triejoin acceleration and cardinality estimation;
// backends that don't implement them (e.g. the in-memory store) simply fall
// back to the default nested-loop-friendly behavior.
pub trait QuadStore: Send + Sync {
    fn insert(&self, quad: &Quad) -> Result<bool, Oxigraph>;
    fn remove(&self, quad: &Quad) -> Result<bool, Oxigraph>;
    fn quads_for_pattern(
        &self,
        subject:    Option<&Term>,
        predicate:  Option<&NamedNode>,
        object:     Option<&Term>,
        graph_name: Option<&GraphName>,
    ) -> Result<Box<dyn Iterator<Item = Result<StoredQuad, Oxigraph>> + '_>, Oxigraph>;
    fn len(&self) -> Result<usize, Oxigraph>;
    fn contains(&self, quad: &Quad) -> Result<bool, Oxigraph>;
}
```

Any backend — in-memory, compact trie + delta, sled, RocksDB — implements this. The query evaluator only ever calls `quads_for_pattern`; it has no knowledge of what is underneath.

### The evaluator is decoupled from storage via a `Dataset` trait

A `StoreDataset` adapter bridges any `QuadStore` into the evaluator. The evaluator only sees the `Dataset` abstraction:

```rust
// Simplified for illustration — the real trait (crates/nova-query/src/dataset.rs)
// operates over a richer QuadPattern/GraphSelector pair (so it can express
// GRAPH ?g, FROM/FROM NAMED, and graph unions precisely) and mirrors
// QuadStore's optional lftj_*/supports_* capability methods.
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

### Crates

| Crate | Purpose |
|---|---|
| `oxigraph-nova-core` | RDF types (re-exports `oxrdf`), `QuadStore` trait, `TrieIterator` trait, `Dictionary` (40-bit `TermId`, 8-bit `GraphId`), error types |
| `oxigraph-nova-query` | SPARQL 1.2 evaluator, `Dataset` trait, Leapfrog Triejoin (`lftj.rs`), `ExtensionRegistry` |
| `oxigraph-nova-storage-memory` | In-memory backend — `Vec`-based linear scan; testing and development |
| `oxigraph-nova-storage-common` | Backend-agnostic WAL + MANIFEST + dictionary-persistence machinery, reusable by any `QuadStore` that wants crash-safe durability |
| `oxigraph-nova-storage-ring` | CompactLTJ LOUDS trie index (6 orderings, O(1) navigation) + `BTreeMap<u128>` LSM delta + Leapfrog Triejoin; live-write, WAL-backed persistent storage engine with mmap'd ε-serde snapshot loading |
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
| Persistence | On-disk from the start | Optional: in-memory by default, or a crash-safe WAL + MANIFEST + mmap'd ε-serde snapshot store (`--location <dir>`) with near-zero-copy load of both the Ring index and the term dictionary |

Nova runs in two modes. The default is a purely in-process, heap-resident store (matches Oxigraph's own no-`--location` in-memory mode, for apples-to-apples comparisons). Passing `--location <dir>` to `nova_serve` switches to a persistent, crash-recoverable mode: every write is first logged to a write-ahead log, snapshots (Ring + dictionary) are mmap'd back in on restart instead of being fully re-parsed, and a MANIFEST file provides the single atomic commit point tying a snapshot generation to a WAL segment.


---

## Conformance and compatibility

Oxigraph Nova targets full conformance with the W3C SPARQL 1.1 and (Working Draft) SPARQL 1.2 test suites, run against the live, up-to-date W3C test manifests rather than a fixed snapshot — see `tests/w3c/` to run the harness yourself. RDF 1.2 features (quoted triples, `TRIPLE()`, base-direction literals) are supported end-to-end since Nova enables the `rdf-12`/`sparql-12` feature flags across the whole parsing stack from day one.

Because Nova reuses the Oxigraph project's own parsing crates (`spargebra`, `oxrdf`, etc. — see the table above), any gap in those crates shows up here too.

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

Requires the Rust **nightly** toolchain (pinned via `rust-toolchain.toml`; `rustup` picks it up automatically). All dependencies are on `crates.io`; no vendored C++ or patched crates.


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

These are internal Criterion benchmarks (`benches/bsbm_large.rs`, `benches/wikidata_slice.rs`) that compare Nova's own `RingStore` against its in-memory baseline. For a comparison against external, independently-developed engines, see the next section.

### External comparative benchmarks (Nova vs Oxigraph vs QLever)

[`benches/external/`](./benches/external/README.md) contains a harness that benchmarks Nova against
[Oxigraph](https://github.com/oxigraph/oxigraph) and [QLever](https://github.com/ad-freiburg/qlever) over
identical synthetic BSBM-style datasets and identical SPARQL queries, run through the standard SPARQL 1.1 HTTP Protocol:

| Report | Dataset | Storage mode |
|---|---|---|
| [`RESULTS.md`](./benches/external/RESULTS.md) | 50,000 entities (1.25M triples) | In-memory (all engines) |
| [`RESULTS_500K.md`](./benches/external/RESULTS_500K.md) | 500,000 entities (12.5M triples) | In-memory (all engines) |
| [`RESULTS_DISK.md`](./benches/external/RESULTS_DISK.md) | 50,000 entities (1.25M triples) | Persistent/disk-backed (each engine's native mode) |

See [`benches/external/README.md`](./benches/external/README.md) for the full methodology, storage-model fairness notes, and instructions to run the harness yourself.

---

## Command-line interface (`oxigraph`)

Building the workspace also produces a standalone `oxigraph` binary
(`crates/nova-cli`, package `oxigraph-nova-cli`) that mirrors a subset of
upstream `oxigraph-cli`'s subcommands — `load`, `backup`, and `serve` —
against Nova's own `RingStore`, under the same binary name so scripts/muscle
memory carry over:

```sh
cargo build --release --bin oxigraph
```

| Subcommand | Purpose |
|---|---|
| `oxigraph load --location <dir> --file <path> [--format <fmt>] [--graph <iri>]` | Bulk-load a file directly into a persistent store, bypassing HTTP entirely (much faster than the Graph Store Protocol for large datasets) |
| `oxigraph backup --location <dir> --destination <dir>` | Create a crash-safe, independent copy of a persistent store's WAL + MANIFEST + snapshot |
| `oxigraph serve [--location <dir>] [--file <path>] [--bind <addr>]` | Start the same SPARQL 1.2 HTTP server described below, as a subcommand instead of a separate binary |

```sh
# Bulk-load a dataset directly into a persistent store
cargo run --release --bin oxigraph -- load --location ./data --file dataset.nt

# Back up a store's on-disk data into an independent directory
cargo run --release --bin oxigraph -- backup --location ./data --destination ./backup

# Serve a persistent store over HTTP (equivalent to nova_serve --location ./data)
cargo run --release --bin oxigraph -- serve --location ./data --bind 0.0.0.0:3030
```

Run `oxigraph <subcommand> --help` for the full flag reference for each
subcommand. `oxigraph serve` is a thin wrapper around the exact same server
logic as the standalone `nova_serve` binary documented next — everything in
the following section (endpoints, protocols, formats) applies equally to
`oxigraph serve`.

---

## Running the server

`nova_serve` is a standalone SPARQL 1.1 HTTP server binary, built on the same
`RingStore` (Ring + LFTJ) used throughout this crate. Its flags are
deliberately named to match upstream Oxigraph's own CLI where the concepts
overlap, so a script or muscle memory built around `oxigraph serve`/
`oxigraph load` mostly carries over unchanged:

| Flag                    | Alias(es) | Meaning                                                                 |
|-------------------------|-----------|--------------------------------------------------------------------------|
| `--file <file>`         | `-f`      | Bulk-load an N-Triples dataset (matches `oxigraph load --file`) |
| `--location <dir>`      | `-l`      | Persistent, WAL-backed store rooted at `<dir>` (matches `oxigraph serve --location`) |
| `--bind <addr>`         | `-b`      | Listen address, default `0.0.0.0:3030` (matches `oxigraph serve --bind`) |
| `--compact-threshold <n>` |          | Delta-size threshold that triggers automatic inline compaction (persistent stores only) |
| `--sync-interval-ms <n>` |          | Override the default 500ms WAL fsync/group-commit interval (persistent stores only) |
| `--query-timeout-s <n>` |          | Abort a `/sparql` query that runs longer than `<n>` seconds with `504 Gateway Timeout`. Unset by default (no timeout). Matches upstream Oxigraph's `--timeout` flag |
| `--max-results <n>` |          | Cap the number of result rows/triples a single `/sparql` query may produce; exceeding it returns `413 Payload Too Large`. Unset by default (no cap) |
| `--max-parallel-queries <n>` |          | Bound the number of `/sparql` query evaluations running concurrently; a request arriving once `<n>` evaluations are already in flight is rejected immediately with `503 Service Unavailable`. Unset by default (unbounded) |


```sh
# In-memory only (no persistence) — bulk-loads a dataset and serves it:
cargo run -p oxigraph-nova-server --release --bin nova_serve -- \
    --file dataset.nt --bind 0.0.0.0:3030

# Persistent (WAL-backed) store — writes survive restarts:
cargo run -p oxigraph-nova-server --release --bin nova_serve -- \
    --location ./data --bind 0.0.0.0:3030

# Persistent store, bulk-loaded from a dataset on first run only (an
# existing WAL at --location is replayed on subsequent runs and --file
# is then ignored):
cargo run -p oxigraph-nova-server --release --bin nova_serve -- \
    --location ./data --file dataset.nt --bind 0.0.0.0:3030
```

Once running, the server exposes the SPARQL 1.1 Protocol and the SPARQL 1.1
Graph Store HTTP Protocol, both content-negotiated via `Accept`/`Content-Type`
exactly as Oxigraph's own server does:

```sh
# SPARQL query (also available at /query, matching Oxigraph's endpoint naming)
curl -X POST http://localhost:3030/sparql \
    -H 'Content-Type: application/sparql-query' \
    -H 'Accept: application/sparql-results+json' \
    --data 'SELECT * WHERE { ?s ?p ?o } LIMIT 10'

# SPARQL update (matching Oxigraph's endpoint naming)
curl -X POST http://localhost:3030/update \
    -H 'Content-Type: application/sparql-update' \
    --data 'INSERT DATA { <http://ex/s> <http://ex/p> "v" }'

# Graph Store Protocol — read/replace/merge/clear a graph (identical to Oxigraph's /store)
curl http://localhost:3030/store?default
curl -X PUT http://localhost:3030/store?graph=http://ex/g1 \
    -H 'Content-Type: text/turtle' --data-binary @graph.ttl

# Prometheus-format metrics — dataset/delta size, compaction count/duration,
# query counters, LFTJ vs nested-loop fallback rate
curl http://localhost:3030/metrics
```

See `nova_serve --help` for the full flag reference, and
`crates/nova-server/src/lib.rs`'s module doc comment for the complete list of
supported RDF/SPARQL-results serialization formats.

### Authentication

`nova_serve` has no built-in authentication, matching upstream Oxigraph's own
stance: put it behind a reverse proxy and let the proxy handle access control
rather than duplicating that logic in the server itself. For example, an
nginx `auth_basic` gate in front of the write-capable endpoints (`/update`
and `PUT`/`POST`/`DELETE` on `/store`) is enough for most deployments:

```nginx
location /update {
    auth_basic           "nova";
    auth_basic_user_file /etc/nginx/nova.htpasswd;
    proxy_pass           http://localhost:3030;
}
```

Leave `/sparql`, `/store` `GET`s, and `/metrics` open (or behind their own
proxy rule) as read-only traffic.


### Running with Docker

A `Dockerfile` and `docker-compose.yml` are provided for a zero-toolchain way
to run `nova_serve`, along with [YASGUI](https://github.com/TriplyDB/YASGUI) —
a browser-based SPARQL query editor — pre-wired to the endpoint.

```sh
docker compose up -d
```

This starts two services:

| Service | URL | Purpose |
|---|---|---|
| `nova-serve` | http://localhost:3030 | SPARQL 1.1 Protocol + Graph Store Protocol endpoint, persisted to a named Docker volume (`nova-data`) so data survives restarts |
| `yasgui` | http://localhost:8091 | Browser-based SPARQL query UI, pre-pointed at `http://localhost:3030/sparql` (override via `?endpoint=<url>`) |

CORS is enabled permissively on `nova-serve` so the YASGUI page (a different
origin/port) can query it directly via `fetch` with no proxy required.

To build/run just the server image directly (no Compose, no YASGUI):

```sh
docker build -t oxigraph-nova .
docker run --rm -p 3030:3030 -v nova-data:/data oxigraph-nova \
    --location /data --bind 0.0.0.0:3030
```

See the comments atop `Dockerfile` for bulk-loading a mounted dataset instead
of (or in addition to) a persistent volume.

---

## Design papers



The compact storage and join algorithms are described in published research. Listed in reading order:

1. **CompactLTJ** — "[CompactLTJ: Space and Time Efficient Leapfrog Triejoin on Graph Databases](https://dl.acm.org/doi/10.1145/3661304.3661898)" (VLDB Journal 2025), Arroyuelo, Navarro, Gómez-Brandón et al. The compact trie storage engine implemented here.
2. **Leapfrog Triejoin** — "[Leapfrog Triejoin: A Simple, Worst-Case Optimal Join Algorithm](https://arxiv.org/abs/1210.0481)" (ICDT 2014), Veldhuizen. The join evaluator.
3. **The Ring** — "[Worst-Case Optimal Graph Joins in Almost No Space](https://dl.acm.org/doi/10.1145/3448016.3457256)" (SIGMOD 2021 / ACM TODS 2024), Hogan et al. The BWT-based succinct index that motivated the architecture; CompactLTJ builds on the same orderings with faster O(1) navigation.
4. **Wavelet Trees** — "[Wavelet Trees for All](https://www.sciencedirect.com/science/article/pii/S1570866713000610)" (ALENEX 2012), Claude & Navarro. Foundational rank/select primitives.

Context / prior art (read, not implemented):

5. **Tentris / Hypertrie** — "[Tentris — A Tensor-Based Triple Store](https://dl.acm.org/doi/10.1007/978-3-030-62419-4_4)" (ISWC 2020). Prior WCOJ state of the art; higher memory, C++ only.
6. **HoneyComb** — "[HoneyComb: A Parallel Worst-Case Optimal Join](https://dl.acm.org/doi/10.1145/3725307)" (ACM PODS 2025). Future parallelism strategy if LFTJ becomes CPU-bound at Wikidata scale.

---

## License

MIT

## Oxigraph Sponsors

* [Zazuko](https://zazuko.com/), a knowledge graph consulting company.
* [RelationLabs](https://relationlabs.ai/) that is building [Relation-Graph](https://github.com/relationlabs/Relation-Graph), a SPARQL database module for the [Substrate blockchain platform](https://substrate.io/) based on Oxigraph.
* [Field 33](https://field33.com) that was building [an ontology management platform](https://plow.pm/).
* [Magnus Bakken](https://github.com/magbak) who is building [Data Treehouse](https://www.data-treehouse.com/), a time-series + RDF datalake platform, and [chrontext](https://github.com/magbak/chrontext), a SPARQL query endpoint on top of joint RDF and time series databases.
* [DeciSym.AI](https://www.decisym.ai/) a cybersecurity consulting company providing RDF-based software.
* [ACE IoT Solutions](https://aceiotsolutions.com/), a building IOT platform.
* [Albin Larsson](https://byabbe.se/) who is building [GovDirectory](https://www.govdirectory.org/), a directory of public agencies based on Wikidata.

And [others](https://github.com/sponsors/Tpt). Many thanks to them!
