//! Large-scale BSBM-mirrored benchmarks — sidecar optimization validation dataset.
//!
//! ## Purpose
//!
//! The existing `wikidata_slice.rs` benchmarks top out at N=50,000 entities with
//! 3 `related-to` edges per entity — too few to activate the L-opt B sidecar
//! (`SIDECAR_THRESH=16`).  This file provides a BSBM-inspired dataset where each
//! entity has **20 has-feature edges**, guaranteeing every entity's feature leaf
//! node in the SPO trie has degree=20 ≥ SIDECAR_THRESH.  Without the sidecar
//! firing, the Elias-Fano `O(1) succ` optimization would show zero improvement.
//!
//! ## Dataset structure (BSBM-inspired)
//!
//! N=50,000 entities × 25 triples each = **1,250,000 triples total**.
//!
//! | Predicate | Fan-out | BSBM analog | Sidecar? |
//! |---|---|---|---|
//! | instance-of (P31) | 1 | `bsbm:ProductType` | No |
//! | located-in (P131) | 1 | `gr:availableAtOrFrom` | No |
//! | has-feature (P2) | **20** ← | `bsbm:productFeature` | **Yes** (degree=20 ≥ 16) |
//! | related-to | 3 | `bsbm:Product seeAlsoProduct` | No |
//!
//! **Sidecar activation** (SPO trie, depth-2 node):
//! - Node `(S=entity_i, P=hasFeature)` has 20 O-children → degree=20 ≥ SIDECAR_THRESH=16.
//! - All 50,000 entities trigger the sidecar for every feature-scan leap.
//!
//! **Sidecar activation** (POS trie, depth-2 node):
//! - Node `(P=hasFeature, O=feature_j)` has ~500 S-children (every 100th entity).
//! - Degree=500 >> SIDECAR_THRESH=16 → `feature_lookup` benchmark.
//!
//! ## Comparison with Oxigraph's BSBM benchmark
//!
//! Oxigraph uses the [Berlin SPARQL Benchmark](http://wifo5-03.informatik.uni-mannheim.de/bizer/berlinsparqlbenchmark/)
//! (`bench/bsbm_oxigraph.sh`) with:
//! - 100k products → 35M triples
//! - HTTP server + Java test-driver
//! - 16 concurrent threads
//! - Measures end-to-end query latency (includes HTTP overhead, thread scheduling)
//!
//! Our benchmarks measure **pure library throughput** (no HTTP, no concurrency) —
//! isolating the storage backend and query evaluator.  This is intentionally
//! different: we're validating internal algorithmic improvements, not overall
//! system throughput.  The BSBM structural join patterns (star, feature-filter,
//! path) are mirrored here without the FILTER/ORDER BY/LIMIT operators that BSBM
//! queries use (not yet implemented in our evaluator).
//!
//! ## Parallel LFTJ scaling probes
//!
//! In addition to the sidecar-focused shapes above, this file hosts two
//! benchmarks that exercise the parallel root-dispatch path in
//! `oxigraph-nova-query::lftj` (`run_lftj_root` / chunked `par_chunks`):
//!
//! | Benchmark | Root domain | Purpose |
//! |---|---|---|
//! | `large/large_root` | ≈ 2_000 features (no class filter) | Honest multi-core scaling on a 1M-row join |
//! | `large/skewed_paths` | power-law related graph | Residual single-level-split skew (HoneyComb §1–2) |
//!
//! Run just those two:
//!
//! ```bash
//! cargo bench -p oxigraph-nova-bench --bench bsbm_large -- 'large/large_root|large/skewed_paths'
//! ```
//!
//! ## Running
//!
//! ```bash
//! # All large-scale benchmarks (generates data once, ~5-10s startup)
//! cargo bench -p oxigraph-nova-bench -- large/
//!
//! # Only the sidecar-exercising benchmark (validates high-degree optimization)
//! cargo bench -p oxigraph-nova-bench -- large/star_with_features
//!
//! # Parallel LFTJ scaling probes (large root domain + power-law skew)
//! cargo bench -p oxigraph-nova-bench --bench bsbm_large -- 'large/large_root|large/skewed_paths'
//!
//! # Comparison: baseline (no EF) vs with EF
//! # After implementing Elias-Fano backend, re-run and compare reports in
//! # target/criterion/large/star_with_features/
//! ```
//!
//! ## Optional: load a real BSBM N-Triples file
//!
//! To benchmark against real BSBM data (rather than synthetic), set the
//! `BSBM_NT_FILE` environment variable before running:
//!
//! ```bash
//! # 1. Generate BSBM data (requires Java + Oxigraph bench submodule):
//! #    cd /Users/jgentes/Documents/Workspace/oxigraph/bench
//! #    git submodule update --init
//! #    cd bsbm-tools && ./generate -fc -pc 1000 -s nt -fn explore-1000
//!
//! # 2. Run our benchmarks against the BSBM data:
//! BSBM_NT_FILE=/path/to/explore-1000.nt cargo bench -p oxigraph-nova-bench -- large/
//! ```
//!
//! When `BSBM_NT_FILE` is set, the BSBM data is loaded for `large/compact` and
//! `large/ingest` benchmarks.  SPARQL query benchmarks always use synthetic data
//! since the BSBM IRIs differ from our Wikidata-style prefixes.


