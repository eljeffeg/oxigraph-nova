//! `oxigraph` — standalone offline CLI tooling for Oxigraph Nova.
//!
//! Mirrors a subset of `oxigraph-cli`'s subcommands (see
//! `./research/oxigraph/cli`), adapted to Nova's own `RingStore`/`Server`
//! API surface, and shipped under
//! the same binary name (`oxigraph`) so scripts/muscle memory written
//! against upstream `oxigraph-cli` work unchanged. Nine subcommands total:
//!
//! - `oxigraph load --location <dir> [--file <path>]... [--format F] \
//!   [--graph G] [--base IRI]` — bulk-load directly into a persistent
//!   store, without going through HTTP. Accepts multiple `--file`s (parsed
//!   in parallel, merged into a single bulk-load pass) or, if `--file` is
//!   omitted entirely, reads from stdin (`--format` then required).
//! - `oxigraph backup --location <dir> --destination <dir>` — mirrors
//!   `oxigraph backup` 1:1 (see `RingStore::backup`).
//! - `oxigraph query --location <dir> (--query Q | --query-file F) ...` —
//!   run a SPARQL query against a persistent store, offline (no HTTP),
//!   with results format-negotiated the same way as `/sparql`.
//! - `oxigraph update --location <dir> (--update U | --update-file F)` —
//!   run a SPARQL update against a persistent store, offline (no HTTP).
//! - `oxigraph dump --location <dir> [--file F] [--format F] [--graph G]`
//!   — serialize the store's logical RDF content out to a file.
//! - `oxigraph convert [--from-file F] [--to-file F] ...` — a pure
//!   `oxrdfio` reparse/reserialize pipe, no store at all.
//! - `oxigraph optimize --location <dir>` — force storage compaction.
//! - `oxigraph serve-read-only --location <dir> ...` — serve a store with
//!   every write rejected at the HTTP layer (see `cli::Command::ServeReadOnly`'s
//!   doc comment for the exact isolation semantics/caveats).
//! - `oxigraph serve ...` — a thin wrapper around the same store
//!   construction + `Server::run` logic as the standalone `nova_serve`
//!   binary, exposed as a subcommand of this unified tool. `nova_serve`
//!   itself is left unchanged (external benchmark scripts depend on its
//!   exact flags — see `benches/external/run_comparison*.sh`).

mod cli;
mod load;

use clap::Parser;
use cli::{Args, Command};
use mimalloc::MiMalloc;
use oxigraph_nova_core::{GraphName, NamedOrBlankNode, QuadStore, Term};
use oxigraph_nova_query::{Evaluator, QueryResult, StoreDataset, execute_update};
use oxigraph_nova_server::{Server, mimalloc_tuning};
use oxigraph_nova_storage_ring::{RingStore, SyncPolicy};
use oxrdfio::{RdfFormat, RdfParser, RdfSerializer};
use sparesults::{QueryResultsFormat, QueryResultsSerializer};
use spargebra::SparqlParser;
use spargebra::algebra::GraphPattern;
use std::io::Write as _;
use std::sync::Arc;
use std::time::{Duration, Instant};

// See `nova_serve.rs`'s doc comment for the full rationale: mimalloc avoids
// unbounded RSS growth from macOS's default allocator across repeated
// large-result-set queries. Shared here since `oxigraph serve` runs the same
// `Server` under the same workload shape.
#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

