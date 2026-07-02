//! Standalone dataset generator for external comparative benchmarks.
//!
//! Emits the same synthetic BSBM-style dataset used by `benches/bsbm_large.rs`
//! as a plain N-Triples file, so Nova, Oxigraph, and QLever can all be loaded
//! with byte-identical input for a fair comparison.
//!
//! # Usage
//!
//! ```bash
//! cargo run -p oxigraph-nova-bench --release --bin gen_dataset -- \
//!     --entities 50000 --out /tmp/bench_data/dataset.nt
//! ```
//!
//! Also writes a `<out>.queries.json` file alongside the dataset containing
//! the fixed SPARQL query set (with expected result counts) used by the
//! external comparison harness — computed once here so every engine is
//! queried with exactly the same SPARQL text.

use oxigraph_nova_bench::{generate_quads_large, triple_count, write_nt_line};
use std::env;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::time::Instant;

fn main() {
    let mut entities: usize = 50_000;
    let mut out: PathBuf = PathBuf::from("/tmp/oxigraph-nova-bench/dataset.nt");

    let args: Vec<String> = env::args().collect();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--entities" | "-n" => {
                i += 1;
                entities = args[i].parse().expect("--entities must be a number");
            }
            "--out" | "-o" => {
                i += 1;
                out = PathBuf::from(&args[i]);
            }
            "--help" | "-h" => {
                println!(
                    "Usage: gen_dataset [--entities N] [--out PATH]\n\
                     Generates N*(2+20+3) synthetic BSBM-style triples as N-Triples."
                );
                return;
            }
            other => panic!("unknown argument: {other}"),
        }
        i += 1;
    }

    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent).expect("failed to create output directory");
    }

    eprintln!(
        "[gen_dataset] Generating {entities} entities ({} triples) -> {}",
        triple_count(entities),
        out.display()
    );
    let t0 = Instant::now();
    let quads = generate_quads_large(entities);
    eprintln!(
        "[gen_dataset] Generated {} quads in {:.2}s",
        quads.len(),
        t0.elapsed().as_secs_f64()
    );

    let t1 = Instant::now();
    let file = File::create(&out).expect("failed to create output file");
    let mut w = BufWriter::new(file);
    for q in &quads {
        write_nt_line(&mut w, q).expect("failed to write N-Triples line");
    }
    w.flush().expect("failed to flush output file");
    eprintln!(
        "[gen_dataset] Wrote {} to disk in {:.2}s",
        out.display(),
        t1.elapsed().as_secs_f64()
    );

    // Write the fixed query set + metadata as JSON so the harness script and
    // all three engines run identical SPARQL text against identical data.
    let queries_path = out.with_extension("queries.json");
    let queries = build_query_set(entities);
    let mut qf =
        BufWriter::new(File::create(&queries_path).expect("failed to create queries file"));
    write!(qf, "{queries}").expect("failed to write queries file");
    eprintln!(
        "[gen_dataset] Wrote query set -> {}",
        queries_path.display()
    );
}

/// Build the fixed SPARQL query set as a JSON array of `{name, sparql, expected}`.
/// Kept in plain string formatting (no serde dependency needed) since the
/// harness script only needs to `jq` over it.
fn build_query_set(n: usize) -> String {
    let n_classes = oxigraph_nova_bench::N_CLASSES;
    let n_features = oxigraph_nova_bench::N_FEATURES;
    let fan_out_features = oxigraph_nova_bench::FAN_OUT_FEATURES;

    let expected_scan = n; // one instance-of triple per entity
    let expected_2join = n / n_classes;
    let expected_feature_lookup = n * fan_out_features / n_features;
    let expected_star = (n / n_classes) * fan_out_features;
    let expected_path = 0; // not asserted; informational only for path query
    let _ = expected_path;

    format!(
        r#"[
  {{
    "name": "scan",
    "sparql": "PREFIX wd: <https://www.wikidata.org/entity/> PREFIX wdt: <https://www.wikidata.org/prop/direct/> SELECT * WHERE {{ ?s wdt:P31 ?o }}",
    "expected": {expected_scan}
  }},
  {{
    "name": "2join",
    "sparql": "PREFIX wd: <https://www.wikidata.org/entity/> PREFIX wdt: <https://www.wikidata.org/prop/direct/> SELECT ?s ?region WHERE {{ ?s wdt:P31 wd:class0 . ?s wdt:P131 ?region }}",
    "expected": {expected_2join}
  }},
  {{
    "name": "feature_lookup",
    "sparql": "PREFIX wd: <https://www.wikidata.org/entity/> PREFIX wdt: <https://www.wikidata.org/prop/direct/> SELECT ?p WHERE {{ ?p wdt:P2 wd:feature0 }}",
    "expected": {expected_feature_lookup}
  }},
  {{
    "name": "star_with_features",
    "sparql": "PREFIX wd: <https://www.wikidata.org/entity/> PREFIX wdt: <https://www.wikidata.org/prop/direct/> SELECT ?p ?f WHERE {{ ?p wdt:P31 wd:class0 . ?p wdt:P2 ?f }}",
    "expected": {expected_star}
  }},
  {{
    "name": "path_2hop",
    "sparql": "PREFIX wd: <https://www.wikidata.org/entity/> PREFIX wdt: <https://www.wikidata.org/prop/direct/> SELECT ?a ?b ?c WHERE {{ ?a wdt:related ?b . ?b wdt:related ?c }}",
    "expected": null
  }},
  {{
    "name": "triangle",
    "sparql": "PREFIX wd: <https://www.wikidata.org/entity/> PREFIX wdt: <https://www.wikidata.org/prop/direct/> SELECT ?a ?b ?c WHERE {{ ?a wdt:related ?b . ?b wdt:related ?c . ?a wdt:related ?c }}",
    "expected": null
  }}
]
"#
    )
}