use criterion::{BatchSize, Criterion, Throughput, criterion_group, criterion_main};
use oxigraph_nova_core::{GraphName, NamedNode, Quad, QuadStore, Subject, Term};
use oxigraph_nova_query::{Dataset, Evaluator, QueryResult, StoreDataset};
use oxigraph_nova_storage_ring::RingStore;
use spargebra::SparqlParser;
use std::hint::black_box;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

// ── Namespace constants ───────────────────────────────────────────────────────

const NS_ENTITY: &str = "https://www.wikidata.org/entity/";
const P_P31: &str = "https://www.wikidata.org/prop/direct/P31"; // instance-of (BSBM: productType)
const P_P131: &str = "https://www.wikidata.org/prop/direct/P131"; // located-in  (BSBM: availableAt)
const P_P2: &str = "https://www.wikidata.org/prop/direct/P2"; // has-feature (BSBM: productFeature)
const P_REL: &str = "https://www.wikidata.org/prop/direct/related"; // related-to  (BSBM: seeAlso)

/// SPARQL PREFIX block shared by all query strings.
const PREFIXES: &str = concat!(
    "PREFIX wd:  <https://www.wikidata.org/entity/>\n",
    "PREFIX wdt: <https://www.wikidata.org/prop/direct/>\n",
);

// ── Dataset parameters ────────────────────────────────────────────────────────

/// Number of entities.
pub const LARGE_N: usize = 50_000;

/// Features per entity.  Must be ≥ SIDECAR_THRESH=16 to activate L-opt B.
/// 20 chosen to match BSBM's typical productFeature fan-out and clear the
/// SIDECAR_THRESH=16 threshold with margin.
pub const FAN_OUT_FEATURES: usize = 20;

/// Related-to edges per entity (for triangle queries).  Keep at 3 so triangle
/// result cardinality (~3×N = 150k) remains practical to enumerate.
pub const FAN_OUT_RELATED: usize = 3;

/// Number of distinct class values (BSBM: productType).
pub const N_CLASSES: usize = 50;

/// Number of distinct region values (BSBM: availableAt region).
pub const N_REGIONS: usize = 500;

/// Size of the feature value pool.  With FAN_OUT=20 and LARGE_N=50k:
/// average entities per feature = 50k×20/2000 = 500 → POS degree=500 >> 16.
pub const N_FEATURES: usize = 2_000;

/// Total triples: LARGE_N × (instance-of + located-in + features + related).
pub const LARGE_TRIPLE_COUNT: usize = LARGE_N * (2 + FAN_OUT_FEATURES + FAN_OUT_RELATED);
// = 50_000 × 25 = 1_250_000

// ── IRI helpers ───────────────────────────────────────────────────────────────

#[inline]
fn entity_iri(i: usize) -> NamedNode {
    NamedNode::new_unchecked(format!("{NS_ENTITY}Q{i}"))
}
#[inline]
fn class_iri(j: usize) -> NamedNode {
    NamedNode::new_unchecked(format!("{NS_ENTITY}class{j}"))
}
#[inline]
fn region_iri(k: usize) -> NamedNode {
    NamedNode::new_unchecked(format!("{NS_ENTITY}region{k}"))
}
#[inline]
fn feature_iri(f: usize) -> NamedNode {
    NamedNode::new_unchecked(format!("{NS_ENTITY}feature{f}"))
}

// ── Synthetic dataset generator ───────────────────────────────────────────────

