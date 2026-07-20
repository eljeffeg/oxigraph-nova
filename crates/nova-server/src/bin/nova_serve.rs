//! Standalone Nova SPARQL server binary backed by `LoudsStore` (Ring + LFTJ).
//!
//! Loads an RDF file into memory, builds the Ring index (via
//! `compact()`), then serves the SPARQL 1.1 HTTP Protocol on the given
//! address. Used for external comparative benchmarking against Oxigraph and
//! QLever — see `benches/external/`.
//!
//! # Usage
//! ```bash
//! # In-memory only (no persistence, matches Oxigraph's `serve` without `--location`):
//! cargo run -p oxigraph-nova-server --release --bin nova_serve -- \
//!     --file /tmp/oxigraph-nova-bench/dataset.nt --bind 0.0.0.0:3030
//!
//! # Persistent (WAL-backed) store: writes survive restarts.
//! cargo run -p oxigraph-nova-server --release --bin nova_serve -- \
//!     --location /tmp/oxigraph-nova-bench/data --bind 0.0.0.0:3030
//!
//! # Persistent store, bulk-loaded from a dataset on first run only (if the
//! # WAL directory is empty/fresh; on subsequent runs the existing WAL is
//! # replayed instead and --file is ignored):
//! cargo run -p oxigraph-nova-server --release --bin nova_serve -- \
//!     --location /tmp/oxigraph-nova-bench/data \
//!     --file /tmp/oxigraph-nova-bench/dataset.nt --bind 0.0.0.0:3030
//!
//! # Load a TBox into its own named graph and enable OWL 2 RL reasoning
//! # (QLever-style --graph flag; no dedicated --ontology flag — just load
//! # the ontology file into whatever graph you like):
//! cargo run -p oxigraph-nova-server --release --bin nova_serve -- \
//!     --file dataset.nt \
//!     --file ontology.ttl --graph http://example.org/ontology \
//!     --reasoning --bind 0.0.0.0:3030
//! ```
//!
//! `--location`/`-l`, `--bind`/`-b`, and `--file`/`-f` are named identically
//! to upstream Oxigraph's own `oxigraph serve --location <path> --bind <addr>`
//! and `oxigraph load --file <file>` flags, so a script or muscle memory
//! built around either binary carries over unchanged.
//!
//! `--file` may be given more than once (each occurrence loads one more
//! file); `--graph` applies to the single `--file` that immediately follows
//! it and only to plain-triple formats (see `resolve_format`'s doc comment
//! for format detection and `--graph`'s interaction with dataset formats).
//!
//! Storage model note: without `--location`, `LoudsStore` is always fully
//! in-process heap memory — there is no disk persistence at all, so the
//! entire dataset plus index must fit in RAM (matches Oxigraph's `serve` run
//! without `--location`, for a fair memory-vs-memory comparison). With
//! `--location <dir>`, every `insert`/`remove`/`register_named_graph` call is
//! first durably logged to a write-ahead log (WAL) file in `<dir>` before
//! being applied in memory — see `oxigraph_nova_storage_ring::wal` and
//! `LoudsStore::open` for details of the overall persistent-storage design.

use mimalloc::MiMalloc;
use oxigraph_nova_core::{GraphName, NamedNode, Quad};

use oxigraph_nova_reasoning::{LftjFixpointEngine, ReasoningEngine};
use oxigraph_nova_server::Server;
use oxigraph_nova_storage_ring::{LoudsStore, SyncPolicy};
use oxrdfio::{RdfFormat, RdfParser};
use std::env;
use std::fs::File;