fn main() -> anyhow::Result<()> {
    // Must run before any other allocation — see
    // `oxigraph_nova_server::mimalloc_tuning`'s module doc comment (bulk-load/
    // compaction transient-memory fix, shared with `nova_serve`'s binary
    // since `oxigraph load`/`oxigraph serve` also call `bulk_load()`).
    mimalloc_tuning::tune_mimalloc_purge_delay();

    tracing_subscriber::fmt::init();
    let args = Args::parse();

    match args.command {
        Command::Load {
            location,
            file,
            format,
            graph,
            base,
            lenient,
        } => run_load(
            &location,
            &file,
            format.as_deref(),
            graph.as_deref(),
            base.as_deref(),
            lenient,
        ),
        Command::Backup {
            location,
            destination,
        } => run_backup(&location, &destination),
        Command::Query {
            location,
            query,
            query_file,
            results_file,
            results_format,
            union_default_graph,
        } => run_query(
            &location,
            query.as_deref(),
            query_file.as_deref(),
            results_file.as_deref(),
            results_format.as_deref(),
            union_default_graph,
        ),

        Command::Update {
            location,
            update,
            update_file,
        } => run_update(&location, update.as_deref(), update_file.as_deref()),
        Command::Dump {
            location,
            file,
            format,
            graph,
        } => run_dump(
            &location,
            file.as_deref(),
            format.as_deref(),
            graph.as_deref(),
        ),
        Command::Convert {
            from_file,
            from_format,
            to_file,
            to_format,
            from_graph,
            from_default_graph,
            to_graph,
            lenient,
        } => run_convert(
            from_file.as_deref(),
            from_format.as_deref(),
            to_file.as_deref(),
            to_format.as_deref(),
            from_graph.as_deref(),
            from_default_graph,
            to_graph.as_deref(),
            lenient,
        ),
        Command::Optimize { location } => run_optimize(&location),
        Command::ServeReadOnly {
            location,
            bind,
            query_timeout_s,
            max_results,
            max_parallel_queries,
            fulltext,
            union_default_graph,
        } => {
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(run_serve_read_only(
                location,
                bind,
                query_timeout_s,
                max_results,
                max_parallel_queries,
                fulltext,
                union_default_graph,
            ))
        }
        Command::Serve {
            location,
            file,
            bind,
            compact_threshold,
            sync_interval_ms,
            query_timeout_s,
            max_results,
            max_parallel_queries,
            fulltext,
            union_default_graph,
        } => {
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(run_serve(
                location,
                file,
                bind,
                compact_threshold,
                sync_interval_ms,
                query_timeout_s,
                max_results,
                max_parallel_queries,
                fulltext,
                union_default_graph,
            ))
        }
    }
}

fn parse_graph_name(graph: Option<&str>) -> anyhow::Result<Option<GraphName>> {
    Ok(match graph {
        None => None,
        Some(iri) => Some(GraphName::NamedNode(oxrdf::NamedNode::new(iri)?)),
    })
}

fn run_load(
    location: &std::path::Path,
    files: &[std::path::PathBuf],
    format: Option<&str>,
    graph: Option<&str>,
    base: Option<&str>,
    lenient: bool,
) -> anyhow::Result<()> {
    let graph_name = parse_graph_name(graph)?;

    println!(
        "[oxigraph load] Opening persistent store at {} ...",
        location.display()
    );
    let store = RingStore::open(location)?;

    match files {
        [] => println!("[oxigraph load] Reading from stdin ..."),
        [single] => println!("[oxigraph load] Parsing {} ...", single.display()),
        multiple => println!(
            "[oxigraph load] Parsing {} files in parallel ...",
            multiple.len()
        ),
    }
    let t0 = Instant::now();
    let count =
        load::load_sources_into_store(&store, files, format, graph_name.as_ref(), base, lenient)?;
    // See `oxigraph_nova_server::mimalloc_tuning::mimalloc_collect_now`'s doc
    // comment: force an eager purge pass right after the build's transient
    // scratch-buffer allocation burst.
    mimalloc_tuning::mimalloc_collect_now();
    println!(
        "[oxigraph load] Loaded + compacted {count} quads in {:.2}s.",
        t0.elapsed().as_secs_f64()
    );

    println!(
        "[oxigraph load] Store now has {} triples total.",
        store.triple_count()
    );
    Ok(())
}

fn run_backup(location: &std::path::Path, destination: &std::path::Path) -> anyhow::Result<()> {
    println!(
        "[oxigraph backup] Opening store at {} ...",
        location.display()
    );
    let store = RingStore::open(location)?;
    println!(
        "[oxigraph backup] Backing up to {} ...",
        destination.display()
    );
    store.backup(destination)?;
    println!("[oxigraph backup] Done.");
    Ok(())
}

// ── query ──────────────────────────────────────────────────────────────────

/// Extract the ordered SELECT variable list from the outermost `Project`
/// node — duplicated from `nova-server`'s private `query_select_vars`/
/// `extract_project_vars` (kept private there since it's an internal
/// HTTP-serialization helper; small enough to not warrant a shared
/// `nova-query` export just for this one CLI use).
fn query_select_vars(query: &spargebra::Query) -> Vec<oxrdf::Variable> {
    if let spargebra::Query::Select { pattern, .. } = query {
        extract_project_vars(pattern)
    } else {
        vec![]
    }
}