/// Generate `n` entities in BSBM product-feature style.
///
/// Each entity receives:
/// - 1 `instance-of` triple  → one of `N_CLASSES` classes
/// - 1 `located-in` triple   → one of `N_REGIONS` regions
/// - `FAN_OUT_FEATURES` `has-feature` triples → cycling through `N_FEATURES` feature objects
/// - `FAN_OUT_RELATED` `related-to` triples   → neighboring entity IRIs (for triangles)
///
/// ## Feature assignment
///
/// Entity `i` gets features `(i × FAN_OUT + k) % N_FEATURES` for `k` in `0..FAN_OUT_FEATURES`.
/// This cycles uniformly through the pool; each feature appears in
/// `LARGE_N × FAN_OUT_FEATURES / N_FEATURES = 500` entities on average.
///
/// ## Sidecar guarantee
///
/// In the SPO trie, node `(S=entity_i, P=has-feature)` at depth 2 has exactly
/// `FAN_OUT_FEATURES=20` children — degree ≥ SIDECAR_THRESH=16 for every entity.
///
/// In the POS trie, node `(P=has-feature, O=feature_j)` at depth 2 has ~500
/// subject children for each feature — well above SIDECAR_THRESH.
pub fn generate_quads_large(n: usize) -> Vec<Quad> {
    assert!(
        n > FAN_OUT_RELATED,
        "need at least FAN_OUT_RELATED+1 entities to avoid self-edges"
    );

    let p31 = NamedNode::new_unchecked(P_P31);
    let p131 = NamedNode::new_unchecked(P_P131);
    let p_feat = NamedNode::new_unchecked(P_P2);
    let p_rel = NamedNode::new_unchecked(P_REL);
    let dg = GraphName::DefaultGraph;

    let triples_per = 2 + FAN_OUT_FEATURES + FAN_OUT_RELATED;
    let mut quads = Vec::with_capacity(n * triples_per);

    for i in 0..n {
        let subj = Subject::NamedNode(entity_iri(i));

        // instance-of → class (50 classes)
        quads.push(Quad::new(
            subj.clone(),
            p31.clone(),
            Term::NamedNode(class_iri(i % N_CLASSES)),
            dg.clone(),
        ));

        // located-in → region (500 regions)
        quads.push(Quad::new(
            subj.clone(),
            p131.clone(),
            Term::NamedNode(region_iri(i % N_REGIONS)),
            dg.clone(),
        ));

        // has-feature × FAN_OUT_FEATURES  (degree=20 in SPO → sidecar fires)
        for k in 0..FAN_OUT_FEATURES {
            let f = (i * FAN_OUT_FEATURES + k) % N_FEATURES;
            quads.push(Quad::new(
                subj.clone(),
                p_feat.clone(),
                Term::NamedNode(feature_iri(f)),
                dg.clone(),
            ));
        }

        // related-to × FAN_OUT_RELATED  (for triangle query)
        for j in 1..=FAN_OUT_RELATED {
            quads.push(Quad::new(
                subj.clone(),
                p_rel.clone(),
                Term::NamedNode(entity_iri((i + j) % n)),
                dg.clone(),
            ));
        }
    }
    quads
}

// ── Optional: load BSBM N-Triples file ───────────────────────────────────────
//
// Reads the BSBM_NT_FILE env var.  If set, loads that N-Triples file and
// returns the quads; otherwise returns None.  Used by compact/ingest benches.
//
// Query benchmarks always use synthetic data (BSBM IRIs differ from ours).

fn try_load_bsbm_nt() -> Option<Vec<Quad>> {
    let path = std::env::var("BSBM_NT_FILE").ok()?;
    eprintln!("[bsbm_large] Loading BSBM N-Triples from: {path}");
    match load_quads_from_nt(&path) {
        Ok(quads) => {
            eprintln!(
                "[bsbm_large] Loaded {} triples from BSBM file.",
                quads.len()
            );
            Some(quads)
        }
        Err(e) => {
            eprintln!("[bsbm_large] WARN: failed to read {path}: {e} — using synthetic data.");
            None
        }
    }
}

fn load_quads_from_nt(path: &str) -> Result<Vec<Quad>, Box<dyn std::error::Error>> {
    use oxttl::NTriplesParser;
    let file = std::fs::File::open(path)?;
    let reader = std::io::BufReader::new(file);
    let mut quads = Vec::new();
    for result in NTriplesParser::new().for_reader(reader) {
        let triple = result?;
        quads.push(Quad::new(
            triple.subject,
            triple.predicate,
            triple.object,
            GraphName::DefaultGraph,
        ));
    }
    Ok(quads)
}

// ── Global store (built once; shared across all benchmarks) ──────────────────
//
// Building the store takes ~5-10 s (insert + compact 1.25M triples).
// Using OnceLock ensures we pay this cost once per benchmark binary invocation,
// not once per benchmark group.

static LARGE_STORE: OnceLock<Arc<RingStore>> = OnceLock::new();

fn get_large_store() -> Arc<RingStore> {
    Arc::clone(LARGE_STORE.get_or_init(|| {
        eprintln!(
            "[bsbm_large] Building large store: {LARGE_N} entities \
             ({LARGE_TRIPLE_COUNT} synthetic triples) …"
        );
        let quads = generate_quads_large(LARGE_N);
        let store = Arc::new(RingStore::new());
        for q in &quads {
            store.insert(q).unwrap();
        }
        eprintln!("[bsbm_large] Compacting …");
        store.compact().unwrap();
        eprintln!(
            "[bsbm_large] Ready. triple_count={}, sidecar active on \
             nodes with degree ≥ 16 (expected: every entity's feature node).",
            store.triple_count()
        );
        store
    }))
}

// ── Query execution helper ────────────────────────────────────────────────────

fn count_solutions<D: Dataset>(dataset: &D, sparql: &str) -> usize {
    let q = SparqlParser::new().parse_query(sparql).unwrap();
    let mut ev = Evaluator::new(dataset);
    match ev.evaluate(&q).unwrap() {
        QueryResult::Solutions { stream, .. } => stream.count(),
        QueryResult::Boolean(b) => b as usize,
        QueryResult::Triples(stream) => stream.count(),
    }
}

