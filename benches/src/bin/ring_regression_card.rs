//! Phase 0 — Ring memory/CPU regression card (measure only).
//!
//! Component `RingMemBreakdown` + product dual-residency matrix at N=20k realistic.
//!
//! ```bash
//! cargo run -p oxigraph-nova-bench --release --features cyclic-ring-pilot \
//!   --bin ring_regression_card -- 20000 realistic
//!
//! cargo run -p oxigraph-nova-bench --release --features ring-huffman-cp \
//!   --bin ring_regression_card -- 20000 realistic
//! ```

#![cfg_attr(not(feature = "cyclic-ring-pilot"), allow(dead_code, unused_imports))]

use mimalloc::MiMalloc;
use oxigraph_nova_bench::{generate_quads_large, generate_quads_realistic};
use oxigraph_nova_core::{Dictionary, GraphName, Quad, Subject, Term};
use std::collections::{BTreeSet, HashMap};
use std::env;
use std::time::Instant;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

#[cfg(feature = "cyclic-ring-pilot")]
use oxigraph_nova_storage_ring::{
    BraidedGraphImage, BraidedRingIndex, CyclicRing, RingMemBreakdown, write_novarng1_v1,
};

fn densify_role_local(quads: &[Quad]) -> (Vec<[u32; 3]>, u32, u32, u32) {
    let mut dict = Dictionary::new();
    let mut globals = Vec::new();
    for q in quads {
        if q.graph_name != GraphName::DefaultGraph {
            continue;
        }
        let s = match &q.subject {
            Subject::NamedNode(n) => dict.intern(&Term::NamedNode(n.clone())).unwrap().as_u64(),
            Subject::BlankNode(b) => dict.intern(&Term::BlankNode(b.clone())).unwrap().as_u64(),
            #[allow(unreachable_patterns)]
            _ => continue,
        };
        let p = dict.intern_predicate(&q.predicate).unwrap().as_u64();
        let o = dict.intern(&q.object).unwrap().as_u64();
        globals.push([s, p, o]);
    }
    globals.sort_unstable();
    globals.dedup();
    let mut map_s = HashMap::new();
    let mut map_p = HashMap::new();
    let mut map_o = HashMap::new();
    let mut ns = 0u32;
    let mut np = 0u32;
    let mut no = 0u32;
    for &[s, p, o] in &globals {
        map_s.entry(s).or_insert_with(|| {
            let v = ns;
            ns += 1;
            v
        });
        map_p.entry(p).or_insert_with(|| {
            let v = np;
            np += 1;
            v
        });
        map_o.entry(o).or_insert_with(|| {
            let v = no;
            no += 1;
            v
        });
    }
    let rows = globals
        .iter()
        .map(|&[s, p, o]| [map_s[&s], map_p[&p], map_o[&o]])
        .collect();
    (rows, ns, np, no)
}

fn densify_shared_compact(quads: &[Quad]) -> (Vec<[u32; 3]>, u32, Vec<[u64; 3]>) {
    let mut dict = Dictionary::new();
    let mut globals = Vec::new();
    for q in quads {
        if q.graph_name != GraphName::DefaultGraph {
            continue;
        }
        let s = match &q.subject {
            Subject::NamedNode(n) => dict.intern(&Term::NamedNode(n.clone())).unwrap().as_u64(),
            Subject::BlankNode(b) => dict.intern(&Term::BlankNode(b.clone())).unwrap().as_u64(),
            #[allow(unreachable_patterns)]
            _ => continue,
        };
        let p = dict.intern_predicate(&q.predicate).unwrap().as_u64();
        let o = dict.intern(&q.object).unwrap().as_u64();
        globals.push([s, p, o]);
    }
    globals.sort_unstable();
    globals.dedup();
    let mut symbols = BTreeSet::new();
    for t in &globals {
        for &id in t {
            symbols.insert(id);
        }
    }
    let map: HashMap<u64, u32> = symbols
        .into_iter()
        .enumerate()
        .map(|(i, e)| (e, i as u32))
        .collect();
    let u = map.len() as u32;
    let dense: Vec<[u32; 3]> = globals
        .iter()
        .map(|&[s, p, o]| [map[&s], map[&p], map[&o]])
        .collect();
    (dense, u, globals)
}

#[cfg(feature = "cyclic-ring-pilot")]
fn mib(b: usize) -> f64 {
    b as f64 / (1024.0 * 1024.0)
}

#[cfg(feature = "cyclic-ring-pilot")]
fn print_bd(label: &str, bd: &RingMemBreakdown, mmap_bytes: Option<usize>, heap_resident: bool) {
    println!("--- {label} ---");
    println!(
        "  n={} U={} ns={} np={} no={} huff_cp={} heap_resident={}",
        bd.n, bd.universe, bd.ns, bd.np, bd.no, bd.c_p_is_huff, heap_resident
    );
    println!(
        "  C_o={:.3} MiB  C_p={:.3} MiB  C_s={:.3} MiB  QWT_sum={:.3} MiB",
        mib(bd.c_o),
        mib(bd.c_p),
        mib(bd.c_s),
        mib(bd.qwt_total())
    );
    println!(
        "  A_o={:.3} MiB  A_p={:.3} MiB  A_s={:.3} MiB  A_sum={:.3} MiB  (A_len=U+1={})",
        mib(bd.a_o),
        mib(bd.a_p),
        mib(bd.a_s),
        mib(bd.a_total()),
        bd.universe.saturating_add(1)
    );
    println!(
        "  shell={} B  total={:.3} MiB  ({:.2} B/tri)",
        bd.shell,
        mib(bd.total()),
        bd.bytes_per_triple()
    );
    if let Some(mb) = mmap_bytes {
        println!("  NOVARNG1 image={:.3} MiB", mib(mb));
        let dual = if heap_resident { bd.total() + mb } else { mb };
        println!(
            "  residency_accounted={:.3} MiB (heap+mmap if both hot; mmap-only if heap dropped)",
            mib(dual)
        );
    }
    println!();
}

