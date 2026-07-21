//! `oxigraph` — standalone offline CLI tooling for Oxigraph Nova.
//!
//! Mirrors a subset of upstream `oxigraph-cli`'s subcommands, adapted to
//! Nova's own `LoudsStore`/`Server` API surface, and shipped under the same
//! binary name (`oxigraph`) so scripts/muscle memory written against upstream
//! `oxigraph-cli` work unchanged. Nine subcommands total:
//!
//! - `oxigraph load --location <dir> [--file <path>]... [--format F] \
//!   [--graph G] [--base IRI]` — bulk-load directly into a persistent
//!   store, without going through HTTP. Accepts multiple `--file`s (parsed
//!   in parallel, merged into a single bulk-load pass) or, if `--file` is
//!   omitted entirely, reads from stdin (`--format` then required).
//! - `oxigraph backup --location <dir> --destination <dir>` — mirrors
//!   `oxigraph backup` 1:1 (see `LoudsStore::backup`).
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
use cli::{Args, Command, CypherCommand, McpCommand};
use mimalloc::MiMalloc;
// All product surfaces (including serve / serve-read-only) use the registry only.
use oxigraph_nova_core::{
    GraphName, NamedOrBlankNode, QuadStore, StorageEngineExt, SyncPolicy, Term, new_backend,
    open_backend, require_backend as core_require_backend,
};
// Force-link the storage crate so inventory BackendFactory registrations
// (LOUDS always; Ring when cyclic-ring is on via ring-backend) are present.
use oxigraph_nova_engine_ring as _;
use oxigraph_nova_query::{Evaluator, QueryResult, StoreDataset, execute_update};
use oxigraph_nova_server::{Server, mimalloc_tuning};
use oxigraph_nova_shacl::{NativeValidator, ShaclValidator};
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
            backend,
            file,
            format,
            graph,
            base,
            lenient,
        } => run_load(
            &location,
            &backend,
            &file,
            format.as_deref(),
            graph.as_deref(),
            base.as_deref(),
            lenient,
        ),
        Command::Backup {
            location,
            backend,
            destination,
        } => run_backup(&location, &backend, &destination),
        Command::Query {
            location,
            backend,
            query,
            query_file,
            results_file,
            results_format,
            union_default_graph,
        } => run_query(
            &location,
            &backend,
            query.as_deref(),
            query_file.as_deref(),
            results_file.as_deref(),
            results_format.as_deref(),
            union_default_graph,
        ),

        Command::Update {
            location,
            backend,
            update,
            update_file,
        } => run_update(
            &location,
            &backend,
            update.as_deref(),
            update_file.as_deref(),
        ),
        Command::Dump {
            location,
            backend,
            file,
            format,
            graph,
        } => run_dump(
            &location,
            &backend,
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
        Command::Optimize { location, backend } => run_optimize(&location, &backend),
        Command::Validate {
            location,
            backend,
            shapes,
            shapes_format,
            results_file,
        } => run_validate(
            &location,
            &backend,
            &shapes,
            shapes_format.as_deref(),
            results_file.as_deref(),
        ),

        Command::ServeReadOnly {
            location,
            backend,
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
                backend,
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
            backend,
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
                backend,
                compact_threshold,
                sync_interval_ms,
                query_timeout_s,
                max_results,
                max_parallel_queries,
                fulltext,
                union_default_graph,
            ))
        }

        Command::Mcp { command } => match command {
            McpCommand::Serve {
                location,
                backend,
                reasoning,
                fulltext,
                max_results,
            } => {
                let rt = tokio::runtime::Runtime::new()?;
                rt.block_on(run_mcp_serve(
                    location,
                    backend,
                    reasoning,
                    fulltext,
                    max_results,
                ))
            }
        },
        Command::Cypher { command } => match command {
            CypherCommand::Query {
                location,
                backend,
                query,
                query_file,
                results_file,
                results_format,
            } => run_cypher_query(
                &location,
                &backend,
                query.as_deref(),
                query_file.as_deref(),
                results_file.as_deref(),
                results_format.as_deref(),
            ),
            CypherCommand::Update {
                location,
                backend,
                update,
                update_file,
            } => run_cypher_update(
                &location,
                &backend,
                update.as_deref(),
                update_file.as_deref(),
            ),
        },
    }
}