// ── Benchmark 1: Large compact ────────────────────────────────────────────────
//
// Measures RingStore::compact() on LARGE_TRIPLE_COUNT=1.25M triples.
// If BSBM_NT_FILE is set, uses that dataset instead (for scale comparison).
//
// compact() sorts, deduplicates, and builds 6 LOUDS CLTJ tries.
// At 1.25M triples this is the dominant build-time cost.

fn bench_large_compact(c: &mut Criterion) {
    // Use BSBM file if provided (scale test); otherwise use our synthetic data.
    let quads = try_load_bsbm_nt().unwrap_or_else(|| generate_quads_large(LARGE_N));
    let n = quads.len();

    let mut group = c.benchmark_group("large/compact");
    group.measurement_time(Duration::from_secs(120));
    group.sample_size(10);
    group.throughput(Throughput::Elements(n as u64));

    group.bench_function("ring", |b| {
        b.iter_batched(
            || {
                // Setup (not timed): insert all triples into a fresh store.
                let store = RingStore::new();
                for q in &quads {
                    store.insert(q).unwrap();
                }
                store
            },
            |store| {
                // Timed: sort + dedup + build 6 LOUDS height-3 tries.
                store.compact().unwrap();
                black_box(store)
            },
            BatchSize::LargeInput,
        );
    });
    group.finish();
}

// ── Benchmark 2: Large ingest ─────────────────────────────────────────────────
//
// Measures insert() throughput at 1.25M triples.
// Each insert: intern terms → pack u128 key → BTreeMap insert (O(log n)).
//
// If BSBM_NT_FILE is set, reports BSBM ingest throughput for comparison.

fn bench_large_ingest(c: &mut Criterion) {
    let quads = try_load_bsbm_nt().unwrap_or_else(|| generate_quads_large(LARGE_N));
    let n = quads.len();

    let mut group = c.benchmark_group("large/ingest");
    group.measurement_time(Duration::from_secs(60));
    group.sample_size(10);
    group.throughput(Throughput::Elements(n as u64));

    group.bench_function("ring", |b| {
        b.iter(|| {
            let store = RingStore::new();
            for q in &quads {
                store.insert(q).unwrap();
            }
            black_box(store)
        });
    });
    group.finish();
}

// ── Benchmark 3: Feature lookup (POS trie — degree=500 sidecar node) ──────────
//
// SPARQL: SELECT ?p WHERE { ?p wdt:P2 wd:feature0 }
//
// BSBM analog: Q3/Q4 — "find all products with feature X".
//
// Uses POS trie: P=hasFeature bound (depth-0), O=feature0 bound (depth-1),
// iterate S (depth-2).  The POS trie node (hasFeature, feature0) has degree≈500
// (every ~100th entity has feature0) >> SIDECAR_THRESH=16.
//
// leap() on this node is the primary call site for sidecar optimization validation.
//
// Expected: ~500 solutions  (N × FAN_OUT_FEATURES / N_FEATURES = 500).

fn bench_large_feature_lookup(c: &mut Criterion) {
    let store = get_large_store();
    let ds = StoreDataset::new(Arc::clone(&store));

    // feature0 appears in entities where (i*FAN_OUT) % N_FEATURES == 0,
    // i.e., multiples of N_FEATURES/FAN_OUT = 100.  So 50k/100 = 500 entities.
    let sparql = format!("{PREFIXES}SELECT ?p WHERE {{ ?p wdt:P2 wd:feature0 }}");

    // Verify expected result count at startup.
    let expected_approx = LARGE_N * FAN_OUT_FEATURES / N_FEATURES; // ≈ 500
    let actual = count_solutions(&ds, &sparql);
    assert!(
        actual > 0 && (actual as isize - expected_approx as isize).unsigned_abs() < expected_approx,
        "feature_lookup: got {actual} solutions, expected ≈{expected_approx}"
    );
    eprintln!("[bsbm_large] feature_lookup: {actual} solutions (expected ≈{expected_approx})");

    let mut group = c.benchmark_group("large/feature_lookup");
    group.throughput(Throughput::Elements(actual as u64));

    group.bench_function("ring", |b| {
        b.iter(|| black_box(count_solutions(&ds, &sparql)))
    });
    group.finish();
}

// ── Benchmark 4: Class-region star (no sidecar) ───────────────────────────────
//
// SPARQL: SELECT ?p ?r WHERE { ?p wdt:P31 wd:class0 . ?p wdt:P131 ?r }
//
// BSBM analog: Q1/Q2 — "fetch properties of all products of a given type".
//
// Both SPO leaf nodes for class0 and the per-entity region have degree=1.
// This benchmark does NOT activate the sidecar — it establishes the baseline
// cost of two-pattern star joins at large scale.
//
// Expected: N / N_CLASSES = 50k / 50 = 1,000 solutions.