fn extract_project_vars(pattern: &GraphPattern) -> Vec<oxrdf::Variable> {
    match pattern {
        GraphPattern::Project { variables, .. } => variables.clone(),
        GraphPattern::Distinct { inner } => extract_project_vars(inner),
        GraphPattern::Reduced { inner } => extract_project_vars(inner),
        GraphPattern::OrderBy { inner, .. } => extract_project_vars(inner),
        GraphPattern::Slice { inner, .. } => extract_project_vars(inner),
        _ => vec![],
    }
}

/// Resolve `--results-format` (or `results_file`'s extension) to a
/// [`QueryResultsFormat`], defaulting to JSON — used for `Solutions`/
/// `Boolean` query results.
fn resolve_results_format(
    explicit: Option<&str>,
    file: Option<&std::path::Path>,
) -> anyhow::Result<QueryResultsFormat> {
    if let Some(f) = explicit {
        return QueryResultsFormat::from_extension(f)
            .or_else(|| QueryResultsFormat::from_media_type(f))
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "unrecognized results format {f:?}; expected one of: json, xml, csv, tsv \
                     (or the equivalent MIME type)"
                )
            });
    }
    if let Some(ext) = file.and_then(|p| p.extension()).and_then(|e| e.to_str())
        && let Some(fmt) = QueryResultsFormat::from_extension(ext)
    {
        return Ok(fmt);
    }
    Ok(QueryResultsFormat::Json)
}

/// Resolve `--results-format` (or `results_file`'s extension) to an
/// [`RdfFormat`], defaulting to N-Triples — used for `Triples`
/// (CONSTRUCT/DESCRIBE) query results.
fn resolve_triples_format(
    explicit: Option<&str>,
    file: Option<&std::path::Path>,
) -> anyhow::Result<RdfFormat> {
    if let Some(f) = explicit {
        return RdfFormat::from_extension(f)
            .or_else(|| RdfFormat::from_media_type(f))
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "unrecognized RDF format {f:?}; expected one of: nt, ttl, rdf, nq, trig, \
                     jsonld (or the equivalent MIME type)"
                )
            });
    }
    if let Some(ext) = file.and_then(|p| p.extension()).and_then(|e| e.to_str())
        && let Some(fmt) = RdfFormat::from_extension(ext)
    {
        return Ok(fmt);
    }
    Ok(RdfFormat::NTriples)
}

/// Open `results_file` for writing, or fall back to stdout — shared by
/// `run_query`/`run_dump`/`run_convert`.
fn open_output(path: Option<&std::path::Path>) -> anyhow::Result<Box<dyn std::io::Write>> {
    Ok(match path {
        Some(p) => Box::new(std::io::BufWriter::new(std::fs::File::create(p)?)),
        None => Box::new(std::io::BufWriter::new(std::io::stdout())),
    })
}

fn run_query(
    location: &std::path::Path,
    query: Option<&str>,
    query_file: Option<&std::path::Path>,
    results_file: Option<&std::path::Path>,
    results_format: Option<&str>,
    union_default_graph: bool,
) -> anyhow::Result<()> {
    let query_text = match (query, query_file) {
        (Some(q), None) => q.to_string(),
        (None, Some(f)) => std::fs::read_to_string(f)
            .map_err(|e| anyhow::anyhow!("failed to read {}: {e}", f.display()))?,
        (None, None) => anyhow::bail!("one of --query / --query-file is required"),
        (Some(_), Some(_)) => unreachable!("clap enforces --query/--query-file mutual exclusion"),
    };

    eprintln!(
        "[oxigraph query] Opening persistent store at {} ...",
        location.display()
    );
    let store = Arc::new(RingStore::open(location)?);

    let parsed = SparqlParser::new().parse_query(&query_text)?;
    let dataset = StoreDataset::new(Arc::clone(&store));
    let options =
        oxigraph_nova_query::QueryOptions::default().with_union_default_graph(union_default_graph);
    let evaluator = Evaluator::with_options(&dataset, options);
    let result = evaluator.evaluate(&parsed)?;

    // Serialize directly into the destination writer (file or stdout) rather
    // than building the whole serialized output in an intermediate `Vec<u8>`
    // first — for large result sets the serialized text can be considerably
    // bigger than Nova's in-memory representation, so buffering it wholesale
    // would work against the low-RSS design goal documented throughout this
    // workspace.
    let mut out = open_output(results_file)?;
    match result {
        QueryResult::Boolean(b) => {
            let fmt = resolve_results_format(results_format, results_file)?;
            QueryResultsSerializer::from_format(fmt).serialize_boolean_to_writer(&mut out, b)?;
        }
        QueryResult::Solutions { stream, .. } => {
            let fmt = resolve_results_format(results_format, results_file)?;
            let variables = query_select_vars(&parsed);
            let mut ser = QueryResultsSerializer::from_format(fmt)
                .serialize_solutions_to_writer(&mut out, variables.clone())?;
            for sol in stream {
                let sol = sol?;
                ser.serialize(
                    variables
                        .iter()
                        .filter_map(|v| sol.get(v).map(|t| (v.as_ref(), t.as_ref()))),
                )?;
            }
            ser.finish()?;
        }
        QueryResult::Triples(stream) => {
            let fmt = resolve_triples_format(results_format, results_file)?;
            let mut ser = RdfSerializer::from_format(fmt).for_writer(&mut out);
            for t in stream {
                let t = t?;
                ser.serialize_triple(&t)?;
            }
            ser.finish()?;
        }
    }
    out.flush()?;
    eprintln!("[oxigraph query] Done.");
    Ok(())
}

