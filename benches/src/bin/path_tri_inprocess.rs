//! In-process heavy-query p50 (no HTTP/serialize) — LOUDS vs Ring.
//! Dataset matches external RESULTS_MEM generator (`generate_quads_large`).
use oxigraph_nova_bench::generate_quads_large;
use oxigraph_nova_core::QuadStore;
use oxigraph_nova_engine_ring::{LoudsStore, RingStore};
use oxigraph_nova_query::{Evaluator, QueryResult, StoreDataset};
use spargebra::SparqlParser;
use std::sync::Arc;
use std::time::Instant;

const N: usize = 50_000;

fn load_ring() -> Arc<RingStore> {
    let store = Arc::new(RingStore::new());
    for q in generate_quads_large(N) {
        store.insert(&q).unwrap();
    }
    store.compact().unwrap();
    store
}

fn load_louds() -> Arc<LoudsStore> {
    let store = Arc::new(LoudsStore::new());
    for q in generate_quads_large(N) {
        store.insert(&q).unwrap();
    }
    store.compact().unwrap();
    store
}

fn eval_loop<S: QuadStore + 'static>(
    ds: &StoreDataset<S>,
    sparql: &str,
    warm: usize,
    iters: usize,
) -> (f64, u64) {
    let q = SparqlParser::new().parse_query(sparql).unwrap();
    for _ in 0..warm {
        let mut ev = Evaluator::new(ds);
        let _ = count_sols(&mut ev, &q);
    }
    let mut times = Vec::new();
    let mut rows = 0u64;
    for _ in 0..iters {
        let t0 = Instant::now();
        let mut ev = Evaluator::new(ds);
        rows = count_sols(&mut ev, &q);
        times.push(t0.elapsed().as_secs_f64() * 1000.0);
    }
    times.sort_by(|a, b| a.partial_cmp(b).unwrap());
    (times[times.len() / 2], rows)
}

fn count_sols<S: QuadStore + 'static>(
    ev: &mut Evaluator<'_, StoreDataset<S>>,
    q: &spargebra::Query,
) -> u64 {
    match ev.evaluate(q).unwrap() {
        QueryResult::Solutions { stream, .. } => {
            let mut n = 0u64;
            for s in stream {
                let _ = s.unwrap();
                n += 1;
            }
            n
        }
        _ => 0,
    }
}

fn main() {
    let queries: &[(&str, &str)] = &[
        (
            "feature_lookup",
            "PREFIX wd: <https://www.wikidata.org/entity/> PREFIX wdt: <https://www.wikidata.org/prop/direct/> SELECT ?p WHERE { ?p wdt:P2 wd:feature0 }",
        ),
        (
            "star_with_features",
            "PREFIX wd: <https://www.wikidata.org/entity/> PREFIX wdt: <https://www.wikidata.org/prop/direct/> SELECT ?p ?f WHERE { ?p wdt:P31 wd:class0 . ?p wdt:P2 ?f }",
        ),
        (
            "path_2hop",
            "PREFIX wd: <https://www.wikidata.org/entity/> PREFIX wdt: <https://www.wikidata.org/prop/direct/> SELECT ?a ?b ?c WHERE { ?a wdt:related ?b . ?b wdt:related ?c }",
        ),
        (
            "triangle",
            "PREFIX wd: <https://www.wikidata.org/entity/> PREFIX wdt: <https://www.wikidata.org/prop/direct/> SELECT ?a ?b ?c WHERE { ?a wdt:related ?b . ?b wdt:related ?c . ?a wdt:related ?c }",
        ),
    ];

    eprintln!("ring load N={N} full BSBM...");
    let ring = load_ring();
    eprintln!("ring mapped={}", ring.all_graphs_mapped());
    let ds_r = StoreDataset::new(Arc::clone(&ring));
    for &(name, sparql) in queries {
        let (p50, rows) = eval_loop(&ds_r, sparql, 2, 5);
        println!("RESULT ring {name} p50_ms={p50:.2} rows={rows}");
    }

    eprintln!("louds load N={N} full BSBM...");
    let louds = load_louds();
    let ds_l = StoreDataset::new(louds);
    for &(name, sparql) in queries {
        let (p50, rows) = eval_loop(&ds_l, sparql, 2, 5);
        println!("RESULT louds {name} p50_ms={p50:.2} rows={rows}");
    }
}
