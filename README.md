# Oxigraph Nova
<p align="center" style="background-color:rgba(0, 0, 0, 0.2);"><img src="docs/nova-title.png" style="width: 400px;" alt="Banner"></p>

**A Rust-native RDF 1.2 / SPARQL 1.2 triple store with a novel succinct index and worst-case optimal joins.**

**Includes:** `Reasoner` • `SHACL` • `openCypher` • `CLI` • `MCP` • `GeoSPARQL` • `Python` • `Javascript`

Oxigraph Nova was built as a sibling to the [Oxigraph](https://github.com/oxigraph/oxigraph) project, sharing deep roots in the Rust RDF ecosystem but pursue **complementary goals**.

**Oxigraph** has established itself as a production-grade, standards-compliant graph database. Its focus is on delivering a safe, correct, maintainable, and practically fast SPARQL implementation built on the mature RocksDB storage engine. It prioritizes broad ecosystem support (excellent Python and JavaScript bindings, CLI, multiple serialization formats), long-term stability, and reliable behavior for real-world workloads. The foundational crates it maintains have become trusted infrastructure across the Rust RDF community.

**Oxigraph Nova** has a different charter: to aggressively explore the *algorithmic and standards frontier*. It targets full native support for RDF 1.2 (quoted triples, `TRIPLE()`, base direction, etc.) and SPARQL 1.2 from day one, while implementing advanced techniques from recent database research — most notably CompactLTJ (succinct LOUDS tries) & Cyclic-QWT Ring combined with Leapfrog Triejoin for worst-case optimal joins. The goal is a store that is simultaneously W3C-conformant, live-writable, and competitive with the fastest static analytical engines on complex queries — a combination that is difficult to achieve when extending a codebase optimized for different constraints.

Because Nova reuses Oxigraph’s battle-tested parsing, serialization, and algebra crates unchanged, it inherits years of correctness investment and full ecosystem compatibility “for free.” All innovation is isolated behind clean seams, which makes Nova a natural experimental platform whose successful techniques can later inform or be upstreamed into Oxigraph without compromising the latter’s stability guarantees.

In short:

| Dimension              | Oxigraph                                      | Oxigraph Nova                                      |
|------------------------|-----------------------------------------------|----------------------------------------------------|
| **Primary Goal**       | Production excellence, stability, compliance  | Algorithmic innovation + latest standards          |
| **Storage Engine**     | RocksDB (mature, battle-tested)               | CompactLTJ LOUDS (default) + Cyclic-QWT Ring; optional Oxigraph-compatible RocksDB; LSM delta + WAL |
| **Join Evaluation**    | Traditional (actively being optimized)        | Leapfrog Triejoin (worst-case optimal)             |
| **RDF / SPARQL Level** | Full RDF 1.1 + preliminary 1.2                | Full RDF 1.2 / SPARQL 1.2 / openCypher             |
| **Stability Profile**  | High — ready for production use               | Experimental / bleeding-edge                       |
| **Ideal For**          | General deployments, broad adoption           | Research, high-performance analytics, standards work |


I envision both projects coexisting comfortably as alternative storage backends behind a common `QuadStore` abstraction. Oxigraph continues to deliver the reliable, widely-supported option the community needs today. Nova serves as the laboratory where we can push performance boundaries, validate emerging standards, and prototype what a next-generation high-performance RDF engine could look like.

## Architecture

Nova separates the storage engine from the SPARQL evaluator behind two small
traits, `QuadStore` and `Dataset` — the only seam the query engine depends
on. Product surfaces (CLI, MCP, `nova-store`, `nova_serve`) never hard-code a
concrete store type: they hold `Arc<dyn StorageEngine>` and construct engines
through a self-registering **`BackendFactory`** registry
(`inventory::submit!` in each engine crate). Deleting a `nova-engine-xxx`
crate (and its dependency edge) simply removes that name from
`--backend` / `available_backends()`; everything else keeps working.

**Self-registration:** product *library* crates (`nova-server`, `nova-store`,
`nova-mcp`) force-link the engines they expose via feature flags. Binaries
(`nova_serve`, `oxigraph`) inherit those registrations transitively — they
only forward features (e.g. `rocksdb-backend`) and never re-force-link engines
themselves.

| Backend | Crate | Status | Select with |
|---|---|---|---|
| **LOUDS** (`LoudsStore`) | `oxigraph-nova-engine-louds` | Default production: six-order CompactLTJ + LSM delta + WAL/snapshot | `--backend louds` (default) |
| **Ring** (`RingStore`) | `oxigraph-nova-engine-ring` (`cyclic-ring`) | Cyclic-QWT with full product surface (WAL, bulk_load, fulltext, backup) | build with `--features ring-backend`, then `--backend ring` |
| **RocksDB** (`RocksDbStore`) | `oxigraph-nova-engine-rocksdb` | Oxigraph-compatible on-disk format (drop-in data directories); Nova evaluator on top | build with `--features rocksdb-backend`, then `--backend rocksdb` |

`oxigraph-nova-engine-ring` still **re-exports** the LOUDS surface so older
`oxigraph_nova_engine_ring::LoudsStore` imports stay green during the split.

The RocksDB backend opens the same on-disk layout as stock Oxigraph 0.5.x
(`Store::open`), so you can stop `oxigraph serve --location D` and start
`nova_serve --backend rocksdb --location D` (or `oxigraph serve …`) without
migrating data. Queries still run through Nova's Leapfrog evaluator (not
Oxigraph's nested-loop engine); LFTJ acceleration is not available on this
backend yet (pattern scans use Oxigraph's multi-index iterators).

The full crate layout, `StorageEngine` / registry design, CompactLTJ/LFTJ
depth, and extension seams (`QuadStore`, `Dataset`, `TextSearch`,
`ServiceHandler`, embedding the HTTP server) are in
**[`ARCHITECTURE.md`](./docs/ARCHITECTURE.md)**.



---

## Full-text search (opt-in)


An optional Tantivy-backed full-text index sits alongside the storage index,
gated behind a `fulltext` cargo feature (forwarded through `engine-ring` →
`engine-louds`, and available on Ring when built with `ring-backend`) so the
dependency and index-file overhead are zero-cost until explicitly enabled.
Works with every registered backend:

```sh
# Server binary built with full-text search support:
cargo run -p oxigraph-nova-server --release --features fulltext --bin nova_serve -- \
    --location ./data --fulltext --bind 0.0.0.0:3030

# Ring backend + fulltext:
cargo run -p oxigraph-nova-server --release --features "fulltext,ring-backend" --bin nova_serve -- \
    --backend ring --location ./data --fulltext --bind 0.0.0.0:3030

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
compaction cycle that rebuilds the storage image (`StorageEngine::compact()`),
not on every write — search results are eventually consistent with the live
delta, matching the existing LFTJ nested-loop-fallback semantics for
uncompacted writes. A generation marker recorded alongside a persistent
store's snapshot detects a stale or missing index at `enable_fulltext()` time
(e.g. turning the feature on for the first time against a pre-existing
database, or recovering from a prior crash) and triggers a one-time full
rebuild automatically rather than silently serving stale results.

See `crates/core/nova-fulltext/src/lib.rs` for the Tantivy schema and
`crates/engines/nova-engine-louds/src/fulltext.rs` /
`crates/engines/nova-engine-ring/src/ring_store.rs` for compaction-time indexing glue.



## OWL 2 RL reasoning (opt-in)

`oxigraph-nova-reasoning` adds OWL 2 RL inference — computed bottom-up to a
fixpoint via a semi-naive Datalog-style evaluation (a specific, efficient
strategy for the general forward-chaining approach: apply rules to known
facts to derive new ones, repeat until nothing changes) — as an **opt-in
`Dataset` decorator** — zero changes to the evaluator or storage layer.
Enable it server-wide with a single flag:

```sh
cargo run -p oxigraph-nova-server --release --bin nova_serve -- \
    --file dataset.nt \
    --file ontology.ttl --graph http://example.org/ontology \
    --reasoning --bind 0.0.0.0:3030
```

Every `/sparql` query is then evaluated over an in-memory `ReasoningDataset`
overlay instead of the raw store — see
**[`ARCHITECTURE.md`](./docs/ARCHITECTURE.md#3-dataset--datasetlftjsource--the-evaluators-storage-seam)**
for how this decorator is built on the `Dataset` trait.

Rule coverage spans `rdfs:subClassOf`/`subPropertyOf` transitivity, `rdf:type`
propagation, property domain/range/hierarchy propagation, generic
`owl:TransitiveProperty`/`SymmetricProperty`, `owl:equivalentClass`/
`equivalentProperty`, `owl:inverseOf`, and `owl:sameAs` — cross-checked against
[`reasonable`](https://github.com/gtfierro/reasonable), an independent OWL 2
RL reasoner, via differential testing. OWL 2 RL consistency-checking is also
covered: disjoint-class, asymmetric-property, irreflexive-property, and
`owl:sameAs`/`owl:differentFrom` clashes are reported as violations via the
diagnostics endpoint below. Not yet covered: general XSD datatype-value-space
clash detection (e.g. two differently-formatted literals that are provably
equal or provably distinct typed values).


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

See `crates/core/nova-reasoning/src/lib.rs`'s module doc comment for the crate's internal architecture (`SortedVecTrie`, `AtomSource`/`CombinedSource`, `fixpoint::closure_over_store`), and `crates/server/nova-server/tests/reasoning_http.rs` for end-to-end HTTP examples.

## GeoSPARQL support (opt-in)

[`spargeo`](https://crates.io/crates/spargeo) — the GeoSPARQL function
library from Oxigraph's own crate ecosystem — is wired in behind an opt-in
`geosparql` cargo feature on `oxigraph-nova-query` (forwarded through
`oxigraph-nova-server` and the `oxigraph` CLI's own `geosparql` features),
so the dependency is zero-cost until explicitly enabled. Unlike `--fulltext`
or `--reasoning`, there is no runtime flag to pass — GeoSPARQL functions are
pure and stateless, so enabling the cargo feature at build time is the only
step required:

```sh
# Server binary built with GeoSPARQL support:
cargo run -p oxigraph-nova-server --release --features geosparql --bin nova_serve -- \
    --file dataset.nt --bind 0.0.0.0:3030

# Or via the `oxigraph` CLI's `serve` subcommand:
cargo run --release --bin oxigraph --features geosparql -- \
    serve --file dataset.nt --bind 0.0.0.0:3030
```

Once enabled, all 43 GeoSPARQL extension functions become available under
their standard function-IRI namespace, and WKT literals are typed with the
standard `geo:wktLiteral` datatype:

```sparql
PREFIX geof: <http://www.opengis.net/def/function/geosparql/>
PREFIX geo: <http://www.opengis.net/ont/geosparql#>
SELECT ?a ?b (geof:distance(?a, ?b, <http://www.opengis.net/def/uom/OGC/1.0/metre>) AS ?d) WHERE {
    ?a a <http://example.org/Point> ; <http://example.org/wkt> ?wktA .
    ?b a <http://example.org/Point> ; <http://example.org/wkt> ?wktB .
    FILTER(geof:sfIntersects(?wktA, ?wktB))
}
```

Distance/area functions (`geof:distance`, `geof:area`, ...), geometry
construction and conversion (`geof:convexHull`, `geof:envelope`, WKT↔GeoJSON),
and the Simple Features / Egenhofer / RCC8 topological relation families
(`sf:intersects`, `eh:contains`, `rcc8:dc`, etc.) are all dispatched the same
way as any other SPARQL extension function — no spatial index accelerates
the candidate set beforehand, so filtering happens after the enclosing basic
graph pattern has otherwise been evaluated.

See `crates/core/nova-query/src/evaluator.rs`'s `geosparql_function_local` for the
function-dispatch table, and `crates/server/nova-server/src/service_description.rs`
for how enabled functions are advertised via `sd:extensionFunction` in the
SPARQL Service Description.

## SHACL validation

`oxigraph-nova-shacl` adds SHACL Core validation — checking a data graph
against a shapes graph and reporting conformance/violations — as a plain
library dependency of `nova-server`/`nova-cli`, not a cargo feature: unlike
full-text search, reasoning, or GeoSPARQL, there is nothing to opt into at
build time. Validation is exposed two ways:

```sh
# 1. HTTP: validate the running server's entire store against a shapes
#    graph supplied in the request body (content-negotiated the same way
#    as the Graph Store Protocol's PUT/POST bodies):
curl -X POST http://localhost:3030/validate \
    -H 'Content-Type: text/turtle' --data-binary @shapes.ttl

# 2. CLI: validate a persistent store offline (no HTTP, no running server) —
#    exits non-zero if the data does not conform, so this doubles as a CI gate:
cargo run --release --bin oxigraph -- validate \
    --location ./data --shapes shapes.ttl
```

Both paths run the same default implementation, `NativeValidator` — a
dependency-free SHACL Core subset — and produce the same Nova-owned
validation report shape. The HTTP endpoint returns it as JSON:

```json
{
  "conforms": false,
  "violation_count": 1,
  "warning_count": 0,
  "results": [
    {
      "focus_node": "http://ex/alice",
      "path": "http://ex/age",
      "source_shape": "http://ex/PersonShape",
      "source_constraint_component": "http://www.w3.org/ns/shacl#MinCountConstraintComponent",
      "severity": "Violation",
      "message": "...",
      "value": null
    }
  ]
}
```

The `ShaclValidator` trait this is built on mirrors the OWL 2 RL reasoning
seam above (`ReasoningEngine`) — a pluggable, `Dataset`-level operation, so
alternative implementations (e.g. a heavier SHACL-SPARQL-capable backend)
can be swapped in later without changing callers. See
**[`ARCHITECTURE.md`](./docs/ARCHITECTURE.md#8-shaclvalidator--shacl-validation-seam)**
for the trait itself, and `crates/core/nova-shacl/src/lib.rs`'s module doc
comment for exactly which SHACL Core targets (`sh:targetNode`,
`sh:targetClass`, implicit class targets) and constraints (`sh:minCount`,
`sh:maxCount`, `sh:datatype`, `sh:nodeKind`, `sh:class`, `sh:hasValue`,
`sh:in`) `NativeValidator` currently compiles, and which are deferred.

## MCP server (opt-in)

`oxigraph-nova-mcp` exposes SPARQL and openCypher query/update tools, plus
data-model-discovery helpers, to LLM agents over the [Model Context Protocol](https://modelcontextprotocol.io/)
(MCP), built on the official Rust MCP SDK (`rmcp`). It is gated behind an
opt-in `mcp` cargo feature on the `oxigraph` CLI (zero-cost when disabled,
matching the `fulltext`/`geosparql` pattern):

```sh
# Build with MCP support and start the stdio server against a persistent store
cargo run --release --bin oxigraph --features mcp -- \
    mcp serve --location ./data --reasoning --fulltext
```

The server communicates over stdio — the standard transport MCP clients
(Claude Desktop, Claude Code, and similar agent tooling) use to launch a
local MCP server as a subprocess. A typical client config simply points at
the built binary:

```json
{
  "mcpServers": {
    "oxigraph-nova": {
      "command": "/path/to/oxigraph",
      "args": ["mcp", "serve", "--location", "/path/to/data"]
    }
  }
}
```

Six tools are exposed, all evaluated against the same `Evaluator`/
`StoreDataset` (or reasoning-overlay, if `--reasoning` is passed) path
`nova-server`'s HTTP endpoint already uses:

| Tool | Purpose |
|---|---|
| `sparql_query` `{ query }` | Run a SPARQL query — SELECT/ASK results as SPARQL-results-JSON, CONSTRUCT/DESCRIBE as N-Triples |
| `sparql_update` `{ update }` | Run a SPARQL update, returning a success summary |
| `cypher_query` `{ query }` | Run an openCypher read query (`MATCH`/`RETURN`/…) — same result formats as `sparql_query` |
| `cypher_update` `{ update }` | Run an openCypher write statement (`CREATE`/`SET`/`DELETE`/`REMOVE`) |
| `describe_data_model` | Named graphs, distinct predicates, `rdf:type` classes, and a triple count — lets an agent orient itself before writing a query blind |
| `list_graphs` | A cheap named-graphs + triple-count subset of `describe_data_model`, for quick orientation without a full-store scan |

`--reasoning` and `--fulltext` enable the same OWL 2 RL reasoning overlay
and Tantivy-backed full-text search described above (each requiring the
corresponding cargo feature to also be built in), and `--max-results <n>`
caps the number of rows/triples a single `sparql_query` call may return.

See `crates/server/nova-mcp/src/lib.rs`'s module doc comment for the full tool
reference and transport details.


## openCypher support

`oxigraph-nova-cypher` translates an openCypher subset into the same
`spargebra::Query` / `spargebra::Update` representations Nova's SPARQL
stack already evaluates — no separate Cypher engine. Reads and writes
are exposed on all three query surfaces (HTTP, CLI, MCP).

### Supported subset

| Clause | Notes |
|---|---|
| `MATCH` / `WHERE` / `RETURN` / `ORDER BY` / `SKIP` / `LIMIT` / `DISTINCT` | reads |
| `CREATE` / `SET` / `DELETE` / `DETACH DELETE` / `REMOVE` | writes (optional preceding `MATCH`/`WHERE`) |
| Variable-length relationships | Unbounded forms only: `-[:TYPE*]->`, `-[:TYPE*0..]->`, `-[:TYPE*1..]->` (SPARQL property paths have no bounded `{min,max}`) |
| Relationship properties on `MATCH` | `-[r:KNOWS {since: 2020}]->` lowers to RDF 1.2 quoted-triple annotations (`<< ?from :TYPE ?to >> :since 2020`) |

Not supported (clear error, never a panic): `MERGE`, `WITH`, `OPTIONAL MATCH`,
`UNION`, multiple `MATCH` clauses, bounded variable-length relationships
(`*min..max` with an explicit max), chained property access (`a.b.c`),
expression-level property access on a relationship variable
(`WHERE r.since > 2000`), and `CREATE` with relationship properties
(pending an `oxrdf` upgrade — see below).

### RDF ↔ property-graph mapping

Cypher's LPG model maps onto plain RDF triples under fixed namespaces
(exported as `LABEL_NS` / `REL_NS` / `PROP_NS`):

| Cypher | RDF |
|---|---|
| node label `:Person` | `?n rdf:type <…/cypher/label/Person>` |
| node property `{name: "Alice"}` | `?n <…/cypher/prop/name> "Alice"` |
| relationship `-[:KNOWS]->` | `?a <…/cypher/rel/KNOWS> ?b` |
| relationship property `{since: 2020}` (MATCH) | `<< ?a <…/cypher/rel/KNOWS> ?b >> <…/cypher/prop/since> 2020` |

### HTTP / CLI / MCP

```sh
# HTTP read
curl -X POST http://localhost:3030/cypher \
    -H 'Content-Type: application/x-www-form-urlencoded' \
    --data-urlencode 'query=MATCH (n:Person) WHERE n.age > 30 RETURN n.name AS name'

# HTTP write
curl -X POST http://localhost:3030/cypher/update \
    -H 'Content-Type: text/plain' \
    --data 'CREATE (n:Person {name: "Alice", age: 42})'

# CLI offline read / write (against a persistent store)
cargo run --release --bin oxigraph -- cypher update --location ./data \
    --update 'CREATE (a:Person {name: "Alice"})-[:KNOWS]->(b:Person {name: "Bob"})'
cargo run --release --bin oxigraph -- cypher query --location ./data \
    --query 'MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN a.name AS a, b.name AS b'
```

The MCP server (see "MCP server (opt-in)" above) also exposes
`cypher_query` / `cypher_update` tools alongside the SPARQL ones.

### Relationship properties and the oxrdf 0.5 caveat

`MATCH` relationship properties work today via RDF 1.2 annotation triples
because Nova's query engine already supports matching a quoted triple as a
BGP subject. `CREATE` with relationship properties is rejected with a clear
error: inserting a quad whose *subject* is a quoted triple requires
`oxrdf::Quad.subject` to gain a `Triple` variant, which is expected in
oxrdf 0.5. Until that upgrade lands, seed annotated relationships via SPARQL
Update / Turtle-star if you need them on disk, then query them with Cypher
`MATCH`.

See `crates/core/nova-cypher/src/lib.rs` and `crates/core/nova-cypher/src/lower.rs`
for the full grammar, mapping table, and lowering strategy.

## Conformance and compatibility

Oxigraph Nova targets full conformance with the W3C SPARQL 1.1 and (Working Draft) SPARQL 1.2 test suites, run against the live, up-to-date W3C test manifests rather than a fixed snapshot — see `tests/w3c/` to run the harness yourself. RDF 1.2 features (quoted triples, `TRIPLE()`, base-direction literals) are supported end-to-end since Nova enables the `rdf-12`/`sparql-12` feature flags across the whole parsing stack from day one.

Because Nova reuses the Oxigraph project's own parsing crates (`spargebra`, `oxrdf`, etc. — see the table above), any gap in those crates shows up here too.



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

These are internal Criterion benchmarks (`benches/bsbm_large.rs`, `benches/wikidata_slice.rs`) that compare Nova's storage backends against its in-memory baseline. For a comparison against external, independently-developed engines, see the next section.


### External comparative benchmarks (Nova vs Oxigraph vs QLever)

[`benches/external/`](./benches/external/README.md) contains a harness that benchmarks Nova against
[Oxigraph](https://github.com/oxigraph/oxigraph) and [QLever](https://github.com/ad-freiburg/qlever) over
identical synthetic BSBM-style datasets and identical SPARQL queries, run through the standard SPARQL 1.1 HTTP Protocol:

| Report | Storage mode |
|---|---|
| [`RESULTS_MEM.md`](./docs/benchmarks/RESULTS_MEM.md) | In-memory (all engines) |
| [`RESULTS_DISK.md`](./docs/benchmarks/RESULTS_DISK.md) | Persistent/disk-backed (each engine's native mode) |

See [`benches/external/README.md`](./docs/benchmarks/README.md) for the full methodology, storage-model fairness notes, and instructions to run the harness yourself.


## Command-line interface (`oxigraph`)

Building the workspace also produces a standalone `oxigraph` binary
(`crates/server/nova-cli`, package `oxigraph-nova-cli`) that matches upstream
`oxigraph-cli`'s full 9-subcommand surface — `load`, `backup`, `query`,
`update`, `dump`, `convert`, `optimize`, `serve`, and `serve-read-only` —
against any registered `StorageEngine` (default LOUDS; Ring with
`--features ring-backend --backend ring`; Oxigraph-compatible RocksDB with
`--features rocksdb-backend --backend rocksdb`), plus Nova's own SHACL `validate`
addition, an openCypher `cypher` subcommand (`cypher query` / `cypher update`),

and an opt-in `mcp` subcommand (12 top-level subcommands; `cypher` and `mcp`
each have nested actions — `mcp serve` is gated behind the `mcp` cargo
feature; see "MCP server (opt-in)" and "openCypher support" above),
under the same binary name so scripts/muscle memory carry over:


```sh
cargo build --release --bin oxigraph
```

| Subcommand | Argument | Purpose |
|---|---|---|
| oxigraph load | `--location <dir> --file <path> [--format <fmt>] [--graph <iri>]` | Bulk-load a file directly into a persistent store, bypassing HTTP entirely (much faster than the Graph Store Protocol for large datasets) |
| oxigraph backup | `--location <dir> --destination <dir>` | Create a crash-safe, independent copy of a persistent store's WAL + MANIFEST + snapshot |
| oxigraph query | `--location <dir> (--query <q> \| --query-file <f>) [--results-file <f>] [--results-format <fmt>]` | Run a SPARQL query against a persistent store, offline (no HTTP) — results format-negotiated the same way as `/sparql` |
| oxigraph update | `--location <dir> (--update <u> \| --update-file <f>)` | Run a SPARQL update against a persistent store, offline (no HTTP) |
| oxigraph dump | `--location <dir> [--file <f>] [--format <fmt>] [--graph <iri>]` | Serialize a store's logical RDF content out to a file, optionally restricted to one graph |
| oxigraph convert | `[--from-file <f>] [--from-format <fmt>] [--to-file <f>] [--to-format <fmt>]` | Stream-convert one RDF file to another format, with no store involved at all — supports stdin/stdout |
| oxigraph optimize | `--location <dir> [--backend <name>]` | Force storage compaction on demand (`StorageEngine::compact()`) |
| oxigraph serve | `[--location <dir>] [--file <path>] [--bind <addr>] [--backend <name>]` | Start the same SPARQL 1.2 HTTP server described below, as a subcommand instead of a separate binary |
| oxigraph serve-read-only | `--location <dir> [--bind <addr>] [--backend <name>]` | Same as `serve`, but every write (`/update`, `PUT`/`POST`/`DELETE /store`) is rejected at the HTTP layer with `403 Forbidden` |
| oxigraph validate | `--location <dir> --shapes <path> [--shapes-format <fmt>] [--results-file <f>] [--backend <name>]` | Validate a persistent store's data against a SHACL shapes graph, offline (no HTTP) — exits non-zero if the data does not conform, so this doubles as a CI gate (see "SHACL validation" above) |
| oxigraph cypher query | `--location <dir> (--query <q> \| --query-file <f>) [--results-file <f>] [--results-format <fmt>] [--backend <name>]` | Run an openCypher read query against a persistent store, offline — results format-negotiated the same way as `/cypher` / `/sparql` (see "openCypher support" above) |
| oxigraph cypher update | `--location <dir> (--update <u> \| --update-file <f>) [--backend <name>]` | Run an openCypher write statement against a persistent store, offline (see "openCypher support" above) |
| oxigraph mcp serve | `[--location <dir>] [--backend <name>] [--reasoning] [--fulltext] [--max-results <n>]` | `mcp`'s nested `serve` action: start an MCP (Model Context Protocol) server over stdio, exposing SPARQL/openCypher query/update and data-model-discovery tools to LLM agents — requires the `mcp` cargo feature (see "MCP server (opt-in)" above) |


```sh
# Bulk-load a dataset directly into a persistent store
cargo run --release --bin oxigraph -- load --location ./data --file dataset.nt

# Back up a store's on-disk data into an independent directory
cargo run --release --bin oxigraph -- backup --location ./data --destination ./backup

# Run a SPARQL query offline, writing SPARQL-results JSON to stdout
cargo run --release --bin oxigraph -- query --location ./data \
    --query 'SELECT * WHERE { ?s ?p ?o } LIMIT 10'

# Run a SPARQL update offline
cargo run --release --bin oxigraph -- update --location ./data \
    --update 'INSERT DATA { <http://ex/s> <http://ex/p> "v" }'

# Dump one named graph as Turtle
cargo run --release --bin oxigraph -- dump --location ./data \
    --graph http://ex/g1 --format ttl --file g1.ttl

# Convert a file between RDF formats with no store at all (also supports stdin/stdout)
cargo run --release --bin oxigraph -- convert \
    --from-file data.nt --from-format nt --to-file data.ttl --to-format ttl

# Force compaction on demand
cargo run --release --bin oxigraph -- optimize --location ./data

# Serve a persistent store over HTTP (equivalent to nova_serve --location ./data)
cargo run --release --bin oxigraph -- serve --location ./data --bind 0.0.0.0:3030

# Serve the same store read-only — writes get 403 Forbidden
cargo run --release --bin oxigraph -- serve-read-only --location ./data --bind 0.0.0.0:3031

# Validate a persistent store against a SHACL shapes graph, offline
cargo run --release --bin oxigraph -- validate --location ./data --shapes shapes.ttl

# Run an openCypher write, then a read, offline
cargo run --release --bin oxigraph -- cypher update --location ./data \
    --update 'CREATE (n:Person {name: "Alice"})'
cargo run --release --bin oxigraph -- cypher query --location ./data \
    --query 'MATCH (n:Person) RETURN n.name AS name'

# Start an MCP server over stdio for LLM-agent access (requires --features mcp)
cargo run --release --bin oxigraph --features mcp -- mcp serve --location ./data
```

Run `oxigraph <subcommand> --help` for the full flag reference for each
subcommand. `oxigraph serve`/`serve-read-only` are thin wrappers around the
exact same server logic as the standalone `nova_serve` binary documented
next — everything in the following section (endpoints, protocols, formats)
applies equally to `oxigraph serve`. `serve-read-only`'s write-gate is
HTTP-layer only, not storage-level concurrent-multi-process read isolation —
see `oxigraph serve-read-only --help` for the exact caveat.

A handful of upstream `oxigraph-cli` flags aren't implemented yet (query
`--explain`/`--stats`).

---

## Running the server

`nova_serve` is a standalone SPARQL 1.1 HTTP server binary. It constructs a
`StorageEngine` via the self-registering backend registry (default LOUDS;
Ring with `--features ring-backend --backend ring`; Oxigraph-compatible
RocksDB with `--features rocksdb-backend --backend rocksdb`) and serves it
through `Server<dyn QuadStore>`. Flags match upstream Oxigraph's CLI where
concepts overlap:

| Flag                    | Alias(es) | Meaning                                                                 |
|-------------------------|-----------|--------------------------------------------------------------------------|
| `--file <file>`         | `-f`      | Bulk-load an RDF dataset (matches `oxigraph load --file`) |
| `--location <dir>`      | `-l`      | Persistent store rooted at `<dir>` (LOUDS/Ring: WAL; RocksDB: Oxigraph on-disk format) (matches `oxigraph serve --location`) |
| `--backend <name>`      |           | Storage backend (`louds` default; `ring` requires `--features ring-backend`; `rocksdb` requires `--features rocksdb-backend`) |
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

# Ring backend (WAL + snapshot, same product surface as LOUDS):
cargo run -p oxigraph-nova-server --release --features ring-backend --bin nova_serve -- \
    --backend ring --location ./data --file dataset.nt --bind 0.0.0.0:3030

# Oxigraph-compatible RocksDB (drop-in data directory; Nova SPARQL evaluator):
cargo run -p oxigraph-nova-server --release --features rocksdb-backend --bin nova_serve -- \
    --backend rocksdb --location ./oxigraph-data --bind 0.0.0.0:3030

# Same via the unified CLI:
cargo run --release --bin oxigraph --features rocksdb-backend -- \
    serve --backend rocksdb --location ./oxigraph-data --bind 0.0.0.0:3030

# Persistent store, bulk-loaded from a dataset on first run only (an
# existing WAL at --location is replayed on subsequent runs and --file
# is then ignored):
cargo run -p oxigraph-nova-server --release --bin nova_serve -- \
    --location ./data --file dataset.nt --bind 0.0.0.0:3030
```



Once running, the server exposes the SPARQL 1.1 Protocol, the SPARQL 1.1
Graph Store HTTP Protocol, and openCypher query/update endpoints, all
content-negotiated via `Accept`/`Content-Type` exactly as Oxigraph's own
server does (Cypher results reuse the same SPARQL-results serializers):

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

# openCypher query / update (same result-format negotiation as /sparql)
curl -X POST http://localhost:3030/cypher \
    -H 'Content-Type: application/x-www-form-urlencoded' \
    --data-urlencode 'query=MATCH (n:Person) RETURN n.name AS name LIMIT 10'
curl -X POST http://localhost:3030/cypher/update \
    -H 'Content-Type: text/plain' \
    --data 'CREATE (n:Person {name: "Alice"})'

# Graph Store Protocol — read/replace/merge/clear a graph (identical to Oxigraph's /store)
curl http://localhost:3030/store?default
curl -X PUT http://localhost:3030/store?graph=http://ex/g1 \
    -H 'Content-Type: text/turtle' --data-binary @graph.ttl

# Prometheus-format metrics — dataset/delta size, compaction count/duration,
# query counters, LFTJ vs nested-loop fallback rate
curl http://localhost:3030/metrics
```

See `nova_serve --help` for the full flag reference, and
`crates/server/nova-server/src/lib.rs`'s module doc comment for the complete list of
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