// ── update ─────────────────────────────────────────────────────────────────

fn run_update(
    location: &std::path::Path,
    update: Option<&str>,
    update_file: Option<&std::path::Path>,
) -> anyhow::Result<()> {
    let update_text = match (update, update_file) {
        (Some(u), None) => u.to_string(),
        (None, Some(f)) => std::fs::read_to_string(f)
            .map_err(|e| anyhow::anyhow!("failed to read {}: {e}", f.display()))?,
        (None, None) => anyhow::bail!("one of --update / --update-file is required"),
        (Some(_), Some(_)) => {
            unreachable!("clap enforces --update/--update-file mutual exclusion")
        }
    };

    println!(
        "[oxigraph update] Opening persistent store at {} ...",
        location.display()
    );
    let store = Arc::new(RingStore::open(location)?);

    let parsed = SparqlParser::new().parse_update(&update_text)?;
    execute_update(&store, &parsed)?;

    println!("[oxigraph update] Done.");
    Ok(())
}

// ── dump ───────────────────────────────────────────────────────────────────

fn run_dump(
    location: &std::path::Path,
    file: Option<&std::path::Path>,
    format: Option<&str>,
    graph: Option<&str>,
) -> anyhow::Result<()> {
    let graph_name = parse_graph_name(graph)?;

    eprintln!(
        "[oxigraph dump] Opening persistent store at {} ...",
        location.display()
    );
    let store = RingStore::open(location)?;

    let fmt = load::resolve_format_opt(format, file)?;

    if graph_name.is_none() && !fmt.supports_datasets() {
        anyhow::bail!(
            "no --graph given (dumping every graph), but {} is a plain triple format; pass \
             --graph to pick one graph, or choose a dataset format (nq/trig/jsonld) via --format",
            fmt.name()
        );
    }

    // Serialize directly into the destination writer (file or stdout)
    // rather than buffering the whole serialized output in an intermediate
    // `Vec<u8>` first, and iterate each graph's quads straight off the
    // `quads_for_pattern` iterator rather than collecting into a `Vec`
    // first — for large stores/graphs, holding either the whole quad set or
    // the whole serialized text in memory at once would work against Nova's
    // low-RSS design goal.
    let mut out = open_output(file)?;
    let mut writer = RdfSerializer::from_format(fmt).for_writer(&mut out);

    match &graph_name {
        Some(g) => {
            for sq in store.quads_for_pattern(None, None, None, Some(g))? {
                let sq = sq?;
                let subject = match sq.subject.as_ref() {
                    Term::NamedNode(n) => NamedOrBlankNode::NamedNode(n.clone()),
                    Term::BlankNode(b) => NamedOrBlankNode::BlankNode(b.clone()),
                    // RDF-1.2 quoted-triple subjects can't be represented by
                    // `oxrdf`'s `Triple`/`Quad` (no `Term::Triple` subject
                    // variant) — see `Command::Dump`'s doc comment in
                    // `cli.rs` for the full explanation.
                    _ => continue,
                };
                let object = sq.object.as_ref().clone();
                writer.serialize_triple(oxrdf::TripleRef::new(&subject, &sq.predicate, &object))?;
            }
        }
        None => {
            // Dataset format, no --graph filter: dump every known graph
            // (default + named), preserving each quad's own graph.
            let mut graphs: Vec<GraphName> = vec![GraphName::DefaultGraph];
            graphs.extend(store.known_named_graphs()?.collect::<Result<Vec<_>, _>>()?);
            for g in &graphs {
                for sq in store.quads_for_pattern(None, None, None, Some(g))? {
                    let sq = sq?;
                    let subject = match sq.subject.as_ref() {
                        Term::NamedNode(n) => NamedOrBlankNode::NamedNode(n.clone()),
                        Term::BlankNode(b) => NamedOrBlankNode::BlankNode(b.clone()),
                        _ => continue,
                    };
                    let object = sq.object.as_ref().clone();
                    writer.serialize_quad(oxrdf::QuadRef::new(
                        &subject,
                        &sq.predicate,
                        &object,
                        g,
                    ))?;
                }
            }
        }
    }

    writer.finish()?;
    out.flush()?;
    eprintln!("[oxigraph dump] Done.");
    Ok(())
}