/// Parse `--backend` for offline CLI commands via the self-registering
/// storage registry. Unknown names list every linked backend.
fn require_backend(backend: &str) -> anyhow::Result<&'static str> {
    core_require_backend(backend).map_err(|e| anyhow::anyhow!("{e}"))
}

fn parse_graph_name(graph: Option<&str>) -> anyhow::Result<Option<GraphName>> {
    Ok(match graph {
        None => None,
        Some(iri) => Some(GraphName::NamedNode(oxrdf::NamedNode::new(iri)?)),
    })
}

fn run_load(
    location: &std::path::Path,
    backend: &str,
    files: &[std::path::PathBuf],
    format: Option<&str>,
    graph: Option<&str>,
    base: Option<&str>,
    lenient: bool,
) -> anyhow::Result<()> {
    let backend = require_backend(backend)?;
    let graph_name = parse_graph_name(graph)?;

    println!(
        "[oxigraph load] Opening persistent {backend} store at {} ...",
        location.display()
    );

    match files {
        [] => println!("[oxigraph load] Reading from stdin ..."),
        [single] => println!("[oxigraph load] Parsing {} ...", single.display()),
        multiple => println!(
            "[oxigraph load] Parsing {} files in parallel ...",
            multiple.len()
        ),
    }
    let t0 = Instant::now();

    let store = open_backend(backend, location).map_err(|e| anyhow::anyhow!("{e}"))?;
    let count = load::load_sources_into_store(
        |it| store.bulk_load(it),
        files,
        format,
        graph_name.as_ref(),
        base,
        lenient,
    )?;
    mimalloc_tuning::mimalloc_collect_now();
    println!(
        "[oxigraph load] Loaded + compacted {count} quads in {:.2}s.",
        t0.elapsed().as_secs_f64()
    );
    let triple_count = store.triple_count();

    println!("[oxigraph load] Store now has {triple_count} triples total.");
    Ok(())
}

fn run_backup(
    location: &std::path::Path,
    backend: &str,
    destination: &std::path::Path,
) -> anyhow::Result<()> {
    let backend = require_backend(backend)?;
    println!(
        "[oxigraph backup] Opening {backend} store at {} ...",
        location.display()
    );
    println!(
        "[oxigraph backup] Backing up to {} ...",
        destination.display()
    );
    let store = open_backend(backend, location).map_err(|e| anyhow::anyhow!("{e}"))?;
    store
        .backup(destination)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
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

fn write_query_result(
    result: QueryResult,
    parsed: &spargebra::Query,
    results_file: Option<&std::path::Path>,
    results_format: Option<&str>,
) -> anyhow::Result<()> {
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
            let variables = query_select_vars(parsed);
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
    Ok(())
}

fn run_query(
    location: &std::path::Path,
    backend: &str,
    query: Option<&str>,
    query_file: Option<&std::path::Path>,
    results_file: Option<&std::path::Path>,
    results_format: Option<&str>,
    union_default_graph: bool,
) -> anyhow::Result<()> {
    let backend = require_backend(backend)?;
    let query_text = match (query, query_file) {
        (Some(q), None) => q.to_string(),
        (None, Some(f)) => std::fs::read_to_string(f)
            .map_err(|e| anyhow::anyhow!("failed to read {}: {e}", f.display()))?,
        (None, None) => anyhow::bail!("one of --query / --query-file is required"),
        (Some(_), Some(_)) => unreachable!("clap enforces --query/--query-file mutual exclusion"),
    };

    eprintln!(
        "[oxigraph query] Opening persistent {backend} store at {} ...",
        location.display()
    );
    let parsed = SparqlParser::new().parse_query(&query_text)?;
    let options =
        oxigraph_nova_query::QueryOptions::default().with_union_default_graph(union_default_graph);

    let store = open_backend(backend, location).map_err(|e| anyhow::anyhow!("{e}"))?;
    let dataset = StoreDataset::new(store);
    let result = Evaluator::with_options(&dataset, options).evaluate(&parsed)?;

    write_query_result(result, &parsed, results_file, results_format)?;
    eprintln!("[oxigraph query] Done.");
    Ok(())
}

// ── update ─────────────────────────────────────────────────────────────────

fn run_update(
    location: &std::path::Path,
    backend: &str,
    update: Option<&str>,
    update_file: Option<&std::path::Path>,
) -> anyhow::Result<()> {
    let backend = require_backend(backend)?;
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
        "[oxigraph update] Opening persistent {backend} store at {} ...",
        location.display()
    );
    let parsed = SparqlParser::new().parse_update(&update_text)?;
    let store = open_backend(backend, location).map_err(|e| anyhow::anyhow!("{e}"))?;
    execute_update(&store, &parsed)?;

    println!("[oxigraph update] Done.");
    Ok(())
}