fn bench_large_star_class_region(c: &mut Criterion) {
    let store = get_large_store();
    let ds = StoreDataset::new(Arc::clone(&store));

    let sparql =
        format!("{PREFIXES}SELECT ?p ?r WHERE {{ ?p wdt:P31 wd:class0 . ?p wdt:P131 ?r }}");

    let expected = LARGE_N / N_CLASSES; // 1000
    let actual = count_solutions(&ds, &sparql);
    assert_eq!(
        actual, expected,
        "star_class_region: unexpected solution count"
    );
    eprintln!("[bsbm_large] star_class_region: {actual} solutions");

    let mut group = c.benchmark_group("large/star_class_region");
    group.throughput(Throughput::Elements(actual as u64));

    group.bench_function("ring", |b| {
        b.iter(|| black_box(count_solutions(&ds, &sparql)))
    });
    group.finish();
}

// ── Benchmark 5: Star with features (SPO trie — degree=20 sidecar node) ───────
//
// SPARQL: SELECT ?p ?f WHERE { ?p wdt:P31 wd:class0 . ?p wdt:P2 ?f }
//
// BSBM analog: Q2 extended — "all features of products in class0".
//
// This IS the primary sidecar optimization benchmark.  The join:
//   1. First pattern: bound P31+class0, iterate ?p (1,000 matching entities).
//   2. Second pattern: ?p bound, bound P2, iterate ?f (20 features per entity).
//
// LFTJ uses SPO ordering for step 2: seeks within node (entity_i, hasFeature)
// at depth-2, degree=20 ≥ SIDECAR_THRESH=16.  Every entity triggers the sidecar.
// `leap()` is called 1,000 × 20 = 20,000 times on sidecar nodes.
//
// Expected: (N / N_CLASSES) × FAN_OUT_FEATURES = 1,000 × 20 = 20,000 solutions.

fn bench_large_star_with_features(c: &mut Criterion) {
    let store = get_large_store();
    let ds = StoreDataset::new(Arc::clone(&store));

    // wdt:P2 = P_P2 = "https://www.wikidata.org/prop/direct/P2" (has-feature).
    let sparql = format!("{PREFIXES}SELECT ?p ?f WHERE {{ ?p wdt:P31 wd:class0 . ?p wdt:P2 ?f }}");

    let expected = (LARGE_N / N_CLASSES) * FAN_OUT_FEATURES; // 20_000
    let actual = count_solutions(&ds, &sparql);
    assert_eq!(
        actual, expected,
        "star_with_features: unexpected solution count"
    );
    eprintln!(
        "[bsbm_large] star_with_features: {actual} solutions \
         (hammers degree=20 sidecar nodes → validates optimization)"
    );

    let mut group = c.benchmark_group("large/star_with_features");
    group.throughput(Throughput::Elements(actual as u64));

    group.bench_function("ring", |b| {
        b.iter(|| black_box(count_solutions(&ds, &sparql)))
    });
    group.finish();
}

// ── Benchmark 6: Full star (class + region + features) ────────────────────────
//
// SPARQL: SELECT ?p ?r ?f WHERE { ?p wdt:P31 wd:class0 . ?p wdt:P131 ?r . ?p wdt:P2 ?f }
//
// BSBM analog: Q2 full — "all properties of all products in class0".
//
// 3-way star join.  The feature iteration still hammers the sidecar nodes
// (same as Benchmark 5), but now also retrieves regions.
//
// Expected: (N / N_CLASSES) × FAN_OUT_FEATURES = 20,000 solutions
// (region is a single value per entity; it multiplies into the feature loop).

fn bench_large_full_star(c: &mut Criterion) {
    let store = get_large_store();
    let ds = StoreDataset::new(Arc::clone(&store));

    let sparql = format!(
        "{PREFIXES}SELECT ?p ?r ?f WHERE {{ \
            ?p wdt:P31 wd:class0 . \
            ?p wdt:P131 ?r . \
            ?p wdt:P2 ?f \
        }}"
    );

    let expected = (LARGE_N / N_CLASSES) * FAN_OUT_FEATURES; // 20_000
    let actual = count_solutions(&ds, &sparql);
    assert_eq!(actual, expected, "full_star: unexpected solution count");
    eprintln!("[bsbm_large] full_star: {actual} solutions");

    let mut group = c.benchmark_group("large/full_star");
    group.throughput(Throughput::Elements(actual as u64));

    group.bench_function("ring", |b| {
        b.iter(|| black_box(count_solutions(&ds, &sparql)))
    });
    group.finish();
}

// ── Benchmark 7: Triangle at large N ─────────────────────────────────────────
//
// Find all directed triangles A→B→C→A in the `related` graph.
//
// Same query as wikidata_slice.rs but at N=50k (vs 10k there).  With
// FAN_OUT_RELATED=3, each entity has 3 outgoing edges → ~3×N = 150k triangles.
// related-to leaf nodes have degree=3 < SIDECAR_THRESH — does NOT activate
// the sidecar, providing a clean "sidecar-free" reference data point at large N.
//
// Expected: ≈150k triangles (3×N = 3×50,000 = 150,000).