// Large SELECT result sets (hundreds of thousands of rows) allocate and free
// many large (multi-KiB–multi-MiB) buffers in quick succession while
// serializing a response. macOS's system allocator (`libmalloc`) keeps freed
// large-block VM regions mapped rather than eagerly returning them to the OS,
// so a process's physical footprint (RSS) climbs monotonically across
// repeated large-result-set queries and never comes back down under normal
// operation — this looks exactly like a memory leak when watched via
// `vmmap`/`ps`, but a byte-exact allocation/deallocation counting harness
// (`benches/src/bin/profile_eval.rs --count-allocs`) proves every single
// byte allocated during evaluation is deallocated again before the next
// query starts: nothing is actually leaked at the Rust level. Swapping in
// `mimalloc`, which is much more aggressive about giving large freed regions
// back to the OS, eliminates the growth entirely (RSS plateaus rather than
// climbing without bound across dozens of repeated large-result queries —
// verified via `benches/src/bin/profile_eval_mimalloc.rs`).
#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

// See `oxigraph_nova_server::mimalloc_tuning`'s module doc comment for the
// full rationale behind this bulk-load/compaction transient-memory tuning
// (measured ~4.8x steady-state / ~1.5x peak physical-footprint reduction on
// a 500K-entity/12.5M-triple in-memory dataset).
use oxigraph_nova_server::mimalloc_tuning;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Resolve an RDF format from a file's extension, mirroring
/// `oxigraph-cli`'s `nova-cli/src/load.rs::resolve_format` (kept as a small
/// standalone copy here rather than a shared dependency, since `nova-cli`
/// is a separate binary crate and this is the only bit of it `nova_serve`
/// needs).
fn resolve_format(path: &Path) -> RdfFormat {
    let name = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_else(|| panic!("cannot guess RDF format from file extension of {}; expected one of: nt, ttl, rdf, nq, trig, jsonld", path.display()));

    RdfFormat::from_extension(&name)
        .or_else(|| RdfFormat::from_media_type(&name))
        .unwrap_or_else(|| {
            panic!(
                "unrecognized RDF format {name:?}; expected one of: nt, ttl, rdf, nq, trig, jsonld"
            )
        })
}

/// One `--file [--graph <iri>]` entry from the command line.
struct FileLoad {
    path: PathBuf,
    graph: Option<GraphName>,
}