#[cfg(feature = "cyclic-ring-pilot")]
fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    let n: usize = args.first().and_then(|s| s.parse().ok()).unwrap_or(20_000);
    let corpus = args.get(1).map(|s| s.as_str()).unwrap_or("realistic");

    println!("=== Ring regression card (Phase 0) ===");
    println!("n={n} corpus={corpus}");
    println!("ring-huffman-cp={}", cfg!(feature = "ring-huffman-cp"));
    println!();

    let quads = match corpus {
        "synthetic" | "bsbm" => generate_quads_large(n),
        _ => generate_quads_realistic(n),
    };
    let (role_local, ns, np, no) = densify_role_local(&quads);
    let (shared, u_shared, ext) = densify_shared_compact(&quads);
    let n_tri = role_local.len() as u32;
    println!(
        "triples={n_tri} role-local ns={ns} np={np} no={no} U_role={} | shared U={u_shared}",
        ns + np + no
    );
    println!();

    let t0 = Instant::now();
    let role_qwt = CyclicRing::build_from_role_local_qwt_cp(&role_local, ns, np, no);
    let ms = t0.elapsed().as_secs_f64() * 1e3;
    print_bd(
        &format!("role-local Qwt C_p (build {ms:.1} ms)"),
        &role_qwt.mem_breakdown(),
        write_novarng1_v1(&role_qwt).ok().map(|b| b.len()),
        true,
    );

    let t0 = Instant::now();
    let shared_qwt = CyclicRing::build_shared_qwt_cp(&shared, u_shared);
    let ms = t0.elapsed().as_secs_f64() * 1e3;
    let img_qwt = write_novarng1_v1(&shared_qwt).unwrap();
    print_bd(
        &format!("shared-Σ Qwt C_p (build {ms:.1} ms) — product alphabet"),
        &shared_qwt.mem_breakdown(),
        Some(img_qwt.len()),
        true,
    );

    let t0 = Instant::now();
    let shared_def = CyclicRing::build_shared(&shared, u_shared);
    let ms = t0.elapsed().as_secs_f64() * 1e3;
    let img_def = write_novarng1_v1(&shared_def).unwrap();
    print_bd(
        &format!(
            "shared-Σ default builder huff={} (build {ms:.1} ms)",
            shared_def.c_p_is_huff()
        ),
        &shared_def.mem_breakdown(),
        Some(img_def.len()),
        true,
    );

    #[cfg(feature = "ring-huffman-cp")]
    {
        let t0 = Instant::now();
        let role_huff = CyclicRing::build_from_role_local_huff_cp(&role_local, ns, np, no);
        let ms = t0.elapsed().as_secs_f64() * 1e3;
        let img = write_novarng1_v1(&role_huff).unwrap();
        print_bd(
            &format!("role-local Huff C_p (build {ms:.1} ms)"),
            &role_huff.mem_breakdown(),
            Some(img.len()),
            true,
        );
    }

    // Product path + dual residency
    let t0 = Instant::now();
    let mut idx = BraidedRingIndex::from_shared_triples(&shared, u_shared);
    let build_ms = t0.elapsed().as_secs_f64() * 1e3;
    let heap_bd = idx.heap().unwrap().mem_breakdown();
    let path = std::env::temp_dir().join(format!(
        "ring_reg_card_{}_{}.novarng1",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));

    // Dual residency (KEEP)
    idx.materialize_mapped_ex(&path, true).expect("mmap keep");
    let mmap_sz = idx.mapped().unwrap().image_bytes();
    print_bd(
        &format!("product shared idx + KEEP_HEAP (build {build_ms:.1} ms)"),
        &heap_bd,
        Some(mmap_sz),
        true,
    );
    assert!(idx.has_heap() && idx.has_mapped());

    // Single residency (default drop)
    let mut idx2 = BraidedRingIndex::from_shared_triples(&shared, u_shared);
    idx2.materialize_mapped_ex(&path, false).expect("mmap drop");
    let mmap_sz2 = idx2.mapped().unwrap().image_bytes();
    println!("--- product shared idx + DROP heap (Phase 1A default) ---");
    println!(
        "  has_heap={} has_mapped={} mmap={:.3} MiB  heap_bytes_reclaimed≈{:.3} MiB",
        idx2.has_heap(),
        idx2.has_mapped(),
        mib(mmap_sz2),
        mib(heap_bd.total())
    );
    println!(
        "  residency_accounted={:.3} MiB (mmap only)",
        mib(mmap_sz2)
    );
    println!();

    // BraidedGraphImage product path
    let t0 = Instant::now();
    let mut gimg = BraidedGraphImage::from_external_triples(&ext);
    let gms = t0.elapsed().as_secs_f64() * 1e3;
    gimg.materialize_mapped(&path).expect("image mmap");
    println!("--- BraidedGraphImage::from_external_triples (build {gms:.1} ms) ---");
    println!(
        "  n={} U_remap={} has_heap={} has_mapped={} mmap={:.3} MiB",
        gimg.n(),
        gimg.universe(),
        gimg.index().has_heap(),
        gimg.has_mapped(),
        mib(gimg.index().mapped().map(|m| m.image_bytes()).unwrap_or(0))
    );
    println!();

    let _ = std::fs::remove_file(&path);
    println!("=== end regression card ===");
}

#[cfg(not(feature = "cyclic-ring-pilot"))]
fn main() {
    eprintln!("error: build with --features cyclic-ring-pilot");
    std::process::exit(2);
}