// ── cypher query ───────────────────────────────────────────────────────────

fn run_cypher_query(
    location: &std::path::Path,
    backend: &str,
    query: Option<&str>,
    query_file: Option<&std::path::Path>,
    results_file: Option<&std::path::Path>,
    results_format: Option<&str>,
) -> anyhow::Result<()> {
    let backend = require_backend(backend)?;
    let query_text = match (query, query_file) {
        (Some(q), None) => q.to_string(),
        (None, Some(f)) => std::fs::read_to_string(f)
            .map_err(|e| anyhow::anyhow!("failed to read {}: {e}", f.display()))?,
        (None, None) => anyhow::bail!("one of --query / --query-file is required"),
        (Some(_), Some(_)) => unreachable!("clap enforces --query/--query-file mutual exclusion"),
    };

    eprintln!(
        "[oxigraph cypher query] Opening persistent {backend} store at {} ...",
        location.display()
    );
    let parsed = oxigraph_nova_cypher::parse_and_lower(&query_text)?;
    let options = oxigraph_nova_query::QueryOptions::default();
    let store = open_backend(backend, location).map_err(|e| anyhow::anyhow!("{e}"))?;
    let dataset = StoreDataset::new(store);
    let result = Evaluator::with_options(&dataset, options).evaluate(&parsed)?;

    write_query_result(result, &parsed, results_file, results_format)?;
    eprintln!("[oxigraph cypher query] Done.");
    Ok(())
}

// ── cypher update ──────────────────────────────────────────────────────────

fn run_cypher_update(
    location: &std::path::Path,
    backend: &str,
    update: Option<&str>,
    update_file: Option<&std::path::Path>,
) -> anyhow::Result<()> {
    let backend = require_backend(backend)?;
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
        "[oxigraph cypher update] Opening persistent {backend} store at {} ...",
        location.display()
    );
    let parsed = oxigraph_nova_cypher::parse_and_lower_update(&update_text)?;
    let store = open_backend(backend, location).map_err(|e| anyhow::anyhow!("{e}"))?;
    execute_update(&store, &parsed)?;

    println!("[oxigraph cypher update] Done.");
    Ok(())
}

// ── dump ───────────────────────────────────────────────────────────────────

fn run_dump(
    location: &std::path::Path,
    backend: &str,
    file: Option<&std::path::Path>,
    format: Option<&str>,
    graph: Option<&str>,
) -> anyhow::Result<()> {
    let backend = require_backend(backend)?;
    let graph_name = parse_graph_name(graph)?;

    eprintln!(
        "[oxigraph dump] Opening persistent {backend} store at {} ...",
        location.display()
    );

    let fmt = load::resolve_format_opt(format, file)?;

    if graph_name.is_none() && !fmt.supports_datasets() {
        anyhow::bail!(
            "no --graph given (dumping every graph), but {} is a plain triple format; pass \
             --graph to pick one graph, or choose a dataset format (nq/trig/jsonld) via --format",
            fmt.name()
        );
    }

    let store = open_backend(backend, location).map_err(|e| anyhow::anyhow!("{e}"))?;
    dump_store(store.as_ref(), file, fmt, graph_name.as_ref())?;
    eprintln!("[oxigraph dump] Done.");
    Ok(())
}