fn bench_large_triangle(c: &mut Criterion) {
    let store = get_large_store();
    let ds = StoreDataset::new(Arc::clone(&store));

    let sparql = format!(
        "{PREFIXES}SELECT ?a ?b ?c WHERE {{ \
            ?a wdt:related ?b . \
            ?b wdt:related ?c . \
            ?a wdt:related ?c \
        }}"
    );

    let mut group = c.benchmark_group("large/triangle");
    group.measurement_time(Duration::from_secs(30));
    group.sample_size(10);

    group.bench_function("ring", |b| {
        b.iter(|| black_box(count_solutions(&ds, &sparql)))
    });
    group.finish();
}

// ── Benchmark 8: Large root domain (parallel LFTJ scaling) ────────────────────
//
// SPARQL: SELECT ?s ?f WHERE { ?s wdt:P2 ?f }
//
// Exercises the parallel root-dispatch path (`run_lftj_root` / chunked
// `par_chunks`) on a domain large enough that work-stealing has real work to
// do. Unlike `star_class_region` / `star_with_features` (root domain ≈ 1k after
// the class0 filter), this query has no constant filter on the subject, so
// adaptive VEO sees:
//
//   - ?f (feature objects of P2): pool size N_FEATURES = 2_000
//   - ?s (subjects with P2):      LARGE_N = 50_000
//
// VEO therefore picks ?f first (smaller real subtree). Root domain ≈ 2_000
// values, each expanding to ≈ LARGE_N × FAN_OUT_FEATURES / N_FEATURES = 500
// subjects → 1_000_000 solutions total. That is well above
// `PARALLEL_LFTJ_ROOT_THRESHOLD` (64) and each chunk's sequential loop does
// hundreds of microsecond-scale subtrees rather than sub-µs empty ones.
//
// ## Why this exists
//
// Phases A–C of parallel LFTJ (snapshot + per-worker decode caches + SmallVec)
// were validated on the class-filtered star shapes, where absolute speedups
// were large relative to the *previous parallel baseline* but the root domain
// itself was modest. This benchmark is the honest scaling probe: if chunked
// parallel LFTJ is paying off, `large/large_root` should show multi-core
// utilisation on a multi-million-solution join. Compare against a sequential
// baseline by temporarily forcing `PARALLEL_LFTJ_ROOT_THRESHOLD = usize::MAX`
// (Criterion's own `change:` is vs. its last saved run, not a fixed reference).
//
// Expected: LARGE_N × FAN_OUT_FEATURES = 50_000 × 20 = 1_000_000 solutions.

fn bench_large_root(c: &mut Criterion) {
    let store = get_large_store();
    let ds = StoreDataset::new(Arc::clone(&store));

    let sparql = format!("{PREFIXES}SELECT ?s ?f WHERE {{ ?s wdt:P2 ?f }}");

    let expected = LARGE_N * FAN_OUT_FEATURES; // 1_000_000
    let actual = count_solutions(&ds, &sparql);
    assert_eq!(actual, expected, "large_root: unexpected solution count");
    eprintln!(
        "[bsbm_large] large_root: {actual} solutions \
         (root domain ≈ {N_FEATURES} features × ~{} subjects — parallel LFTJ scaling probe)",
        LARGE_N * FAN_OUT_FEATURES / N_FEATURES
    );

    let mut group = c.benchmark_group("large/large_root");
    group.measurement_time(Duration::from_secs(30));
    group.sample_size(10);
    group.throughput(Throughput::Elements(actual as u64));

    group.bench_function("ring", |b| {
        b.iter(|| black_box(count_solutions(&ds, &sparql)))
    });
    group.finish();
}

// ── Benchmark 9: Skewed two-hop paths (power-law out-degree) ──────────────────
//
// SPARQL: SELECT ?a ?b ?c WHERE { ?a wdt:related ?b . ?b wdt:related ?c }
//
// HoneyComb (§1–2) shows that naively partitioning only the outermost join
// variable is skew-prone: a few hub values dominate wall-clock time and leave
// other workers idle. Our production path currently does a *single-level*
// chunked root split (PJDL-style enumerate-then-dispatch) and deliberately
// defers deeper adaptive splitting until a real skewed workload proves it
// necessary. This benchmark *is* that workload.
//
// ## Dataset
//
// SKEW_N = 20_000 entities, each with a power-law number of `related-to`
// outgoing edges:
//
//     out_degree(i) = clamp(SKEW_SCALE / (i + 1), 1, SKEW_MAX_OUT)
//
// Entity 0 is a hub with SKEW_MAX_OUT edges; the tail has degree 1. Total edge
// count is ≈ SKEW_SCALE · H_{SKEW_N} (harmonic sum) — tens of thousands, cheap
// to build. Targets are `(i + 1 + j) % SKEW_N` so the graph is a directed
// near-cycle with hub shortcuts — no self-loops.
//
// ## What the query stresses
//
// Adaptive VEO typically iterates ?a (or ?b) first over the set of subjects
// that have outgoing related-edges (≈ SKEW_N). Chunked parallel dispatch then
// hands contiguous slices of that domain to rayon workers. Because out-degree
// is power-law, early chunks (low entity ids) contain the hubs and do far more
// second-hop expansion than late chunks. Rayon's work-stealing *within* the
// fixed chunk set can rebalance unfinished chunks, but a single outsized hub
// *inside* a chunk still serialises that worker — exactly the residual skew
// HoneyComb measures. Tracking this number over time tells us when (if ever)
// a below-root adaptive split is warranted.
//
// Result cardinality is data-dependent (hubs dominate); we only assert > 0
// and report the count at startup.