// ── convert ────────────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
fn run_convert(
    from_file: Option<&std::path::Path>,
    from_format: Option<&str>,
    to_file: Option<&std::path::Path>,
    to_format: Option<&str>,
    from_graph: Option<&str>,
    from_default_graph: bool,
    to_graph: Option<&str>,
    lenient: bool,
) -> anyhow::Result<()> {
    let from_fmt = load::resolve_format_opt(from_format, from_file)?;
    let to_fmt = load::resolve_format_opt(to_format, to_file)?;
    let from_graph_name = parse_graph_name(from_graph)?;
    let to_graph_name = parse_graph_name(to_graph)?;

    let mut from_parser = RdfParser::from_format(from_fmt);
    if lenient {
        from_parser = from_parser.lenient();
    }
    let quads: Box<dyn Iterator<Item = Result<oxrdf::Quad, oxrdfio::RdfParseError>>> =
        match from_file {
            Some(f) => {
                let reader = std::io::BufReader::new(std::fs::File::open(f)?);
                Box::new(from_parser.for_reader(reader))
            }
            None => {
                let reader = std::io::BufReader::new(std::io::stdin());
                Box::new(from_parser.for_reader(reader))
            }
        };

    // Serialize directly into the destination writer (file or stdout)
    // rather than buffering the whole serialized output in an intermediate
    // `Vec<u8>` first — `convert` is meant to be a streaming reparse/
    // reserialize pipe (it already reads `quads` lazily off the parser
    // iterator), so buffering the output wholesale would defeat that.
    let mut out = open_output(to_file)?;
    let mut writer = RdfSerializer::from_format(to_fmt).for_writer(&mut out);
    let mut count = 0usize;
    for quad in quads {
        let mut quad = quad?;

        // Apply `--from-graph`/`--from-default-graph` filter.
        if from_default_graph && !matches!(quad.graph_name, GraphName::DefaultGraph) {
            continue;
        }
        if let Some(g) = &from_graph_name
            && &quad.graph_name != g
        {
            continue;
        }
        // Post-filter, remap the quad's graph to the default graph (the
        // filter selected exactly one graph's worth of quads).
        if from_default_graph || from_graph_name.is_some() {
            quad.graph_name = GraphName::DefaultGraph;
        }
        // Apply `--to-graph` remap: only meaningful for quads currently in
        // the default graph post-filter (i.e. either no `--from-graph`/
        // `--from-default-graph` was given and the quad was already in the
        // default graph, or the filter above just remapped it there) — see
        // `Command::Convert`'s doc comment in `cli.rs` for the full
        // semantics.
        if let Some(g) = &to_graph_name
            && matches!(quad.graph_name, GraphName::DefaultGraph)
        {
            quad.graph_name = g.clone();
        }

        writer.serialize_quad(&quad)?;
        count += 1;
    }

    writer.finish()?;
    out.flush()?;
    eprintln!("[oxigraph convert] Converted {count} quads.");
    Ok(())
}

// ── optimize ───────────────────────────────────────────────────────────────

fn run_optimize(location: &std::path::Path) -> anyhow::Result<()> {
    println!(
        "[oxigraph optimize] Opening persistent store at {} ...",
        location.display()
    );
    let store = RingStore::open(location)?;
    println!("[oxigraph optimize] Compacting ...");
    let t0 = Instant::now();
    store.compact()?;
    mimalloc_tuning::mimalloc_collect_now();
    println!(
        "[oxigraph optimize] Done in {:.2}s ({} triples).",
        t0.elapsed().as_secs_f64(),
        store.triple_count()
    );
    Ok(())
}

