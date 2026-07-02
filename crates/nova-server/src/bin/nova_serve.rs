//! Standalone Nova SPARQL server binary backed by `RingStore` (Ring + LFTJ).
//!
//! Loads an N-Triples file into memory, builds the Ring index (via
//! `compact()`), then serves the SPARQL 1.1 HTTP Protocol on the given
//! address. Used for external comparative benchmarking against Oxigraph and
//! QLever — see `benches/external/`.
//!
//! # Usage
//! ```bash
//! cargo run -p oxigraph-nova-server --release --bin nova_serve -- \
//!     --data /tmp/oxigraph-nova-bench/dataset.nt --bind 0.0.0.0:3030
//! ```
//!
//! Storage model note: `RingStore` is always fully in-process heap memory —
//! there is no disk persistence at all, so the entire dataset plus index
//! must fit in RAM. This matches Oxigraph's `serve` (run without
//! `--location`) for a fair memory-vs-memory comparison.

use oxigraph_nova_core::{GraphName, Quad};

use oxigraph_nova_server::Server;
use oxigraph_nova_storage_ring::RingStore;
use oxttl::NTriplesParser;
use std::env;
use std::fs::File;
use std::io::BufReader;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let mut data: Option<PathBuf> = None;
    let mut bind: String = "0.0.0.0:3030".to_string();

    let args: Vec<String> = env::args().collect();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--data" | "-d" => {
                i += 1;
                data = Some(PathBuf::from(&args[i]));
            }
            "--bind" | "-b" => {
                i += 1;
                bind = args[i].clone();
            }
            "--help" | "-h" => {
                println!(
                    "Usage: nova_serve --data <dataset.nt> [--bind 0.0.0.0:3030]\n\
                     Loads an N-Triples file into a RingStore and serves SPARQL HTTP."
                );
                return;
            }
            other => panic!("unknown argument: {other}"),
        }
        i += 1;
    }

    let data = data.expect("--data <dataset.nt> is required");

    eprintln!("[nova_serve] Loading {} ...", data.display());
    let t0 = Instant::now();
    let file = File::open(&data).expect("failed to open dataset file");
    let reader = BufReader::new(file);

    // Phase A.3: parse quads and hand them to `bulk_load()`, which bypasses
    // the delta `BTreeMap` entirely (avoids O(n) BTreeMap node-allocation
    // overhead during the initial load) — see `CLAUDE.md`'s "Memory
    // footprint investigation" section. We still log progress every 200K
    // triples during parsing, via an iterator adapter (no separate
    // `compact()` call needed — `bulk_load()` produces a compacted state
    // directly).
    let store = RingStore::new();
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

    let count = store.bulk_load(quads).expect("bulk_load failed");
    eprintln!(
        "[nova_serve] Loaded + compacted {count} triples in {:.2}s. Ring triple_count={}",
        t0.elapsed().as_secs_f64(),
        store.triple_count()
    );


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
            "[nova_serve]   Dictionary (terms, 2x dup):      {:>10.2} MiB",
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

    // ── Per-ordering (SPO/SOP/PSO/POS/OPS/OSP) breakdown (Phase A.1) ────────
    {
        let bd = store.per_ordering_breakdown();
        let mib = |b: usize| b as f64 / (1024.0 * 1024.0);
        eprintln!("[nova_serve] ── Per-ordering breakdown (Phase A.1 diagnostic) ──");
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
        eprintln!(
            "[nova_serve]   (*Vocab is undeduped per-ordering; deduped total below)"
        );
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
