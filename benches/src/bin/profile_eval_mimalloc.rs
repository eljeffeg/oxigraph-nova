//! A/B test twin of `profile_eval --rss-loop`, but with `mimalloc` installed
//! as the process's global allocator instead of the system allocator, to
//! test whether swapping allocators eliminates the RSS-growth-without-bound
//! behavior observed with the system allocator (see `profile_eval.rs`'s
//! `--rss-loop` mode and its module docs). `#[global_allocator]` can't be
//! swapped at runtime, hence this is a separate binary rather than a flag.
//!
//! Usage:
//!   cargo run -p oxigraph-nova-bench --release --bin profile_eval_mimalloc -- \
//!       /tmp/oxigraph-nova-bench/dataset.nt /tmp/oxigraph-nova-bench/dataset.queries.json \
//!       path_2hop 60

use mimalloc::MiMalloc;
use oxigraph_nova_core::{GraphName, Quad};
use oxigraph_nova_query::{Evaluator, QueryResult, StoreDataset};
use oxigraph_nova_storage_ring::RingStore;
use oxttl::NTriplesParser;
use serde::Deserialize;
use spargebra::SparqlParser;
use std::fs::File;
use std::io::BufReader;
use std::sync::Arc;
use std::time::Instant;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

#[derive(Deserialize)]
struct QueryDef {
    name: String,
    sparql: String,
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let path = args
        .get(1)
        .cloned()
        .unwrap_or_else(|| "/tmp/oxigraph-nova-bench/dataset.nt".to_string());
    let queries_path = args
        .get(2)
        .cloned()
        .unwrap_or_else(|| "/tmp/oxigraph-nova-bench/dataset.queries.json".to_string());
    let query_name = args
        .get(3)
        .cloned()
        .unwrap_or_else(|| "path_2hop".to_string());
    let n: usize = args.get(4).map(|s| s.parse().unwrap()).unwrap_or(60);

    eprintln!("[profile_eval_mimalloc] Loading {path} ...");
    let t0 = Instant::now();
    let file = File::open(&path).expect("open dataset");
    let reader = BufReader::new(file);
    let quads = NTriplesParser::new().for_reader(reader).map(|r| {
        let t = r.expect("parse error");
        Quad::new(t.subject, t.predicate, t.object, GraphName::DefaultGraph)
    });
    let store = RingStore::new();
    let count = store.bulk_load(quads).expect("bulk_load failed");
    eprintln!(
        "[profile_eval_mimalloc] Loaded + compacted {count} triples in {:.2}s.",
        t0.elapsed().as_secs_f64()
    );

    let store = Arc::new(store);
    let ds = StoreDataset::new(Arc::clone(&store));

    let queries_json = std::fs::read_to_string(&queries_path).expect("read queries.json");
    let queries: Vec<QueryDef> = serde_json::from_str(&queries_json).expect("parse queries.json");
    let qd = queries
        .iter()
        .find(|q| q.name == query_name)
        .unwrap_or_else(|| panic!("no such query: {query_name}"));
    let q = SparqlParser::new().parse_query(&qd.sparql).unwrap();

    let pid = std::process::id();
    println!("[rss-loop-mimalloc] pid={pid} query={query_name} n={n}");
    for i in 0..n {
        let ev = Evaluator::new(&ds);
        let result = ev.evaluate(&q).unwrap();
        let rows = match result {
            QueryResult::Solutions(s) => s.len(),
            QueryResult::Boolean(b) => b as usize,
            QueryResult::Triples(t) => t.len(),
        };
        let rss_kb = std::process::Command::new("ps")
            .args(["-o", "rss=", "-p", &pid.to_string()])
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .and_then(|s| s.trim().parse::<u64>().ok())
            .unwrap_or(0);
        println!(
            "[rss-loop-mimalloc] iter={i} rows={rows} rss_mb={:.1}",
            rss_kb as f64 / 1024.0
        );
    }
}
