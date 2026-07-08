//! `oxigraph` — standalone offline CLI tooling for Oxigraph Nova.
//!
//! Mirrors a subset of `oxigraph-cli`'s subcommands (see
//! `./research/oxigraph/cli`), adapted to Nova's own `RingStore`/`Server`
//! API surface, and shipped under
//! the same binary name (`oxigraph`) so scripts/muscle memory written
//! against upstream `oxigraph-cli` work unchanged:
//!
//! - `oxigraph load --location <dir> --file <path> [--format F] [--graph G]`
//!   — bulk-load directly into a persistent store, without going through HTTP.
//! - `oxigraph backup --location <dir> --destination <dir>` — mirrors
//!   `oxigraph backup` 1:1 (see `RingStore::backup`).
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
use oxigraph_nova_core::GraphName;
use oxigraph_nova_server::{Server, mimalloc_tuning};
use oxigraph_nova_storage_ring::{RingStore, SyncPolicy};
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
        } => run_load(&location, &file, format.as_deref(), graph.as_deref()),
        Command::Backup {
            location,
            destination,
        } => run_backup(&location, &destination),
        Command::Serve {
            location,
            file,
            bind,
            compact_threshold,
            sync_interval_ms,
            query_timeout_s,
            max_results,
            max_parallel_queries,
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
    file: &std::path::Path,
    format: Option<&str>,
    graph: Option<&str>,
) -> anyhow::Result<()> {
    let graph_name = parse_graph_name(graph)?;

    println!(
        "[oxigraph load] Opening persistent store at {} ...",
        location.display()
    );
    let store = RingStore::open(location)?;

    println!("[oxigraph load] Parsing {} ...", file.display());
    let t0 = Instant::now();
    let quads = load::parse_file(file, format, graph_name.as_ref())?;
    let parsed = quads.len();

    println!("[oxigraph load] Bulk-loading {parsed} quads ...");
    let count = store.bulk_load(quads)?;
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

async fn run_serve(
    location: Option<std::path::PathBuf>,
    file: Option<std::path::PathBuf>,
    bind: String,
    compact_threshold: Option<usize>,
    sync_interval_ms: Option<u64>,
    query_timeout_s: Option<u64>,
    max_results: Option<usize>,
    max_parallel_queries: Option<usize>,
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
            let quads = load::parse_file(file, None, None)?;
            let count = store.bulk_load(quads)?;
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
    server.run(&bind).await?;
    Ok(())
}
