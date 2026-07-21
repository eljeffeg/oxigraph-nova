//! In-process path_2hop / triangle p50 (no HTTP/serialize).
use std::sync::Arc;
use std::time::Instant;

use oxigraph_nova_core::{GraphName, NamedNode, Quad, QuadStore, Subject, Term};
use oxigraph_nova_engine_ring::LoudsStore;
use oxigraph_nova_engine_ring::RingStore;
use oxigraph_nova_query::{Evaluator, QueryResult, StoreDataset};
use spargebra::SparqlParser;

const N: usize = 50_000;
const P_REL: &str = "https://www.wikidata.org/prop/direct/related";
const WD: &str = "http://www.wikidata.org/entity/";

fn entity(i: usize) -> NamedNode {
    NamedNode::new_unchecked(format!("{WD}Q{i}"))
}
fn pred() -> NamedNode {
    NamedNode::new_unchecked(P_REL)
}

fn quads() -> Vec<Quad> {
    let p = pred();
    let dg = GraphName::DefaultGraph;
    let mut out = Vec::with_capacity(N * 3);
    for i in 0..N {
        let s = entity(i);
        for d in 1..=3 {
            let o = entity((i + d) % N);
            out.push(Quad::new(
                Subject::NamedNode(s.clone()),
                p.clone(),
                Term::NamedNode(o),
                dg.clone(),
            ));
        }
    }
    out
}

fn load_ring() -> Arc<RingStore> {
    let store = Arc::new(RingStore::new());
    for q in quads() {
        store.insert(&q).unwrap();
    }
    store.compact().unwrap();
    store
}

fn load_louds() -> Arc<LoudsStore> {
    let store = Arc::new(LoudsStore::new());
    for q in quads() {
        store.insert(&q).unwrap();
    }
    store.compact().unwrap();
    store
}

fn run_ring(store: Arc<RingStore>, sparql: &str, warm: usize, iters: usize) -> (f64, u64) {
    let ds = StoreDataset::new(store);
    eval_loop(&ds, sparql, warm, iters)
}

fn run_louds(store: Arc<LoudsStore>, sparql: &str, warm: usize, iters: usize) -> (f64, u64) {
    let ds = StoreDataset::new(store);
    eval_loop(&ds, sparql, warm, iters)
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
    let path_q = "PREFIX wdt: <https://www.wikidata.org/prop/direct/>\
SELECT ?a ?b ?c WHERE { ?a wdt:related ?b . ?b wdt:related ?c . }";
    let tri_q = "PREFIX wdt: <https://www.wikidata.org/prop/direct/>\
SELECT ?a ?b ?c WHERE { ?a wdt:related ?b . ?b wdt:related ?c . ?a wdt:related ?c . }";

    eprintln!("ring load N={N}...");
    let ring = load_ring();
    eprintln!("ring mapped={}", ring.all_graphs_mapped());
    let (rp, rr) = run_ring(Arc::clone(&ring), path_q, 2, 5);
    let (rt, rtr) = run_ring(ring, tri_q, 2, 5);
    println!("RESULT ring path_2hop p50_ms={rp:.2} rows={rr}");
    println!("RESULT ring triangle p50_ms={rt:.2} rows={rtr}");

    eprintln!("louds load N={N}...");
    let louds = load_louds();
    let (lp, lr) = run_louds(Arc::clone(&louds), path_q, 2, 5);
    let (lt, ltr) = run_louds(louds, tri_q, 2, 5);
    println!("RESULT louds path_2hop p50_ms={lp:.2} rows={lr}");
    println!("RESULT louds triangle p50_ms={lt:.2} rows={ltr}");
    println!(
        "RESULT ratio path={:.3}x tri={:.3}x",
        rp / lp.max(0.001),
        rt / lt.max(0.001)
    );
}