/// Entities in the power-law skew dataset.
const SKEW_N: usize = 20_000;

/// Scale factor for out_degree(i) ≈ SKEW_SCALE / (i + 1).
/// Entity 0 would request SKEW_SCALE edges before the SKEW_MAX_OUT clamp.
const SKEW_SCALE: usize = 5_000;

/// Cap on any single node's out-degree (keeps hub expansion practical).
const SKEW_MAX_OUT: usize = 2_000;

#[inline]
fn skew_out_degree(i: usize) -> usize {
    (SKEW_SCALE / (i + 1)).clamp(1, SKEW_MAX_OUT)
}

/// Generate a directed power-law `related-to` graph over `n` entities.
///
/// Entity `i` emits `skew_out_degree(i)` edges to distinct targets
/// `(i + 1 + j) % n` for `j in 0..degree`, guaranteeing no self-loop.
fn generate_quads_skew(n: usize) -> Vec<Quad> {
    let p_rel = NamedNode::new_unchecked(P_REL);
    let dg = GraphName::DefaultGraph;
    // Upper bound: every node at SKEW_MAX_OUT (actual total is much smaller).
    let mut quads = Vec::with_capacity(n * 4);
    for i in 0..n {
        let subj = Subject::NamedNode(entity_iri(i));
        let degree = skew_out_degree(i);
        for j in 0..degree {
            let target = (i + 1 + j) % n;
            quads.push(Quad::new(
                subj.clone(),
                p_rel.clone(),
                Term::NamedNode(entity_iri(target)),
                dg.clone(),
            ));
        }
    }
    quads
}

static SKEW_STORE: OnceLock<Arc<RingStore>> = OnceLock::new();

fn get_skew_store() -> Arc<RingStore> {
    Arc::clone(SKEW_STORE.get_or_init(|| {
        let mut edge_count = 0usize;
        for i in 0..SKEW_N {
            edge_count += skew_out_degree(i);
        }
        eprintln!(
            "[bsbm_large] Building skew store: {SKEW_N} entities, \
             {edge_count} related-to edges (power-law, max_out={SKEW_MAX_OUT}) …"
        );
        let quads = generate_quads_skew(SKEW_N);
        let store = Arc::new(RingStore::new());
        for q in &quads {
            store.insert(q).unwrap();
        }
        eprintln!("[bsbm_large] Compacting skew store …");
        store.compact().unwrap();
        eprintln!(
            "[bsbm_large] Skew store ready. hub_out_degree={}, \
             tail_out_degree={}, triple_count={}",
            skew_out_degree(0),
            skew_out_degree(SKEW_N - 1),
            store.triple_count()
        );
        store
    }))
}

fn bench_skewed_paths(c: &mut Criterion) {
    let store = get_skew_store();
    let ds = StoreDataset::new(Arc::clone(&store));

    // Two-hop paths (not a closing triangle): every ?a→?b edge expands into
    // out_degree(b) continuations, so hub intermediates amplify fan-out.
    let sparql = format!(
        "{PREFIXES}SELECT ?a ?b ?c WHERE {{ \
            ?a wdt:related ?b . \
            ?b wdt:related ?c \
        }}"
    );

    let actual = count_solutions(&ds, &sparql);
    assert!(
        actual > 0,
        "skewed_paths: expected a non-empty 2-hop result on the power-law graph"
    );
    eprintln!(
        "[bsbm_large] skewed_paths: {actual} solutions \
         (power-law related graph, hub_out={})",
        skew_out_degree(0)
    );

    let mut group = c.benchmark_group("large/skewed_paths");
    group.measurement_time(Duration::from_secs(45));
    group.sample_size(10);
    group.throughput(Throughput::Elements(actual as u64));

    group.bench_function("ring", |b| {
        b.iter(|| black_box(count_solutions(&ds, &sparql)))
    });
    group.finish();
}