fn dump_store(
    store: &(impl QuadStore + ?Sized),
    file: Option<&std::path::Path>,
    fmt: RdfFormat,
    graph_name: Option<&GraphName>,
) -> anyhow::Result<()> {
    // Serialize directly into the destination writer (file or stdout)
    // rather than buffering the whole serialized output in an intermediate
    // `Vec<u8>` first, and iterate each graph's quads straight off the
    // `quads_for_pattern` iterator rather than collecting into a `Vec`
    // first — for large stores/graphs, holding either the whole quad set or
    // the whole serialized text in memory at once would work against Nova's
    // low-RSS design goal.
    let mut out = open_output(file)?;
    let mut writer = RdfSerializer::from_format(fmt).for_writer(&mut out);

    match graph_name {
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

fn run_optimize(location: &std::path::Path, backend: &str) -> anyhow::Result<()> {
    let backend = require_backend(backend)?;
    println!(
        "[oxigraph optimize] Opening persistent {backend} store at {} ...",
        location.display()
    );
    println!("[oxigraph optimize] Compacting ...");
    let t0 = Instant::now();
    let store = open_backend(backend, location).map_err(|e| anyhow::anyhow!("{e}"))?;
    store.compact().map_err(|e| anyhow::anyhow!("{e}"))?;
    let triple_count = store.triple_count();
    mimalloc_tuning::mimalloc_collect_now();
    println!(
        "[oxigraph optimize] Done in {:.2}s ({triple_count} triples).",
        t0.elapsed().as_secs_f64(),
    );
    Ok(())
}

// ── serve-read-only ─────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
async fn run_serve_read_only(
    location: std::path::PathBuf,
    backend: String,
    bind: String,
    query_timeout_s: Option<u64>,
    max_results: Option<usize>,
    max_parallel_queries: Option<usize>,
    fulltext: bool,
    union_default_graph: bool,
) -> anyhow::Result<()> {
    let backend = require_backend(&backend)?;
    println!(
        "[oxigraph serve-read-only] Opening persistent {backend} store at {} ...",
        location.display()
    );

    let store = open_backend(backend, &location).map_err(|e| anyhow::anyhow!("{e}"))?;
    println!(
        "[oxigraph serve-read-only] Recovered {} triples from WAL/snapshot.",
        store.triple_count()
    );

    #[cfg(feature = "fulltext")]
    let text_search: Option<Arc<dyn oxigraph_nova_core::TextSearch>> = if fulltext {
        println!(
            "[oxigraph serve-read-only] Enabling full-text search (text:query/text:contains) ..."
        );
        store
            .enable_fulltext()
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        store.clone().as_text_search()
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

    println!(
        "[oxigraph serve-read-only] Ready (read-only, backend={backend}). Serving on http://{bind}/sparql"
    );
    server.run(&bind).await?;
    Ok(())
}

// ── serve ───────────────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
async fn run_serve(
    location: Option<std::path::PathBuf>,
    file: Option<std::path::PathBuf>,
    bind: String,
    backend: String,
    compact_threshold: Option<usize>,
    sync_interval_ms: Option<u64>,
    query_timeout_s: Option<u64>,
    max_results: Option<usize>,
    max_parallel_queries: Option<usize>,
    fulltext: bool,
    union_default_graph: bool,
) -> anyhow::Result<()> {
    let backend = require_backend(&backend)?;

    let store = match &location {
        Some(dir) => {
            println!(
                "[oxigraph serve] Opening persistent {backend} store at {} ...",
                dir.display()
            );
            let store = open_backend(backend, dir).map_err(|e| anyhow::anyhow!("{e}"))?;
            if let Some(threshold) = compact_threshold {
                store.set_auto_compact_threshold(threshold);
                println!("[oxigraph serve] Auto-compact threshold set to {threshold}.");
            }
            if let Some(ms) = sync_interval_ms {
                store.set_sync_policy(SyncPolicy::Interval(Duration::from_millis(ms)));
                println!("[oxigraph serve] WAL sync policy set to Interval({ms}ms).");
            }
            println!(
                "[oxigraph serve] Recovered {} triples from WAL/snapshot.",
                store.triple_count()
            );
            store
        }
        None => {
            println!("[oxigraph serve] Backend: {backend} (in-memory)");
            new_backend(backend).map_err(|e| anyhow::anyhow!("{e}"))?
        }
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
                |it| store.bulk_load(it),
                std::slice::from_ref(file),
                None,
                None,
                None,
                false,
            )?;
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
        "[oxigraph serve] Ready. Serving on http://{bind}/sparql (backend={backend}, triple_count={})",
        store.triple_count()
    );

    #[cfg(feature = "fulltext")]
    let text_search: Option<Arc<dyn oxigraph_nova_core::TextSearch>> = if fulltext {
        println!("[oxigraph serve] Enabling full-text search (text:query/text:contains) ...");
        store
            .enable_fulltext()
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        store.clone().as_text_search()
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

// ── validate ───────────────────────────────────────────────────────────────

/// Format one [`oxigraph_nova_shacl::ValidationResult`] as a human-readable
/// multi-line block, written to `out`.
fn write_validation_result(
    out: &mut dyn std::io::Write,
    index: usize,
    result: &oxigraph_nova_shacl::ValidationResult,
) -> anyhow::Result<()> {
    let severity = match result.severity {
        oxigraph_nova_shacl::Severity::Violation => "Violation",
        oxigraph_nova_shacl::Severity::Warning => "Warning",
        oxigraph_nova_shacl::Severity::Info => "Info",
    };
    writeln!(out, "[{index}] {severity}: {}", result.message)?;
    writeln!(out, "    focus node:  {}", result.focus_node)?;
    if let Some(path) = &result.path {
        writeln!(out, "    path:        {path}")?;
    }
    writeln!(out, "    source shape: {}", result.source_shape)?;
    writeln!(
        out,
        "    constraint:  {}",
        result.source_constraint_component
    )?;
    if let Some(value) = &result.value {
        writeln!(out, "    value:       {value}")?;
    }
    Ok(())
}

fn run_validate(
    location: &std::path::Path,
    backend: &str,
    shapes_path: &std::path::Path,
    shapes_format: Option<&str>,
    results_file: Option<&std::path::Path>,
) -> anyhow::Result<()> {
    let backend = require_backend(backend)?;
    eprintln!(
        "[oxigraph validate] Opening persistent {backend} store at {} ...",
        location.display()
    );

    let fmt = load::resolve_format_opt(shapes_format, Some(shapes_path))?;
    eprintln!(
        "[oxigraph validate] Parsing shapes graph {} ({}) ...",
        shapes_path.display(),
        fmt.name()
    );
    let reader = std::io::BufReader::new(
        std::fs::File::open(shapes_path)
            .map_err(|e| anyhow::anyhow!("failed to open {}: {e}", shapes_path.display()))?,
    );
    let shapes: Vec<oxrdf::Quad> = RdfParser::from_format(fmt)
        .with_default_graph(GraphName::DefaultGraph)
        .for_reader(reader)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| anyhow::anyhow!("{} parse error: {e}", fmt.name()))?;

    eprintln!("[oxigraph validate] Validating ...");
    let store = open_backend(backend, location).map_err(|e| anyhow::anyhow!("{e}"))?;
    let dataset = StoreDataset::new(store);
    let report = NativeValidator::new().validate(&shapes, &dataset)?;

    let mut out = open_output(results_file)?;
    if report.conforms {
        writeln!(out, "CONFORMS")?;
    } else {
        writeln!(
            out,
            "DOES NOT CONFORM ({} violation(s), {} warning(s))",
            report.violation_count(),
            report.warning_count()
        )?;
        for (i, result) in report.results.iter().enumerate() {
            write_validation_result(&mut out, i + 1, result)?;
        }
    }
    out.flush()?;

    eprintln!("[oxigraph validate] Done.");
    if !report.conforms {
        std::process::exit(1);
    }
    Ok(())
}

// ── mcp serve ────────────────────────────────────────────────────────────────

/// `oxigraph mcp serve` — construct a `NovaMcpService` and serve it over
/// stdio (MVP transport, per `oxigraph_nova_mcp`'s crate docs).
///
/// **Important**: unlike every other subcommand in this file, this handler
/// must not `println!` anything to stdout once the stdio transport takes
/// over — the MCP stdio transport uses stdout for the JSON-RPC protocol
/// stream itself. All diagnostics below go to stderr / `tracing` instead.
#[cfg(feature = "mcp")]
async fn run_mcp_serve(
    location: Option<std::path::PathBuf>,
    backend: String,
    reasoning: bool,
    fulltext: bool,
    max_results: Option<usize>,
) -> anyhow::Result<()> {
    // Resolve via the self-registering backend registry (no LOUDS/Ring match arms).
    let backend =
        oxigraph_nova_core::require_backend(&backend).map_err(|e| anyhow::anyhow!("{e}"))?;
    let store = match &location {
        Some(dir) => {
            eprintln!(
                "[oxigraph mcp serve] Opening persistent {backend} store at {} ...",
                dir.display()
            );
            let store = oxigraph_nova_core::open_backend(backend, dir)
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            eprintln!(
                "[oxigraph mcp serve] Recovered {} triples from WAL.",
                store.triple_count()
            );
            store
        }
        None => {
            eprintln!(
                "[oxigraph mcp serve] Using an in-memory {backend} store (no --location given)."
            );
            oxigraph_nova_core::new_backend(backend).map_err(|e| anyhow::anyhow!("{e}"))?
        }
    };

    #[cfg(feature = "fulltext")]
    let text_search: Option<Arc<dyn oxigraph_nova_core::TextSearch>> = if fulltext {
        eprintln!("[oxigraph mcp serve] Enabling full-text search (text:query/text:contains) ...");
        store
            .enable_fulltext()
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        store.clone().as_text_search()
    } else {
        None
    };
    #[cfg(not(feature = "fulltext"))]
    let text_search: Option<Arc<dyn oxigraph_nova_core::TextSearch>> = {
        if fulltext {
            anyhow::bail!(
                "--fulltext was passed, but this binary was not built with the `fulltext` \
                 cargo feature (rebuild with `--features mcp,fulltext`)"
            );
        }
        None
    };

    let mut service = oxigraph_nova_mcp::NovaMcpService::new(Arc::clone(&store));
    if reasoning {
        eprintln!("[oxigraph mcp serve] Enabling OWL 2 RL reasoning overlay ...");
        let engine: Arc<dyn oxigraph_nova_reasoning::ReasoningEngine> =
            Arc::new(oxigraph_nova_reasoning::LftjFixpointEngine::new());
        service = service.with_reasoning(Arc::new(oxigraph_nova_reasoning::ReasoningState::new(
            engine,
        )));
    }
    if let Some(ts) = text_search {
        service = service.with_text_search(ts);
    }
    if let Some(n) = max_results {
        service = service.with_max_results(n);
    }

    eprintln!("[oxigraph mcp serve] Ready (backend={backend}). Serving MCP over stdio ...");
    oxigraph_nova_mcp::serve_stdio(service).await?;
    eprintln!("[oxigraph mcp serve] Client disconnected; shutting down.");
    Ok(())
}

/// Hard error: this binary was not built with the `mcp` cargo feature.
#[cfg(not(feature = "mcp"))]
async fn run_mcp_serve(
    _location: Option<std::path::PathBuf>,
    _backend: String,
    _reasoning: bool,
    _fulltext: bool,
    _max_results: Option<usize>,
) -> anyhow::Result<()> {
    anyhow::bail!(
        "`oxigraph mcp serve` was invoked, but this binary was not built with the `mcp` cargo \
         feature (rebuild with `cargo run -p oxigraph-nova-cli --features mcp`)"
    );
}
