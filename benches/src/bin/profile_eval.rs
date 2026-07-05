//! Ad-hoc profiling tool: loads the 500K-entity benchmark dataset into a
//! RingStore and times pure in-process query evaluation (no HTTP, no JSON
//! serialization) for the queries that lost to Oxigraph/QLever in the
//! external HTTP benchmark. Used to isolate whether the bottleneck is LFTJ
//! evaluation itself or the HTTP/serialization layer around it.
//!
//! Usage:
//!   cargo run -p oxigraph-nova-bench --release --bin profile_eval -- \
//!       /tmp/oxigraph-nova-bench/dataset.nt
use oxigraph_nova_core::{GraphName, Quad, QuadStore};
use oxigraph_nova_query::{Dataset, Evaluator, QueryResult, StoreDataset};
use oxigraph_nova_storage_ring::RingStore;
use oxttl::NTriplesParser;
use spargebra::SparqlParser;
use std::fs::File;
use std::io::BufReader;
use std::sync::Arc;
use std::time::Instant;

const PREFIXES: &str = concat!(
    "PREFIX wd:  <https://www.wikidata.org/entity/>\n",
    "PREFIX wdt: <https://www.wikidata.org/prop/direct/>\n",
);

fn run_query<D: Dataset>(ds: &D, name: &str, sparql: &str, iters: usize) {
    let q = SparqlParser::new().parse_query(sparql).unwrap();
    // warmup
    for _ in 0..2 {
        let ev = Evaluator::new(ds);
        let _ = ev.evaluate(&q).unwrap();
    }
    let mut times = Vec::with_capacity(iters);
    let mut n_results = 0usize;
    for _ in 0..iters {
        let ev = Evaluator::new(ds);
        let t0 = Instant::now();
        let result = ev.evaluate(&q).unwrap();
        let elapsed = t0.elapsed();
        n_results = match result {
            QueryResult::Solutions(s) => s.len(),
            QueryResult::Boolean(b) => b as usize,
            QueryResult::Triples(t) => t.len(),
        };
        times.push(elapsed.as_secs_f64() * 1000.0);
    }
    times.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let mean = times.iter().sum::<f64>() / times.len() as f64;
    let p50 = times[times.len() / 2];
    println!(
        "[{name}] rows={n_results} iters={iters} mean={mean:.2}ms p50={p50:.2}ms min={:.2}ms max={:.2}ms",
        times[0],
        times[times.len() - 1]
    );
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let path = args
        .get(1)
        .cloned()
        .unwrap_or_else(|| "/tmp/oxigraph-nova-bench/dataset.nt".to_string());

    eprintln!("[profile_eval] Loading {path} ...");
    let t0 = Instant::now();
    let file = File::open(&path).expect("open dataset");
    let reader = BufReader::new(file);
    let mut parsed = 0usize;
    let quads = NTriplesParser::new().for_reader(reader).map(|r| {
        let t = r.expect("parse error");
        parsed += 1;
        if parsed.is_multiple_of(1_000_000) {
            eprintln!("[profile_eval]   ... {parsed} triples parsed");
        }
        Quad::new(t.subject, t.predicate, t.object, GraphName::DefaultGraph)
    });
    let store = RingStore::new();
    let count = store.bulk_load(quads).expect("bulk_load failed");
    eprintln!(
        "[profile_eval] Loaded + compacted {count} triples in {:.2}s.",
        t0.elapsed().as_secs_f64()
    );

    let store = Arc::new(store);
    let ds = StoreDataset::new(Arc::clone(&store));

    println!("\n=== Pure in-process evaluation (no HTTP, no JSON serialization) ===\n");

    run_query(
        &ds,
        "scan",
        &format!("{PREFIXES}SELECT ?s ?p ?o WHERE {{ ?s ?p ?o }}"),
        5,
    );
    run_query(
        &ds,
        "2join",
        &format!(
            "{PREFIXES}SELECT ?p ?r WHERE {{ ?p wdt:P31 wd:class0 . ?p wdt:P131 ?r }}"
        ),
        10,
    );
    run_query(
        &ds,
        "feature_lookup",
        &format!("{PREFIXES}SELECT ?p WHERE {{ ?p wdt:P2 wd:feature0 }}"),
        10,
    );
    run_query(
        &ds,
        "star_with_features",
        &format!(
            "{PREFIXES}SELECT ?p ?f WHERE {{ ?p wdt:P31 wd:class0 . ?p wdt:P2 ?f }}"
        ),
        10,
    );
    run_query(
        &ds,
        "path_2hop",
        &format!(
            "{PREFIXES}SELECT ?a ?b ?c WHERE {{ ?a wdt:related ?b . ?b wdt:related ?c }}"
        ),
        5,
    );
    run_query(
        &ds,
        "triangle",
        &format!(
            "{PREFIXES}SELECT ?a ?b ?c WHERE {{ ?a wdt:related ?b . ?b wdt:related ?c . ?a wdt:related ?c }}"
        ),
        5,
    );
}