// ── Benchmark 10: Degree sweep (pin EF crossover threshold) ──────────────────
//
// Measures the `feature_lookup` query at six POS-trie degree levels:
//   16, 32, 64, 128, 256, 512 S-children per (P=has-feature, O=feature_j) node.
//
// ## Purpose
//
// Step 12 benchmarks showed:
//   - degree ≈ 500 (feature_lookup): EF −3.1% vs Slice (EF wins)
//   - degree = 20  (star_with_features): EF +0.8% (within noise; Slice holds)
//
// The crossover is somewhere in 20–500.  This sweep pins it per degree so
// EF_THRESH in louds.rs can be set to a measured, not estimated, value.
//
// ## Dataset design
//
// SWEEP_N=10k entities × SWEEP_FANOUT=4 features = 40k feature triples.
// pool_size(D) = SWEEP_N × SWEEP_FANOUT / D = 40_000 / D features.
//
// For feature0, exactly D entities (≈) map to it → POS node degree ≈ D.
//
// SPO fanout = 4 < SIDECAR_THRESH=16 → no SPO sidecar fires; only POS is exercised.
//
// ## How to run A/B per degree
//
// ```bash
// # Baseline (Slice for D<128, Ef for D≥128 — adaptive default):
// cargo bench -p oxigraph-nova-bench --bench bsbm_large -- 'large/degree_sweep'
//
// # Force Ef for ALL sidecar nodes (A/B reference — all degrees use Ef):
// cargo bench -p oxigraph-nova-bench --bench bsbm_large --features l-opt-ef -- 'large/degree_sweep'
// ```
//
// Criterion saves results under `target/criterion/large/degree_sweep/ring/N/`.
// Compare baseline vs l-opt-ef at each N to find the crossover.

/// Entities in the degree-sweep dataset.  Small enough for fast per-degree build.
const SWEEP_N: usize = 10_000;

/// Features per entity in the sweep.  Keep below SIDECAR_THRESH=16 so the
/// SPO sidecar does NOT fire — isolates the POS sidecar effect at each degree.
const SWEEP_FANOUT: usize = 4;

/// Degrees to sweep over. Spans SIDECAR_THRESH (16) to well past the known
/// crossover range (20–500) for complete coverage.
const SWEEP_DEGREES: &[usize] = &[16, 32, 64, 128, 256, 512];

/// Generate `n` entities with `fanout` has-feature triples each, cycling
/// through a pool of `pool_size` features.
///
/// Feature assignment: entity `i` gets features `(i * fanout + k) % pool_size`
/// for `k in 0..fanout`.  Feature 0 appears in entities where
/// `(i * fanout) % pool_size == 0` → degree ≈ n * fanout / pool_size.
///
/// Only has-feature triples are generated (no class/region/related-to),
/// so the only sidecar nodes that fire are in the POS trie.
fn generate_quads_sweep(n: usize, fanout: usize, pool_size: usize) -> Vec<Quad> {
    let p_feat = NamedNode::new_unchecked(P_P2);
    let dg = GraphName::DefaultGraph;
    let mut quads = Vec::with_capacity(n * fanout);
    for i in 0..n {
        let subj = Subject::NamedNode(entity_iri(i));
        for k in 0..fanout {
            let f = (i * fanout + k) % pool_size;
            quads.push(Quad::new(
                subj.clone(),
                p_feat.clone(),
                Term::NamedNode(feature_iri(f)),
                dg.clone(),
            ));
        }
    }
    quads
}

fn bench_degree_sweep(c: &mut Criterion) {
    let mut group = c.benchmark_group("large/degree_sweep");
    group.measurement_time(Duration::from_secs(10));
    group.sample_size(20);

    for &degree in SWEEP_DEGREES {
        // pool_size chosen so that the POS node (P=has-feature, O=feature0)
        // has approximately `degree` subject-children.
        let pool_size = (SWEEP_N * SWEEP_FANOUT / degree).max(1);

        // Build + compact a dedicated store for this degree level.
        // Not timed — setup cost is outside the measurement window.
        let quads = generate_quads_sweep(SWEEP_N, SWEEP_FANOUT, pool_size);
        let store = Arc::new(RingStore::new());
        for q in &quads {
            store.insert(q).unwrap();
        }
        store.compact().unwrap();
        let ds = StoreDataset::new(Arc::clone(&store));

        // Query: iterate all subjects with (P=has-feature, O=feature0).
        let sparql = format!("{PREFIXES}SELECT ?p WHERE {{ ?p wdt:P2 wd:feature0 }}");
        let actual = count_solutions(&ds, &sparql);
        assert!(
            actual > 0,
            "degree_sweep: degree={degree} pool={pool_size} yielded 0 solutions — check generator"
        );
        eprintln!(
            "[degree_sweep] degree={degree:>4} pool={pool_size:>5} actual={actual} solutions",
        );

        // Report solutions/s — allows normalised comparison across degrees.
        group.throughput(Throughput::Elements(actual as u64));
        group.bench_with_input(
            criterion::BenchmarkId::new("ring", degree),
            &degree,
            |b, _| b.iter(|| black_box(count_solutions(&ds, &sparql))),
        );
    }
    group.finish();
}

// ── Criterion registration ────────────────────────────────────────────────────

criterion_group!(
    benches,
    bench_large_compact,
    bench_large_ingest,
    bench_large_feature_lookup,
    bench_large_star_class_region,
    bench_large_star_with_features,
    bench_large_full_star,
    bench_large_triangle,
    bench_large_root,
    bench_skewed_paths,
    bench_degree_sweep,
);
criterion_main!(benches);