#[tokio::main]
async fn main() {
    // Must run before any other allocation — see
    // `oxigraph_nova_server::mimalloc_tuning`'s module doc comment.
    mimalloc_tuning::tune_mimalloc_purge_delay();

    tracing_subscriber::fmt::init();

    let mut files: Vec<FileLoad> = Vec::new();

    let mut location: Option<PathBuf> = None;
    let mut bind: String = "0.0.0.0:3030".to_string();
    let mut compact_threshold: Option<usize> = None;
    let mut sync_interval_ms: Option<u64> = None;
    let mut query_timeout_s: Option<u64> = None;
    let mut max_results: Option<usize> = None;
    let mut max_parallel_queries: Option<usize> = None;
    #[cfg_attr(not(feature = "fulltext"), allow(unused_mut))]
    let mut fulltext = false;
    let mut reasoning = false;
    let mut union_default_graph = false;
    // "louds" (default) | "ring" (cyclic QWT pilot; requires --features ring-backend)
    let mut backend = "louds".to_string();

    let args: Vec<String> = env::args().collect();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--backend" => {
                i += 1;
                backend = args[i].clone();
            }
            "--file" | "-f" => {

                i += 1;
                files.push(FileLoad {
                    path: PathBuf::from(&args[i]),
                    graph: None,
                });
            }
            "--graph" => {
                i += 1;
                let iri = &args[i];
                let last = files
                    .last_mut()
                    .unwrap_or_else(|| panic!("--graph must follow the --file it applies to"));
                let node = NamedNode::new(iri)
                    .unwrap_or_else(|e| panic!("--graph {iri:?} is not a valid IRI: {e}"));
                last.graph = Some(GraphName::NamedNode(node));
            }
            "--location" | "-l" => {
                i += 1;
                location = Some(PathBuf::from(&args[i]));
            }
            "--bind" | "-b" => {
                i += 1;
                bind = args[i].clone();
            }
            "--compact-threshold" => {
                i += 1;
                compact_threshold =
                    Some(args[i].parse().unwrap_or_else(|_| {
                        panic!("--compact-threshold must be a positive integer")
                    }));
            }
            "--sync-interval-ms" => {
                i += 1;
                sync_interval_ms =
                    Some(args[i].parse().unwrap_or_else(|_| {
                        panic!("--sync-interval-ms must be a positive integer")
                    }));
            }
            "--query-timeout-s" => {
                i += 1;
                query_timeout_s =
                    Some(args[i].parse().unwrap_or_else(|_| {
                        panic!("--query-timeout-s must be a positive integer")
                    }));
            }
            "--max-results" => {
                i += 1;
                max_results = Some(
                    args[i]
                        .parse()
                        .unwrap_or_else(|_| panic!("--max-results must be a positive integer")),
                );
            }
            "--max-parallel-queries" => {
                i += 1;
                max_parallel_queries = Some(args[i].parse().unwrap_or_else(|_| {
                    panic!("--max-parallel-queries must be a positive integer")
                }));
            }
            "--fulltext" => {
                fulltext = true;
            }
            "--reasoning" => {
                reasoning = true;
            }
            "--union-default-graph" => {
                union_default_graph = true;
            }
            "--help" | "-h" => {
                println!(
                    "Usage: nova_serve [--file <dataset> [--graph <iri>]]... [--location <dir>] \
                     [--bind 0.0.0.0:3030] [--compact-threshold <n>] [--sync-interval-ms <n>] \
                     [--reasoning] [--fulltext] [--union-default-graph]\n\
                     \n\
                     --file <file> (matching `oxigraph load --file`): bulk-load an RDF file.\n\
                     May be given more than once; each occurrence loads one more file. RDF\n\
                     format is auto-detected from the file extension (.nt, .ttl, .rdf, .nq,\n\
                     .trig, .jsonld, ...). Short form: -f.\n\
                     \n\
                     --graph <iri>: target named graph for the *immediately preceding*\n\
                     --file (QLever-style). Ignored (with a warning) for dataset formats\n\
                     (N-Quads/TriG/JSON-LD), since those carry their own per-quad graph.\n\
                     Omit --graph to load into the default graph. There is no dedicated\n\
                     --ontology flag: load a TBox into its own graph with e.g.\n\
                     `--file ontology.ttl --graph http://example.org/ontology`.\n\
                     \n\
                     --location <dir> (matching `oxigraph serve --location`): persistent\n\
                     WAL-backed LoudsStore rooted at <dir>. Short form: -l.\n\
                     --bind <addr> (matching `oxigraph serve --bind`), default 0.0.0.0:3030.\n\
                     Short form: -b.\n\
                     \n\
                     Without --location: purely in-memory LoudsStore, --file is required.\n\
                     With --location <dir>: persistent WAL-backed LoudsStore rooted at <dir>.\n\
                     If <dir> already has a WAL, it is replayed and --file is ignored.\n\
                     If <dir> is empty/fresh and --file is given, the dataset is\n\
                     bulk-loaded and then persisted (each triple logged to the WAL) for\n\
                     future restarts.\n\
                     \n\
                     --compact-threshold <n>: delta-size threshold (number of live entries)\n\
                     that triggers automatic inline compaction for a persistent store.\n\
                     Default: 1,000,000. Has no effect without --location.\n\
                     \n\
                     --sync-interval-ms <n>: override the default WAL durability policy of\n\
                     `Interval(500ms)` (fsync every 500ms, 'group commit') with a custom\n\
                     interval in milliseconds instead, trading a bounded durability window\n\
                     (writes acknowledged since the last flush can be lost on a crash) for\n\
                     write throughput. Has no effect without --location. See\n\
                     oxigraph_nova_storage_ring::SyncPolicy docs.\n\
                     \n\
                     --query-timeout-s <n>: abort a `/sparql` query that runs longer than\n\
                     <n> seconds, returning 504 Gateway Timeout. Unset by default (no\n\
                     timeout). Matches upstream Oxigraph's `--timeout` flag.\n\
                     \n\
                     --max-results <n>: cap the number of result rows/triples a single\n\
                     `/sparql` query may produce; exceeding it returns 413 Payload Too\n\
                     Large. Unset by default (no cap).\n\
                     \n\
                     --max-parallel-queries <n>: bound the number of `/sparql` query\n\
                     evaluations running concurrently; a request arriving while <n>\n\
                     evaluations are in flight is rejected immediately with 503 Service\n\
                     Unavailable. Unset by default (unbounded).\n\
                     \n\
                     --reasoning: enable OWL 2 RL reasoning (`oxigraph_nova_reasoning::\n\
                     LftjFixpointEngine`). Every `/sparql` query is evaluated over an\n\
                     in-memory closure computed over the union of all graphs, rebuilt\n\
                     lazily whenever the store's data changes. Also exposes `GET\n\
                     /reasoning/diagnostics` and advertises the OWL-RL entailment regime\n\
                     on the SPARQL Service Description (`GET /`).\n\
                     \n\
                     --fulltext: enable Tantivy-backed full-text search\n\
                     (`text:query`/`text:contains` SPARQL extension functions), indexed\n\
                     incrementally on the store's compaction cycle. Requires this binary\n\
                     to have been built with the `fulltext` cargo feature\n\
                     (`cargo run -p oxigraph-nova-server --features fulltext --bin nova_serve`);\n\
                     passing --fulltext without that feature enabled is a hard error.\n\
                     \n\
                     --union-default-graph: server-wide default equivalent to upstream\n\
                     Oxigraph's `oxigraph serve --union-default-graph` — a query with no\n\
                     `FROM`/`FROM NAMED` dataset clause of its own then uses the RDF merge\n\
                     of the default graph and every named graph as its effective default\n\
                     graph, instead of just the store's actual default graph. A query that\n\
                     specifies its own `FROM`/`FROM NAMED` clause is unaffected either way."
                );
                return;
            }

            other => panic!("unknown argument: {other}"),
        }
        i += 1;
    }

    // Pilot cyclic-QWT RingStore path (feature ring-backend). In-memory only.
    if backend == "ring" {
        #[cfg(feature = "ring-backend")]
        {
            run_ring(
                files,
                location,
                bind,
                query_timeout_s,
                max_results,
                max_parallel_queries,
                reasoning,
                union_default_graph,
                fulltext,
            )
            .await;
            return;
        }
        #[cfg(not(feature = "ring-backend"))]
        {
            let _ = (
                files,
                location,
                bind,
                query_timeout_s,
                max_results,
                max_parallel_queries,
                reasoning,
                union_default_graph,
                fulltext,
            );
            panic!(
                "--backend ring requires building with --features ring-backend \
                 (enables cyclic-ring-pilot RingStore). Disk --location is not supported."
            );
        }
    } else if backend != "louds" {
        panic!("unknown --backend {backend:?}; expected louds (default) or ring");
    }

    // ── Construct the store: persistent (--location) or in-memory ──────────
    let store = match &location {

        Some(dir) => {
            eprintln!(
                "[nova_serve] Opening persistent store at {} ...",
                dir.display()
            );
            let store = LoudsStore::open(dir).expect("LoudsStore::open failed");
            if let Some(threshold) = compact_threshold {
                store.set_auto_compact_threshold(threshold);
                eprintln!("[nova_serve] Auto-compact threshold set to {threshold}.");
            }
            if let Some(ms) = sync_interval_ms {
                store.set_sync_policy(SyncPolicy::Interval(Duration::from_millis(ms)));
                eprintln!(
                    "[nova_serve] WAL sync policy set to Interval({ms}ms) — group commit \
                     (bounded durability window; see --help)."
                );
            }

            eprintln!(
                "[nova_serve] Recovered {} triples from WAL.",
                store.triple_count()
            );
            store
        }
        None => LoudsStore::new(),
    };

    // Bulk-load from --file only if the store came up empty (a fresh
    // in-memory store, or a fresh/empty --location directory with no prior
    // WAL history). If a persistent store already has data recovered from
    // its WAL, --file is ignored — the WAL is the source of truth.
    if !files.is_empty() {
        if store.triple_count() > 0 {
            eprintln!(
                "[nova_serve] Store already has {} triples (from --location); ignoring --file.",
                store.triple_count()
            );
        } else {
            // First pass: parse every --file into one combined `Vec<Quad>`
            // (each targeting its own resolved graph), then bulk-load them
            // all in a single `bulk_load()` call/compaction.
            let mut all_quads: Vec<Quad> = Vec::new();
            for fl in &files {
                let fmt = resolve_format(&fl.path);
                if fmt.supports_datasets() && fl.graph.is_some() {
                    eprintln!(
                        "[nova_serve] warning: --graph is ignored for dataset formats \
                         (N-Quads/TriG/JSON-LD) — {}'s own per-quad graph is used instead.",
                        fl.path.display()
                    );
                }
                let target_graph = fl.graph.clone().unwrap_or(GraphName::DefaultGraph);

                eprintln!("[nova_serve] Loading {} ...", fl.path.display());
                let t0 = Instant::now();
                let f = File::open(&fl.path).expect("failed to open dataset file");
                let reader = BufReader::new(f);

                let mut parsed: usize = 0;
                let quads: Vec<Quad> = RdfParser::from_format(fmt)
                    .with_default_graph(target_graph)
                    .for_reader(reader)
                    .map(|result| {
                        let quad =
                            result.unwrap_or_else(|e| panic!("{} parse error: {e}", fmt.name()));
                        parsed += 1;
                        if parsed.is_multiple_of(200_000) {
                            eprintln!("[nova_serve]   ... {parsed} triples parsed");
                        }
                        quad
                    })
                    .collect();
                eprintln!(
                    "[nova_serve]   ... parsed {} in {:.2}s.",
                    quads.len(),
                    t0.elapsed().as_secs_f64()
                );
                all_quads.extend(quads);
            }

            // `bulk_load()` bypasses both the delta `BTreeMap` *and* the WAL
            // entirely: it builds the Ring directly in memory, then commits
            // via `commit_compaction`, which — for a persistent store — does
            // a single atomic snapshot + dictionary + WAL-segment-rotation +
            // MANIFEST commit (see `LoudsStore::commit_compaction`). This
            // matters enormously for `--location --file`: the old path went
            // through `extend()` → `insert()`, WAL-logging **and
            // `fsync`-ing every single triple individually** (~4.2 ms/triple
            // measured — see `benches/external/README.md`'s "Critical
            // caveat" section), making a multi-million-triple bulk load take
            // hours. `bulk_load()` is just as crash-safe (the MANIFEST swap
            // is the single atomic commit point: a crash before it leaves
            // the store exactly as empty as it started; a crash after
            // leaves the fully-loaded snapshot committed — no partial
            // states), so there is no reason to prefer the fsync-per-write
            // path for an initial bulk load, persistent or not.
            let t0 = Instant::now();
            let count = store
                .bulk_load(all_quads.into_iter())
                .expect("bulk_load failed");
            eprintln!(
                "[nova_serve] Loaded + compacted {count} triples in {:.2}s.",
                t0.elapsed().as_secs_f64()
            );

            // Force an eager collection pass right after the build's burst
            // of large transient scratch-buffer allocations — see
            // `oxigraph_nova_server::mimalloc_tuning::mimalloc_collect_now`'s
            // doc comment.
            mimalloc_tuning::mimalloc_collect_now();
        }
    } else if location.is_none() {
        panic!("either --file <dataset> or --location <dir> is required");
    }

    eprintln!("[nova_serve] Ring triple_count={}", store.triple_count());

    // ── Real per-component memory breakdown (diagnostic) ────────────────────
    {
        let mb = store.memory_breakdown();
        let mib = |b: usize| b as f64 / (1024.0 * 1024.0);
        eprintln!("[nova_serve] ── Memory breakdown (measured, not estimated) ──");
        eprintln!(
            "[nova_serve]   Ring (6x LOUDS tries):           {:>10.2} MiB",
            mib(mb.ring_bytes)
        );

        eprintln!(
            "[nova_serve]   Dictionary (terms, Arc-deduped): {:>10.2} MiB",
            mib(mb.dict_bytes)
        );

        eprintln!(
            "[nova_serve]   TOTAL:                           {:>10.2} MiB",
            mib(mb.total_bytes())
        );
        eprintln!(
            "[nova_serve]   Triples: {}  |  Bytes/triple: {:.2}",
            mb.triple_count,
            mb.bytes_per_triple()
        );
        eprintln!("[nova_serve] ─────────────────────────────────────────────");
    }

    // ── Per-ordering (SPO/SOP/PSO/POS/OPS/OSP) breakdown ────────────────────
    {
        let bd = store.per_ordering_breakdown();
        let mib = |b: usize| b as f64 / (1024.0 * 1024.0);
        eprintln!("[nova_serve] ── Per-ordering breakdown ──");
        eprintln!(
            "[nova_serve]   {:<6} {:>10} {:>10} {:>12}",
            "Order", "T (MiB)", "L (MiB)", "Vocab* (MiB)"
        );
        for i in 0..6 {
            let ord = bd.orders[i];
            let b = &bd.per_order[i];
            eprintln!(
                "[nova_serve]   {:<6} {:>10.2} {:>10.2} {:>12.2}",
                format!("{ord:?}"),
                mib(b.t_bytes),
                mib(b.l_bytes),
                mib(bd.vocab_undeduped[i]),
            );
        }

        eprintln!("[nova_serve]   (*Vocab is undeduped per-ordering; deduped total below)");
        eprintln!(
            "[nova_serve]   Vocab (deduped, real total):                   {:>10.2} MiB",
            mib(bd.vocab_deduped_total)
        );
        eprintln!("[nova_serve] ─────────────────────────────────────────────");
    }

    eprintln!("[nova_serve] Ready. Serving on http://{bind}/sparql");

    let store = Arc::new(store);

    // ── Optional full-text search (`--fulltext`) ────────────────────────────
    #[cfg(feature = "fulltext")]
    let text_search: Option<Arc<dyn oxigraph_nova_core::TextSearch>> = if fulltext {
        eprintln!("[nova_serve] Enabling full-text search (text:query/text:contains) ...");
        store
            .enable_fulltext()
            .expect("LoudsStore::enable_fulltext failed");
        Some(Arc::clone(&store) as Arc<dyn oxigraph_nova_core::TextSearch>)
    } else {
        None
    };
    #[cfg(not(feature = "fulltext"))]
    let text_search: Option<Arc<dyn oxigraph_nova_core::TextSearch>> = {
        if fulltext {
            panic!(
                "--fulltext was passed, but this binary was not built with the `fulltext` \
                 cargo feature (rebuild with `--features fulltext`)"
            );
        }
        None
    };

    let mut server = Server::new(store);
    if let Some(secs) = query_timeout_s {
        server = server.with_query_timeout(Duration::from_secs(secs));
    }
    if let Some(n) = max_results {
        server = server.with_max_results(n);
    }
    if let Some(n) = max_parallel_queries {
        server = server.with_max_parallel_queries(n);
    }
    if let Some(ts) = text_search {
        server = server.with_text_search(ts);
    }
    if reasoning {
        eprintln!("[nova_serve] Enabling OWL 2 RL reasoning (LftjFixpointEngine) ...");
        let engine: Arc<dyn ReasoningEngine> = Arc::new(LftjFixpointEngine::new());
        server = server.with_reasoning(engine);
    }
    if union_default_graph {
        eprintln!("[nova_serve] Enabling server-wide union-default-graph ...");
        server = server.with_union_default_graph(true);
    }
    server.run(&bind).await.expect("server failed");
}

