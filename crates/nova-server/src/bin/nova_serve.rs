//! Standalone Nova SPARQL server binary backed by `RingStore` (Ring + LFTJ).
//!
//! Loads an N-Triples file into memory, builds the Ring index (via
//! `compact()`), then serves the SPARQL 1.1 HTTP Protocol on the given
//! address. Used for external comparative benchmarking against Oxigraph and
//! QLever — see `benches/external/`.
//!
//! # Usage
//! ```bash
//! # In-memory only (no persistence, matches Oxigraph's `serve` without `--location`):
//! cargo run -p oxigraph-nova-server --release --bin nova_serve -- \
//!     --data /tmp/oxigraph-nova-bench/dataset.nt --bind 0.0.0.0:3030
//!
//! # Persistent (WAL-backed) store: writes survive restarts.
//! cargo run -p oxigraph-nova-server --release --bin nova_serve -- \
//!     --location /tmp/oxigraph-nova-bench/data --bind 0.0.0.0:3030
//!
//! # Persistent store, bulk-loaded from a dataset on first run only (if the
//! # WAL directory is empty/fresh; on subsequent runs the existing WAL is
//! # replayed instead and --data is ignored):
//! cargo run -p oxigraph-nova-server --release --bin nova_serve -- \
//!     --location /tmp/oxigraph-nova-bench/data \
//!     --data /tmp/oxigraph-nova-bench/dataset.nt --bind 0.0.0.0:3030
//! ```
//!
//! Storage model note: without `--location`, `RingStore` is always fully
//! in-process heap memory — there is no disk persistence at all, so the
//! entire dataset plus index must fit in RAM (matches Oxigraph's `serve` run
//! without `--location`, for a fair memory-vs-memory comparison). With
//! `--location <dir>`, every `insert`/`remove`/`register_named_graph` call is
//! first durably logged to a write-ahead log (WAL) file in `<dir>` before
//! being applied in memory — see `oxigraph_nova_storage_ring::wal` and
//! `RingStore::open` for details of the overall persistent-storage design.

use mimalloc::MiMalloc;
use oxigraph_nova_core::{GraphName, Quad};
use oxigraph_nova_server::Server;
use oxigraph_nova_storage_ring::{RingStore, SyncPolicy};
use oxttl::NTriplesParser;
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

use std::io::BufReader;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let mut data: Option<PathBuf> = None;
    let mut location: Option<PathBuf> = None;
    let mut bind: String = "0.0.0.0:3030".to_string();
    let mut compact_threshold: Option<usize> = None;
    let mut sync_interval_ms: Option<u64> = None;

    let args: Vec<String> = env::args().collect();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--data" | "-d" => {
                i += 1;
                data = Some(PathBuf::from(&args[i]));
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
            "--help" | "-h" => {
                println!(
                    "Usage: nova_serve [--data <dataset.nt>] [--location <dir>] [--bind 0.0.0.0:3030] \
                     [--compact-threshold <n>] [--sync-interval-ms <n>]\n\
                     \n\
                     Without --location: purely in-memory RingStore, --data is required.\n\
                     With --location <dir>: persistent WAL-backed RingStore rooted at <dir>.\n\
                     If <dir> already has a WAL, it is replayed and --data is ignored.\n\
                     If <dir> is empty/fresh and --data is given, the dataset is bulk-loaded\n\
                     and then persisted (each triple logged to the WAL) for future restarts.\n\
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
                     oxigraph_nova_storage_ring::SyncPolicy docs."
                );
                return;
            }

            other => panic!("unknown argument: {other}"),
        }
        i += 1;
    }

    // ── Construct the store: persistent (--location) or in-memory ──────────
    let store = match &location {
        Some(dir) => {
            eprintln!(
                "[nova_serve] Opening persistent store at {} ...",
                dir.display()
            );
            let store = RingStore::open(dir).expect("RingStore::open failed");
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
        None => RingStore::new(),
    };

    // Bulk-load from --data only if the store came up empty (a fresh
    // in-memory store, or a fresh/empty --location directory with no prior
    // WAL history). If a persistent store already has data recovered from
    // its WAL, --data is ignored — the WAL is the source of truth.
    if let Some(data) = &data {
        if store.triple_count() > 0 {
            eprintln!(
                "[nova_serve] Store already has {} triples (from --location); ignoring --data.",
                store.triple_count()
            );
        } else {
            eprintln!("[nova_serve] Loading {} ...", data.display());
            let t0 = Instant::now();
            let file = File::open(data).expect("failed to open dataset file");
            let reader = BufReader::new(file);

            let mut parsed: usize = 0;
            let quads = NTriplesParser::new().for_reader(reader).map(|result| {
                let triple = result.expect("N-Triples parse error");
                parsed += 1;
                if parsed.is_multiple_of(200_000) {
                    eprintln!("[nova_serve]   ... {parsed} triples parsed");
                }
                Quad::new(
                    triple.subject,
                    triple.predicate,
                    triple.object,
                    GraphName::DefaultGraph,
                )
            });

            // `bulk_load()` bypasses both the delta `BTreeMap` *and* the WAL
            // entirely: it builds the Ring directly in memory, then commits
            // via `commit_compaction`, which — for a persistent store — does
            // a single atomic snapshot + dictionary + WAL-segment-rotation +
            // MANIFEST commit (see `RingStore::commit_compaction`). This
            // matters enormously for `--location --data`: the old path went
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
            let count = store.bulk_load(quads).expect("bulk_load failed");
            eprintln!(
                "[nova_serve] Loaded + compacted {count} triples in {:.2}s.",
                t0.elapsed().as_secs_f64()
            );
        }
    } else if location.is_none() {
        panic!("either --data <dataset.nt> or --location <dir> is required");
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
            "[nova_serve]   {:<6} {:>10} {:>10} {:>10} {:>12}",
            "Order", "T (MiB)", "L (MiB)", "Side (MiB)", "Vocab* (MiB)"
        );
        for i in 0..6 {
            let ord = bd.orders[i];
            let b = &bd.per_order[i];
            eprintln!(
                "[nova_serve]   {:<6} {:>10.2} {:>10.2} {:>10.2} {:>12.2}",
                format!("{ord:?}"),
                mib(b.t_bytes),
                mib(b.l_bytes),
                mib(b.sidecar_bytes),
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
    Server::new(store).run(&bind).await.expect("server failed");
}