// ── serve-read-only ─────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
async fn run_serve_read_only(
    location: std::path::PathBuf,
    bind: String,
    query_timeout_s: Option<u64>,
    max_results: Option<usize>,
    max_parallel_queries: Option<usize>,
    fulltext: bool,
    union_default_graph: bool,
) -> anyhow::Result<()> {
    println!(
        "[oxigraph serve-read-only] Opening persistent store at {} ...",
        location.display()
    );
    let store = RingStore::open(&location)?;
    println!(
        "[oxigraph serve-read-only] Recovered {} triples from WAL.",
        store.triple_count()
    );

    let store = Arc::new(store);

    #[cfg(feature = "fulltext")]
    let text_search: Option<Arc<dyn oxigraph_nova_core::TextSearch>> = if fulltext {
        println!(
            "[oxigraph serve-read-only] Enabling full-text search (text:query/text:contains) ..."
        );
        store.enable_fulltext()?;
        Some(Arc::clone(&store) as Arc<dyn oxigraph_nova_core::TextSearch>)
    } else {
        None
    };
    #[cfg(not(feature = "fulltext"))]
    let text_search: Option<Arc<dyn oxigraph_nova_core::TextSearch>> = {
        if fulltext {
            anyhow::bail!(
                "--fulltext was passed, but this binary was not built with the `fulltext` \
                 cargo feature (rebuild with `--features fulltext`)"
            );
        }
        None
    };

    let mut server = Server::new(store).read_only(true);
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
    if union_default_graph {
        server = server.with_union_default_graph(true);
    }

    println!("[oxigraph serve-read-only] Ready (read-only). Serving on http://{bind}/sparql");
    server.run(&bind).await?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run_serve(
    location: Option<std::path::PathBuf>,
    file: Option<std::path::PathBuf>,
    bind: String,
    compact_threshold: Option<usize>,
    sync_interval_ms: Option<u64>,
    query_timeout_s: Option<u64>,
    max_results: Option<usize>,
    max_parallel_queries: Option<usize>,
    fulltext: bool,
    union_default_graph: bool,
) -> anyhow::Result<()> {
    let store = match &location {
        Some(dir) => {
            println!(
                "[oxigraph serve] Opening persistent store at {} ...",
                dir.display()
            );
            let store = RingStore::open(dir)?;
            if let Some(threshold) = compact_threshold {
                store.set_auto_compact_threshold(threshold);
                println!("[oxigraph serve] Auto-compact threshold set to {threshold}.");
            }
            if let Some(ms) = sync_interval_ms {
                store.set_sync_policy(SyncPolicy::Interval(Duration::from_millis(ms)));
                println!("[oxigraph serve] WAL sync policy set to Interval({ms}ms).");
            }
            println!(
                "[oxigraph serve] Recovered {} triples from WAL.",
                store.triple_count()
            );
            store
        }
        None => RingStore::new(),
    };

    if let Some(file) = &file {
        if store.triple_count() > 0 {
            println!(
                "[oxigraph serve] Store already has {} triples (from --location); ignoring --file.",
                store.triple_count()
            );
        } else {
            println!("[oxigraph serve] Loading {} ...", file.display());
            let t0 = Instant::now();
            let count = load::load_sources_into_store(
                &store,
                std::slice::from_ref(file),
                None,
                None,
                None,
                false,
            )?;
            // See `oxigraph_nova_server::mimalloc_tuning::mimalloc_collect_now`'s
            // doc comment.
            mimalloc_tuning::mimalloc_collect_now();
            println!(
                "[oxigraph serve] Loaded + compacted {count} triples in {:.2}s.",
                t0.elapsed().as_secs_f64()
            );
        }
    } else if location.is_none() {
        anyhow::bail!("either --file <dataset> or --location <dir> is required");
    }

    println!(
        "[oxigraph serve] Ready. Serving on http://{bind}/sparql (triple_count={})",
        store.triple_count()
    );

    let store = Arc::new(store);

    // ── Optional full-text search (`--fulltext`) ────────────────────────────
    #[cfg(feature = "fulltext")]
    let text_search: Option<Arc<dyn oxigraph_nova_core::TextSearch>> = if fulltext {
        println!("[oxigraph serve] Enabling full-text search (text:query/text:contains) ...");
        store.enable_fulltext()?;
        Some(Arc::clone(&store) as Arc<dyn oxigraph_nova_core::TextSearch>)
    } else {
        None
    };
    #[cfg(not(feature = "fulltext"))]
    let text_search: Option<Arc<dyn oxigraph_nova_core::TextSearch>> = {
        if fulltext {
            anyhow::bail!(
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
    if union_default_graph {
        server = server.with_union_default_graph(true);
    }
    server.run(&bind).await?;
    Ok(())
}