/// In-memory RingStore pilot path (`--backend ring`, feature `ring-backend`).
///
/// No WAL / `--location`. No LOUDS memory-breakdown diagnostics. Used by the
/// external mem harness via `NOVA_BACKEND=ring`.
#[cfg(feature = "ring-backend")]
async fn run_ring(
    files: Vec<FileLoad>,
    location: Option<PathBuf>,
    bind: String,
    query_timeout_s: Option<u64>,
    max_results: Option<usize>,
    max_parallel_queries: Option<usize>,
    reasoning: bool,
    union_default_graph: bool,
    fulltext: bool,
) {
    use oxigraph_nova_storage_ring::RingStore;

    if location.is_some() {
        panic!(
            "--backend ring is in-memory only; --location is not supported yet \
             (no WAL on RingStore)"
        );
    }
    if fulltext {
        panic!("--fulltext is not supported with --backend ring");
    }
    if files.is_empty() {
        panic!("--backend ring requires --file <dataset>");
    }

    eprintln!("[nova_serve] Backend: RingStore (cyclic QWT pilot, in-memory)");
    let store = RingStore::new();

    let mut all_quads: Vec<Quad> = Vec::new();
    for fl in &files {
        let fmt = resolve_format(&fl.path);
        if fmt.supports_datasets() && fl.graph.is_some() {
            eprintln!(
                "[nova_serve] warning: --graph is ignored for dataset formats \
                 (N-Quads/TriG/JSON-LD) — {}'s own per-quad graph is used instead.",
                fl.path.display()
            );
        }
        let target_graph = fl.graph.clone().unwrap_or(GraphName::DefaultGraph);
        eprintln!("[nova_serve] Loading {} ...", fl.path.display());
        let t0 = Instant::now();
        let f = File::open(&fl.path).expect("failed to open dataset file");
        let reader = BufReader::new(f);
        let mut parsed: usize = 0;
        let quads: Vec<Quad> = RdfParser::from_format(fmt)
            .with_default_graph(target_graph)
            .for_reader(reader)
            .map(|result| {
                let quad = result.unwrap_or_else(|e| panic!("{} parse error: {e}", fmt.name()));
                parsed += 1;
                if parsed.is_multiple_of(200_000) {
                    eprintln!("[nova_serve]   ... {parsed} triples parsed");
                }
                quad
            })
            .collect();
        eprintln!(
            "[nova_serve]   ... parsed {} in {:.2}s.",
            quads.len(),
            t0.elapsed().as_secs_f64()
        );
        all_quads.extend(quads);
    }

    let t0 = Instant::now();
    let count = store
        .bulk_load(all_quads.into_iter())
        .expect("RingStore::bulk_load failed");
    eprintln!(
        "[nova_serve] Loaded + compacted {count} triples into RingStore in {:.2}s.",
        t0.elapsed().as_secs_f64()
    );
    mimalloc_tuning::mimalloc_collect_now();

    eprintln!(
        "[nova_serve] Ring triple_count={} (ring={})",
        store.triple_count(),
        store.ring_triple_count()
    );
    eprintln!("[nova_serve] Ready. Serving on http://{bind}/sparql");

    let store = Arc::new(store);
    let mut server = Server::new(store);
    if let Some(secs) = query_timeout_s {
        server = server.with_query_timeout(Duration::from_secs(secs));
    }
    if let Some(n) = max_results {
        server = server.with_max_results(n);
    }
    if let Some(n) = max_parallel_queries {
        server = server.with_max_parallel_queries(n);
    }
    if reasoning {
        eprintln!("[nova_serve] Enabling OWL 2 RL reasoning (LftjFixpointEngine) ...");
        let engine: Arc<dyn ReasoningEngine> = Arc::new(LftjFixpointEngine::new());
        server = server.with_reasoning(engine);
    }
    if union_default_graph {
        eprintln!("[nova_serve] Enabling server-wide union-default-graph ...");
        server = server.with_union_default_graph(true);
    }
    server.run(&bind).await.expect("server failed");
}

