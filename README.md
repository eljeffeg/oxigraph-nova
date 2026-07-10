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
| [`tantivy`](https://crates.io/crates/tantivy) | community | Full-text search engine |


All `rdf-12` / `sparql-12` feature flags are enabled across the parsing stack from day one, giving full RDF-star / quoted-triple support throughout.

---

## Architecture

Nova separates the storage engine from the SPARQL evaluator behind two small
traits, `QuadStore` and `Dataset` — the only seam the query engine depends
on. The default storage engine (`oxigraph-nova-storage-ring`) implements
those traits on top of **the Ring**: six succinct CompactLTJ LOUDS tries (one
per triple ordering) combined with **Leapfrog Triejoin** for worst-case
optimal joins, plus a `BTreeMap`-backed LSM delta so live writes never block
reads.

The full crate layout, the CompactLTJ/Leapfrog-Triejoin design in depth, and
the extension seams for building on top of Nova (`QuadStore`, `Dataset`,
`TextSearch`, `ServiceHandler`, custom SPARQL functions, embedding the HTTP
server) are documented in **[`ARCHITECTURE.md`](./ARCHITECTURE.md)**.

---

## Full-text search (opt-in)


An optional Tantivy-backed full-text index sits alongside the Ring, gated
behind a `fulltext` cargo feature on `oxigraph-nova-storage-ring` so the
dependency and index-file overhead are zero-cost until explicitly enabled:

```sh
# Server binary built with full-text search support:
cargo run -p oxigraph-nova-server --release --features fulltext --bin nova_serve -- \
    --location ./data --fulltext --bind 0.0.0.0:3030

# Or via the `oxigraph` CLI's `serve` subcommand:
cargo run --release --bin oxigraph --features fulltext -- \
    serve --location ./data --fulltext --bind 0.0.0.0:3030
```

Once enabled, two SPARQL extension functions become available under a
dedicated function-IRI namespace — the same convention Jena/GraphDB/Stardog
use for their own full-text extensions:

```sparql
PREFIX text: <http://oxigraph-nova.dev/fn/text#>
SELECT ?s ?o WHERE {
    ?s <http://example.org/name> ?o .
    FILTER(text:query(?o, "fox AND quick"))
}
```

`text:query(?var, "...")` passes the string straight through to Tantivy's
query-parser syntax; `text:contains(?var, "term")` is a plain substring/
phrase match. When the search variable is also bound as a triple pattern's
object elsewhere in the same basic graph pattern, the evaluator pushes the
search down — Tantivy narrows that variable's candidate set *before* the
join runs, rather than scanning every row and re-checking the filter
afterward.

**Consistency model:** the index is rebuilt incrementally on the same
compaction cycle that rebuilds the Ring (`RingStore::compact()`), not on
every write — search results are eventually consistent with the live delta,
matching the existing LFTJ nested-loop-fallback semantics for uncompacted
writes. A generation marker recorded alongside a persistent store's snapshot
detects a stale or missing index at `enable_fulltext()` time (e.g. turning
the feature on for the first time against a pre-existing database, or
recovering from a prior crash) and triggers a one-time full rebuild
automatically rather than silently serving stale results.

See `crates/nova-fulltext/src/lib.rs` for the Tantivy schema and
`crates/nova-storage-ring/src/fulltext.rs` for the compaction-time indexing
glue.


## OWL 2 RL reasoning (opt-in)

`oxigraph-nova-reasoning` adds forward-chaining OWL 2 RL inference as an **opt-in `Dataset` decorator** — zero changes to the evaluator or storage layer. Enable it server-wide with a single flag:

```sh
cargo run -p oxigraph-nova-server --release --bin nova_serve -- \
    --file dataset.nt \
    --file ontology.ttl --graph http://example.org/ontology \
    --reasoning --bind 0.0.0.0:3030
```

Every `/sparql` query is then evaluated over an in-memory `ReasoningDataset` overlay instead of the raw store:

```rust
/// Wraps any Dataset, transparently merging base facts with an in-memory
/// materialized OWL 2 RL closure. The evaluator only ever sees Dataset — it
/// never knows reasoning happened.
pub struct ReasoningDataset<D: Dataset> {
    inner:    D,
    inferred: Vec<(Term, Term, Term)>, // in-memory overlay; never written back into the store
}
impl<D: Dataset> Dataset for ReasoningDataset<D> { … }
```

Rule coverage spans `rdfs:subClassOf`/`subPropertyOf` transitivity, `rdf:type`
propagation, property domain/range/hierarchy propagation, generic
`owl:TransitiveProperty`/`SymmetricProperty`, `owl:equivalentClass`/
`equivalentProperty`, and `owl:inverseOf` — cross-checked against
[`reasonable`](https://github.com/gtfierro/reasonable), an independent OWL 2
RL reasoner, via differential testing. Not yet covered: `owl:sameAs` and the
OWL 2 RL consistency-checking rules.

**HTTP surface:**


```sh
# Query with reasoning enabled — sees both asserted and inferred facts
curl -X POST http://localhost:3030/sparql \
    -H 'Content-Type: application/sparql-query' \
    -H 'Accept: application/sparql-results+json' \
    --data 'ASK { <http://ex/fido> a <http://ex/Animal> }'

# Diagnostics for the current reasoning overlay (404 if --reasoning wasn't passed)
curl http://localhost:3030/reasoning/diagnostics
# → {"inferred_len": 3, "diagnostics": []}

# The SPARQL Service Description also reflects reasoning status
curl http://localhost:3030/
# → sd:defaultEntailmentRegime is http://www.w3.org/ns/entailment/OWL-RL
#   (vs. .../Simple when --reasoning is not passed)
```

**Example:**
```sparql
-- 1. Load your OWL ontology into any named graph (or the default graph)
INSERT DATA { GRAPH <http://example.org/ontology> { <Ex:Dog> rdfs:subClassOf <Ex:Animal> . } }

-- 2. Query against a server started with --reasoning
SELECT ?animal WHERE { ?animal a <Ex:Animal> }
-- → returns both explicit Ex:Animal instances and inferred ones (dogs, etc.)
```

See `crates/nova-reasoning/src/lib.rs`'s module doc comment for the crate's internal architecture (`SortedVecTrie`, `AtomSource`/`CombinedSource`, `fixpoint::closure_over_store`), and `crates/nova-server/tests/reasoning_http.rs` for end-to-end HTTP examples.

## Design trade-offs vs. QLever

QLever (C++) is a high-performance RDF store optimized for bulk-loaded static datasets. It uses six sorted compressed integer arrays and merge joins — an excellent approach for read-heavy analytical workloads over large, stable graphs. The table below shows how the two stores differ across a few dimensions; each row reflects a deliberate design choice, not a deficiency in either system.

| Dimension | QLever | Oxigraph Nova |
|---|---|---|
| Sequential predicate scan | Fast | Fast (Ring POS/PSO traversal — same orderings, near-optimal space) |
| 2–3 way join, selective | Fast | Fast (dictionary integer IDs, no Term clone overhead) |
| Cyclic joins | Merge-join based | LFTJ: worst-case optimal over Ring `TrieIterator`s |
| Live writes | SPARQL UPDATE via offline diff/merge | O(log n) per write into `BTreeMap` delta; Ring rebuild with no read downtime |
| Full-text + SPARQL | Integrated | Tantivy-backed, opt-in (`--fulltext`), incrementally indexed on the compaction cycle |
| Reasoning | None | OWL 2 RL reasoning, opt-in (`--reasoning`), LFTJ-native semi-naive fixpoint engine |
| Memory footprint | Six sorted compressed arrays | Single compact Ring (tens of bytes/triple at benchmark scale) |
| Persistence | On-disk from the start | Optional: in-memory by default, or a crash-safe WAL + MANIFEST + mmap'd ε-serde snapshot store (`--location <dir>`) with near-zero-copy load of both the Ring index and the term dictionary |

Nova runs in two modes. The default is a purely in-process, heap-resident store (matches Oxigraph's own no-`--location` in-memory mode, for apples-to-apples comparisons). Passing `--location <dir>` to `nova_serve` switches to a persistent, crash-recoverable mode: every write is first logged to a write-ahead log, snapshots (Ring + dictionary) are mmap'd back in on restart instead of being fully re-parsed, and a MANIFEST file provides the single atomic commit point tying a snapshot generation to a WAL segment.


---

## Conformance and compatibility

Oxigraph Nova targets full conformance with the W3C SPARQL 1.1 and (Working Draft) SPARQL 1.2 test suites, run against the live, up-to-date W3C test manifests rather than a fixed snapshot — see `tests/w3c/` to run the harness yourself. RDF 1.2 features (quoted triples, `TRIPLE()`, base-direction literals) are supported end-to-end since Nova enables the `rdf-12`/`sparql-12` feature flags across the whole parsing stack from day one.

Because Nova reuses the Oxigraph project's own parsing crates (`spargebra`, `oxrdf`, etc. — see the table above), any gap in those crates shows up here too.


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
| `--fulltext` |          | Enable Tantivy-backed full-text search (`text:query`/`text:contains` extension functions — see "Full-text search" above). Requires building with `cargo ... --features fulltext`; passing the flag without that feature is a hard startup error |


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


## Oxigraph Sponsors

* [Zazuko](https://zazuko.com/), a knowledge graph consulting company.
* [RelationLabs](https://relationlabs.ai/) that is building [Relation-Graph](https://github.com/relationlabs/Relation-Graph), a SPARQL database module for the [Substrate blockchain platform](https://substrate.io/) based on Oxigraph.
* [Field 33](https://field33.com) that was building [an ontology management platform](https://plow.pm/).
* [Magnus Bakken](https://github.com/magbak) who is building [Data Treehouse](https://www.data-treehouse.com/), a time-series + RDF datalake platform, and [chrontext](https://github.com/magbak/chrontext), a SPARQL query endpoint on top of joint RDF and time series databases.
* [DeciSym.AI](https://www.decisym.ai/) a cybersecurity consulting company providing RDF-based software.
* [ACE IoT Solutions](https://aceiotsolutions.com/), a building IOT platform.
* [Albin Larsson](https://byabbe.se/) who is building [GovDirectory](https://www.govdirectory.org/), a directory of public agencies based on Wikidata.

And [others](https://github.com/sponsors/Tpt). Many thanks to them!

## License

MIT

