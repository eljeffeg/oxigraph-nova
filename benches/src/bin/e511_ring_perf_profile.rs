//! E5.11 / Braided Ring — product perf profile (fair LOUDS + D2).
//!
//! **Default path:** range locality, star/triangle vs fair LOUDS, D1/D2
//! product gates. DROP-branch matrices (B5/C1/D3/D4/E1) require
//! `--features diagnostics` and the `--full-campaign` CLI flag.
//!
//! ```bash
//! # Product gates (D1/D2 + fair LOUDS):
//! cargo run -p oxigraph-nova-bench --release --features cyclic-ring-pilot \
//!   --bin e511_ring_perf_profile -- 20000 realistic
//!
//! # Full historical campaign (needs diagnostics):
//! cargo run -p oxigraph-nova-bench --release \
//!   --features cyclic-ring-pilot,diagnostics \
//!   --bin e511_ring_perf_profile -- 20000 realistic --full-campaign
//! ```
//!
//! Status: `research/BRAIDED_RING.md` · campaign note: `research/notes/e5.11-ring-performance.md`.

#![cfg_attr(not(feature = "cyclic-ring-pilot"), allow(dead_code, unused_imports))]

use mimalloc::MiMalloc;
use std::collections::HashMap;
use std::env;
use std::hint::black_box;
use std::time::Instant;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

#[cfg(feature = "cyclic-ring-pilot")]
fn main() {
    use oxigraph_nova_bench::{generate_quads_large, generate_quads_realistic};
    // Intersection2Stats used via MappedRingA::intersection_next_value2_counted
    use oxigraph_nova_storage_ring::build_louds_from_sorted;
    use oxigraph_nova_storage_ring::cyclic_ring::mapped_qwt::c1_batch_k;
    #[cfg(feature = "diagnostics")]
    use oxigraph_nova_storage_ring::cyclic_ring::mapped_qwt::{
        C1_K_DEFAULT, PrefetchPolicy, PresenceCellSize, PresenceSummaryTable, prefetch_policy,
        set_c1_batch_k, set_prefetch_policy,
    };
    use oxigraph_nova_storage_ring::cyclic_ring::mapped_ring::{
        open_novarng1_mmap, write_novarng1_file,
    };
    use oxigraph_nova_storage_ring::cyclic_ring::{Col, CyclicRing, RowRange};
    use std::fs;

    let args: Vec<String> = env::args().skip(1).collect();
    let full_campaign = args.iter().any(|a| a == "--full-campaign");
    let pos: Vec<&str> = args
        .iter()
        .map(String::as_str)
        .filter(|a| *a != "--full-campaign")
        .collect();
    let n: usize = pos.first().and_then(|s| s.parse().ok()).unwrap_or(20_000);
    let corpus = pos
        .get(1)
        .map(|s| s.to_string())
        .unwrap_or_else(|| "realistic".to_string());

    println!("=== Braided Ring perf profile (E5.11 / F0) ===");
    println!("n={n} corpus={corpus} full_campaign={full_campaign}");
    if full_campaign {
        #[cfg(not(feature = "diagnostics"))]
        {
            eprintln!(
                "error: --full-campaign requires --features cyclic-ring-pilot,diagnostics"
            );
            std::process::exit(2);
        }
    }
    println!();

    let quads = match corpus.as_str() {
        "synthetic" | "bsbm" => generate_quads_large(n),
        _ => generate_quads_realistic(n),
    };
    let (triples, ns, np, no) = densify_spo(&quads);
    let n_tri = triples.len() as u32;
    println!(
        "triples={n_tri}  ns={ns} np={np} no={no}  U={}",
        ns + np + no
    );

    let t0 = Instant::now();
    let ring = CyclicRing::build_from_role_local(&triples, ns, np, no);
    let build_ms = t0.elapsed().as_secs_f64() * 1e3;
    let pilot_bytes = ring.mem_bytes();
    println!(
        "pilot CyclicRing: n={} U={} mem_bytes={pilot_bytes} B ({:.3} MiB) build={build_ms:.1} ms",
        ring.n,
        ring.universe,
        pilot_bytes as f64 / (1024.0 * 1024.0)
    );

    let path = std::env::temp_dir().join(format!("e511_novarng1_{}_{}.bin", std::process::id(), n));
    write_novarng1_file(&path, &ring).expect("write");
    let t_open = Instant::now();
    let mapped = open_novarng1_mmap(&path).expect("mmap");
    let open_ms = t_open.elapsed().as_secs_f64() * 1e3;
    let image_bytes = mapped.image_bytes();
    let overhead_pct =
        100.0 * (image_bytes as f64 - pilot_bytes as f64) / pilot_bytes.max(1) as f64;
    println!(
        "mmap open: {open_ms:.3} ms  image={image_bytes} B ({:.3} MiB)  overhead={overhead_pct:.2}%",
        image_bytes as f64 / (1024.0 * 1024.0)
    );

    // Shared-alphabet SPO for LOUDS
    let mut spo_shared: Vec<[u32; 3]> = triples
        .iter()
        .map(|&[s, p, o]| [s, ns + p, ns + np + o])
        .collect();
    spo_shared.sort_unstable();
    let louds = build_louds_from_sorted(&spo_shared).expect("louds");

    let star_seeds: Vec<u32> = (0..128u32).map(|k| (k * 97) % ns.max(1)).collect();
    let tri_seeds: Vec<u32> = (0..64u32).map(|k| (k * 131) % ns.max(1)).collect();

    // ── Range locality under star ranges ──────────────────────────────────
    println!();
    println!("--- range locality (star ranges on C_o) ---");
    let mut n_ranges = 0u64;
    let mut sum_len = 0u64;
    let mut same_block = 0u64;
    let mut same_sb = 0u64;
    let mut empty = 0u64;
    let mut hist = [0u64; 8]; // buckets: 0, 1, 2-3, 4-7, 8-15, 16-31, 32-63, 64+
    for &s in &star_seeds {
        let r = ring.range_s(s);
        if r.is_empty() {
            empty += 1;
            continue;
        }
        n_ranges += 1;
        let len = r.len() as u64;
        sum_len += len;
        let lo = r.start as usize;
        let hi = (r.end as usize).saturating_sub(1);
        let b_lo = lo / 256;
        let b_hi = hi / 256;
        let sb_lo = b_lo / 8;
        let sb_hi = b_hi / 8;
        if b_lo == b_hi {
            same_block += 1;
        }
        if sb_lo == sb_hi {
            same_sb += 1;
        }
        let bucket = match len {
            0 => 0,
            1 => 1,
            2..=3 => 2,
            4..=7 => 3,
            8..=15 => 4,
            16..=31 => 5,
            32..=63 => 6,
            _ => 7,
        };
        hist[bucket] += 1;
    }
    let avg_len = if n_ranges > 0 {
        sum_len as f64 / n_ranges as f64
    } else {
        0.0
    };
    println!(
        "star ranges: non_empty={n_ranges} empty={empty} avg_len={avg_len:.2}  same_block={same_block} ({:.1}%)  same_sb={same_sb} ({:.1}%)",
        100.0 * same_block as f64 / n_ranges.max(1) as f64,
        100.0 * same_sb as f64 / n_ranges.max(1) as f64
    );
    println!("len hist [0,1,2-3,4-7,8-15,16-31,32-63,64+]: {:?}", hist);

    // ── RDI counters: heap vs mmap ────────────────────────────────────────
    // With C1 batch (default K=32), symbol/count identity still required;
    // rank_probes/frames/empty_br only match when K=0 (generic RDI path).
    println!();
    println!(
        "--- RDI counters (star, C_o under range_s); c1_k={} ---",
        c1_batch_k()
    );
    let mut h_c = RdiAgg::default();
    let mut m_c = RdiAgg::default();
    // E5.11 B2 path counters (mmap fused range_counts4 only; zero on C1 batch)
    let mut b2_calls = 0u64;
    let mut b2_same_line = 0u64;
    let mut b2_same_sb = 0u64;
    let mut b2_general = 0u64;
    let mut b2_dl_loads = 0u64;
    let mut b2_sb_loads = 0u64;
    // E5.11 C1 batch counters
    let mut c1_attempts = 0u64;
    let mut c1_hits = 0u64;
    let mut c1_fallbacks = 0u64;
    let mut c1_rows = 0u64;
    let mut c1_levels = 0u64;
    let mut c1_dl = 0u64;
    for &s in &star_seeds {
        let r = ring.range_s(s);
        if r.is_empty() {
            continue;
        }
        // heap
        {
            let mut it = ring.range_distinct_iter(Col::O, r);
            while let Some(x) = it.next() {
                black_box(x);
            }
            h_c.opens += 1;
            h_c.symbols += it.symbols_yielded();
            h_c.rank_probes += it.rank_probes();
            h_c.frames += it.frames_popped();
            h_c.empty_br += it.empty_branches();
            h_c.children += it.children_pushed();
            h_c.branch_tr += it.branch_transitions();
            // heap qwt does not expose unary_collapses; leave 0
        }
        // mmap
        {
            let mr = mapped.range_s(s).unwrap_or(RowRange::empty());
            let mut it = mapped.range_distinct_iter(Col::O, mr).unwrap();
            while let Some(x) = it.next_symbol() {
                black_box(x);
            }
            m_c.opens += 1;
            m_c.symbols += it.symbols_yielded;
            m_c.rank_probes += it.rank_probes;
            m_c.frames += it.frames_popped;
            m_c.empty_br += it.empty_branches;
            m_c.children += it.children_pushed;
            m_c.branch_tr += it.branch_transitions;
            m_c.unary += it.unary_collapses;
            b2_calls += it.range_counts_calls;
            b2_same_line += it.same_line_hits;
            b2_same_sb += it.same_superblock_hits;
            b2_general += it.general_hits;
            b2_dl_loads += it.data_line_loads;
            b2_sb_loads += it.superblock_loads;
            c1_attempts += it.batch_attempts;
            c1_hits += it.batch_hits;
            c1_fallbacks += it.batch_fallbacks;
            c1_rows += it.batch_rows_decoded;
            c1_levels += it.batch_levels_processed;
            c1_dl += it.batch_data_line_loads;
        }
    }
    // Symbol identity always required; full RDI counter identity only when C1 off.
    let symbol_match = h_c.symbols == m_c.symbols;
    let counter_match = symbol_match
        && h_c.rank_probes == m_c.rank_probes
        && h_c.frames == m_c.frames
        && h_c.empty_br == m_c.empty_br
        && h_c.children == m_c.children;
    println!("heap:  {}", h_c.fmt());
    println!("mmap:  {}", m_c.fmt());
    println!(
        "symbol identity: {}  full RDI counter identity: {}{}",
        if symbol_match { "MATCH" } else { "MISMATCH" },
        if counter_match {
            "MATCH"
        } else {
            "N/A (C1 batch path)"
        },
        if c1_batch_k() == 0 && !counter_match {
            " — MISMATCH"
        } else {
            ""
        }
    );
    if c1_attempts > 0 {
        println!(
            "C1 batch: attempts={c1_attempts} hits={c1_hits} ({:.1}%) fallbacks={c1_fallbacks} rows={c1_rows} levels_sum={c1_levels} data_line_loads={c1_dl}",
            100.0 * c1_hits as f64 / c1_attempts.max(1) as f64
        );
    }

    if h_c.symbols > 0 {
        println!(
            "per-symbol: rank_probes={:.2} frames={:.2} empty_br={:.2} children={:.2}  (mmap unary={:.2})",
            h_c.rank_probes as f64 / h_c.symbols as f64,
            h_c.frames as f64 / h_c.symbols as f64,
            h_c.empty_br as f64 / h_c.symbols as f64,
            h_c.children as f64 / h_c.symbols as f64,
            m_c.unary as f64 / m_c.symbols.max(1) as f64
        );
    }
    // B2 fused expand path mix (all RDI expands under star, not only top-level ranges)
    if b2_calls > 0 {
        println!();
        println!("--- B2 range_counts4 path mix (mmap RDI expands) ---");
        println!(
            "calls={b2_calls}  same_line={b2_same_line} ({:.1}%)  same_sb={b2_same_sb} ({:.1}%)  general={b2_general} ({:.1}%)",
            100.0 * b2_same_line as f64 / b2_calls as f64,
            100.0 * b2_same_sb as f64 / b2_calls as f64,
            100.0 * b2_general as f64 / b2_calls as f64
        );
        println!(
            "loads: data_line={b2_dl_loads} ({:.2}/call)  superblock={b2_sb_loads} ({:.2}/call)",
            b2_dl_loads as f64 / b2_calls as f64,
            b2_sb_loads as f64 / b2_calls as f64
        );
    }

    // ── Timed star ────────────────────────────────────────────────────────
    println!();
    println!("--- star latency (128 subjects, RDI-O) ---");
    let star_rounds = 11usize;
    // warmup
    for &s in star_seeds.iter().take(8) {
        black_box(star_heap(&ring, s));
        black_box(star_mmap(&mapped, s));
        black_box(louds_star(&louds, s));
    }
    let mut heap_ms = Vec::with_capacity(star_rounds);
    let mut mmap_ms = Vec::with_capacity(star_rounds);
    let mut louds_ms = Vec::with_capacity(star_rounds);
    for _ in 0..star_rounds {
        let t = Instant::now();
        let mut total = 0u32;
        for &s in &star_seeds {
            total = total.wrapping_add(star_heap(&ring, s));
        }
        black_box(total);
        heap_ms.push(t.elapsed().as_secs_f64() * 1e3);

        let t = Instant::now();
        let mut total = 0u32;
        for &s in &star_seeds {
            total = total.wrapping_add(star_mmap(&mapped, s));
        }
        black_box(total);
        mmap_ms.push(t.elapsed().as_secs_f64() * 1e3);

        let t = Instant::now();
        let mut total = 0u32;
        for &s in &star_seeds {
            total = total.wrapping_add(louds_star(&louds, s));
        }
        black_box(total);
        louds_ms.push(t.elapsed().as_secs_f64() * 1e3);
    }
    let h_star = median_f64(&mut heap_ms);
    let m_star = median_f64(&mut mmap_ms);
    let l_star = median_f64(&mut louds_ms);
    let star_mh = ratio(m_star, h_star);
    let star_hl = ratio(h_star, l_star);
    let star_ml = ratio(m_star, l_star);
    println!("star median ms: heap={h_star:.4}  mmap={m_star:.4}  louds={l_star:.4}");
    println!("  mmap/heap={star_mh:.3}×  heap/LOUDS={star_hl:.3}×  mmap/LOUDS={star_ml:.3}×");

    // ── RDI ns/symbol (full C_o) ──────────────────────────────────────────
    let full = RowRange::full(ring.n);
    let rounds = 21usize;
    let mut h_rdi = Vec::with_capacity(rounds);
    let mut m_rdi = Vec::with_capacity(rounds);
    let mut nsym = 0u64;
    for _ in 0..rounds {
        let t = Instant::now();
        let mut acc = 0u64;
        let mut ns_ = 0u64;
        let mut it = ring.range_distinct_iter(Col::O, full);
        while let Some((s, c)) = it.next() {
            acc = acc.wrapping_add(s as u64).wrapping_add(c as u64);
            ns_ += 1;
        }
        black_box(acc);
        h_rdi.push(t.elapsed().as_nanos() as u64);
        nsym = ns_;

        let t = Instant::now();
        let mut acc = 0u64;
        let mut it = mapped.range_distinct_iter(Col::O, full).unwrap();
        while let Some((s, c)) = it.next_symbol() {
            acc = acc.wrapping_add(s as u64).wrapping_add(c as u64);
        }
        black_box(acc);
        m_rdi.push(t.elapsed().as_nanos() as u64);
    }
    h_rdi.sort_unstable();
    m_rdi.sort_unstable();
    let nf = nsym.max(1) as f64;
    let h_ns = h_rdi[h_rdi.len() / 2] as f64 / nf;
    let m_ns = m_rdi[m_rdi.len() / 2] as f64 / nf;
    let rdi_ratio = ratio(m_ns, h_ns);
    println!(
        "RDI full C_o (ns/sym): heap={h_ns:.2}  mmap={m_ns:.2}  ratio={rdi_ratio:.3}×  n_sym={nsym}"
    );

    // ── Triangle ──────────────────────────────────────────────────────────
    println!();
    println!(
        "--- triangle latency (64 seeds, D2 three-range common-object existence; fair LOUDS) ---"
    );
    // Semantic gate: Ring D2 Boolean hit and first-common O must match fair LOUDS.
    // Both sides use the shared alphabet from build_from_role_local / spo_shared:
    // S∈[0,ns), P∈[ns,ns+np), O∈[ns+np,U). Compare shared O labels directly.
    let mut tri_semantic_mismatches = 0u64;
    let mut tri_ring_hits = 0u64;
    let mut tri_louds_hits = 0u64;
    let mut tri_legacy_open_hits = 0u64;
    for &a in &tri_seeds {
        let ring_hits = triangle_mmap(&mapped, a, ns);
        let fair_hits = louds_triangle(&louds, a, ns);
        let legacy_hits = louds_subject_open_probe(&louds, a, ns);
        tri_ring_hits = tri_ring_hits.wrapping_add(ring_hits);
        tri_louds_hits = tri_louds_hits.wrapping_add(fair_hits);
        tri_legacy_open_hits = tri_legacy_open_hits.wrapping_add(legacy_hits);
        if ring_hits != fair_hits {
            tri_semantic_mismatches += 1;
        }
        // Pin first-common shared-alphabet O when present.
        for d in 1..=3u32 {
            let b = (a + d) % ns.max(1);
            let c = (b + 1) % ns.max(1);
            let ra = mapped.range_s(a).unwrap_or(RowRange::empty());
            let rb = mapped.range_s(b).unwrap_or(RowRange::empty());
            let rc = mapped.range_s(c).unwrap_or(RowRange::empty());
            let ring_shared = mapped.intersection_next_value3(Col::O, ra, rb, rc, 0);
            let louds_shared = louds_first_common_object(&louds, a, b, c);
            if ring_shared != louds_shared {
                tri_semantic_mismatches += 1;
            }
        }
    }

    println!(
        "triangle semantic gate: mismatches={tri_semantic_mismatches} ring_hits={tri_ring_hits} fair_louds_hits={tri_louds_hits} legacy_open_hits={tri_legacy_open_hits}"
    );
    if tri_semantic_mismatches != 0 {
        println!("STOP: fair LOUDS triangle is not semantically equivalent to Ring D2");
        std::process::exit(1);
    }

    let tri_rounds = 9usize;
    for &a in tri_seeds.iter().take(4) {
        black_box(triangle_heap(&ring, a, ns));
        black_box(triangle_mmap(&mapped, a, ns));
        black_box(louds_triangle(&louds, a, ns));
    }
    let mut h_tri = Vec::with_capacity(tri_rounds);
    let mut m_tri = Vec::with_capacity(tri_rounds);
    let mut l_tri = Vec::with_capacity(tri_rounds);
    let mut rnv_calls = 0u64;
    for _ in 0..tri_rounds {
        let t = Instant::now();
        let mut hits = 0u64;
        for &a in &tri_seeds {
            hits = hits.wrapping_add(triangle_heap(&ring, a, ns));
        }
        black_box(hits);
        h_tri.push(t.elapsed().as_secs_f64() * 1e3);

        let t = Instant::now();
        let mut hits = 0u64;
        for &a in &tri_seeds {
            hits = hits.wrapping_add(triangle_mmap(&mapped, a, ns));
        }
        black_box(hits);
        m_tri.push(t.elapsed().as_secs_f64() * 1e3);

        let t = Instant::now();
        let mut hits = 0u64;
        for &a in &tri_seeds {
            hits = hits.wrapping_add(louds_triangle(&louds, a, ns));
        }
        black_box(hits);
        l_tri.push(t.elapsed().as_secs_f64() * 1e3);
    }
    // Count RNV calls once
    for &a in &tri_seeds {
        rnv_calls += triangle_rnv_count(&ring, a, ns);
    }
    let h_t = median_f64(&mut h_tri);
    let m_t = median_f64(&mut m_tri);
    let l_t = median_f64(&mut l_tri);
    let tri_mh = ratio(m_t, h_t);
    let tri_hl = ratio(h_t, l_t);
    let tri_ml = ratio(m_t, l_t);
    println!("tri median ms: heap={h_t:.4}  mmap={m_t:.4}  fair_louds={l_t:.4}");
    println!(
        "  mmap/heap={tri_mh:.3}×  heap/fair_LOUDS={tri_hl:.3}×  mmap/fair_LOUDS={tri_ml:.3}×  rnv_calls={rnv_calls}"
    );

    // ── Rank micro (C_o mid-symbol) ───────────────────────────────────────
    println!();
    println!("--- rank micro (C_o, mid symbol) ---");
    let rank_sym = ring.access(Col::O, ring.n / 2);
    let rank_pos: Vec<u32> = (0..ring.n.min(4_000)).step_by(3).collect();
    let mut h_rk = Vec::with_capacity(rounds);
    let mut m_rk = Vec::with_capacity(rounds);
    for _ in 0..rounds {
        let t = Instant::now();
        let mut acc = 0u64;
        for &i in &rank_pos {
            acc = acc.wrapping_add(ring.rank(Col::O, rank_sym, i) as u64);
        }
        black_box(acc);
        h_rk.push(t.elapsed().as_nanos() as u64);

        let t = Instant::now();
        let mut acc = 0u64;
        for &i in &rank_pos {
            acc = acc.wrapping_add(mapped.rank(Col::O, rank_sym, i).unwrap_or(0) as u64);
        }
        black_box(acc);
        m_rk.push(t.elapsed().as_nanos() as u64);
    }
    h_rk.sort_unstable();
    m_rk.sort_unstable();
    let np_ = rank_pos.len().max(1) as f64;
    let hr = h_rk[h_rk.len() / 2] as f64 / np_;
    let mr = m_rk[m_rk.len() / 2] as f64 / np_;
    println!(
        "rank ns/op: heap={hr:.1}  mmap={mr:.1}  ratio={:.3}×",
        ratio(mr, hr)
    );

    let _ = fs::remove_file(&path);

    // ── Summary / gates ───────────────────────────────────────────────────
    println!();
    println!("--- Phase A summary ---");
    println!(
        "RESULT e511_star mmap_heap={star_mh:.4} heap_louds={star_hl:.4} mmap_louds={star_ml:.4}"
    );
    println!(
        "RESULT e511_tri_fair mmap_heap={tri_mh:.4} heap_fair_louds={tri_hl:.4} mmap_fair_louds={tri_ml:.4} semantic_mismatches={tri_semantic_mismatches}"
    );
    // Keep a legacy key only as an explicit non-G3 diagnostic (subject-open probe).
    println!(
        "RESULT e511_tri_legacy_open_probe ring_hits={tri_ring_hits} fair_louds_hits={tri_louds_hits} legacy_open_hits={tri_legacy_open_hits}"
    );

    println!(
        "RESULT e511_rdi  mmap_heap={rdi_ratio:.4} counters={}",
        if counter_match { "MATCH" } else { "MISMATCH" }
    );
    println!(
        "RESULT e511_locality same_block={:.4} same_sb={:.4} avg_len={avg_len:.2}",
        same_block as f64 / n_ranges.max(1) as f64,
        same_sb as f64 / n_ranges.max(1) as f64
    );
    println!(
        "RESULT e511_bytes image={image_bytes} pilot={pilot_bytes} overhead_pct={overhead_pct:.4}"
    );

    println!();
    println!("Goals: G1 star mmap/heap≤1.10  G2 star mmap/LOUDS≤1.50  G3 tri mmap/LOUDS≤1.75");
    println!(
        "  G1: {} ({star_mh:.3}×)",
        if star_mh <= 1.10 { "MET" } else { "OPEN" }
    );
    println!(
        "  G2: {} ({star_ml:.3}×)",
        if star_ml <= 1.50 { "MET" } else { "OPEN" }
    );
    println!(
        "  G3: {} ({tri_ml:.3}×)",
        if tri_ml <= 1.75 { "MET" } else { "OPEN" }
    );

    // Phase B pointers from locality
    println!();
    println!("--- Phase B pointers (from locality) ---");
    let sb_frac = same_block as f64 / n_ranges.max(1) as f64;
    let ssb_frac = same_sb as f64 / n_ranges.max(1) as f64;
    if sb_frac >= 0.40 {
        println!(
            "B2 HIGH VALUE: {:.1}% star ranges same 256-block → range_counts4 same-block path",
            sb_frac * 100.0
        );
    } else if ssb_frac >= 0.50 {
        println!(
            "B2 MEDIUM: {:.1}% same superblock → range_counts4 same-SB path; same-block only {:.1}%",
            ssb_frac * 100.0,
            sb_frac * 100.0
        );
    } else {
        println!(
            "B2: same-block only {:.1}%, same-SB {:.1}% — still implement range_counts4 but expect smaller win",
            sb_frac * 100.0,
            ssb_frac * 100.0
        );
    }

    if !symbol_match {
        println!("STOP: symbol identity mismatch — fix C1/RDI before continuing");
        std::process::exit(1);
    }
    if c1_batch_k() == 0 && !counter_match {
        println!("STOP: full RDI counter mismatch with C1 disabled");
        std::process::exit(1);
    }

    if star_mh > 1.25 {
        println!("B1/B3 priority: mmap tax {star_mh:.2}× > 1.25 → hot views + layout first");
    }
    if tri_mh <= 1.10 && tri_ml > 1.75 {
        println!(
            "D priority: triangle mmap≈heap but {tri_ml:.2}× LOUDS → fused intersection, not layout"
        );
    }

    // ── DROP-branch matrices (B5/C1) — only with --full-campaign + diagnostics ──
    #[cfg(feature = "diagnostics")]
    if full_campaign {
            // ── E5.11 B5: prefetch policy matrix (quick; keep only ≥3–5%) ─────────
            println!();
            println!("--- B5 prefetch matrix (star + RDI full; keep ≥3–5%) ---");
            println!("active_policy_at_entry={}", prefetch_policy().as_str());
            let b5_rounds = 9usize;
            // Warm once under current policy
            for &s in star_seeds.iter().take(8) {
                black_box(star_mmap(&mapped, s));
            }
            let mut b5_rows: Vec<(&str, f64, f64)> = Vec::new();
            for &pol in &PrefetchPolicy::ALL {
                set_prefetch_policy(pol);
                // short warm under new policy
                for &s in star_seeds.iter().take(4) {
                    black_box(star_mmap(&mapped, s));
                }
                let mut star_ms = Vec::with_capacity(b5_rounds);
                let mut rdi_ns = Vec::with_capacity(b5_rounds);
                for _ in 0..b5_rounds {
                    let t = Instant::now();
                    let mut total = 0u32;
                    for &s in &star_seeds {
                        total = total.wrapping_add(star_mmap(&mapped, s));
                    }
                    black_box(total);
                    star_ms.push(t.elapsed().as_secs_f64() * 1e3);

                    let t = Instant::now();
                    let mut acc = 0u64;
                    let mut it = mapped.range_distinct_iter(Col::O, full).unwrap();
                    while let Some((s, c)) = it.next_symbol() {
                        acc = acc.wrapping_add(s as u64).wrapping_add(c as u64);
                    }
                    black_box(acc);
                    rdi_ns.push(t.elapsed().as_nanos() as u64);
                }
                let sm = median_f64(&mut star_ms);
                rdi_ns.sort_unstable();
                let rn = rdi_ns[rdi_ns.len() / 2] as f64 / nsym.max(1) as f64;
                b5_rows.push((pol.as_str(), sm, rn));
                println!(
                    "  policy={:<4}  star_med_ms={sm:.4}  rdi_ns/sym={rn:.2}",
                    pol.as_str()
                );
            }
            // Restore default (None) for product path.
            set_prefetch_policy(PrefetchPolicy::None);
            let base = b5_rows
                .iter()
                .find(|r| r.0 == "none")
                .map(|r| (r.1, r.2))
                .unwrap_or((m_star, m_ns));
            let mut best_gain = 0.0f64;
            let mut best_name = "none";
            for &(name, sm, rn) in &b5_rows {
                // Positive gain = faster than none (lower ms / ns).
                let g_star = (base.0 - sm) / base.0.max(1e-12);
                let g_rdi = (base.1 - rn) / base.1.max(1e-12);
                let g = g_star.max(g_rdi);
                if g > best_gain {
                    best_gain = g;
                    best_name = name;
                }
                println!(
                    "  vs none: {name:<4}  star_gain={:+.2}%  rdi_gain={:+.2}%",
                    g_star * 100.0,
                    g_rdi * 100.0
                );
            }
            let keep = best_gain >= 0.03;
            println!(
                "B5 decision: best={best_name} max_gain={:.1}%  {} (threshold 3%)",
                best_gain * 100.0,
                if keep {
                    "KEEP candidate"
                } else {
                    "DROP — leave None"
                }
            );
            println!(
                "RESULT e511_b5 best={} gain={:.4} keep={}",
                best_name, best_gain, keep
            );

            // ── E5.11 C1: short-range batch K matrix ──────────────────────────────
            println!();
            println!("--- C1 short-range batch matrix (K=0/16/32/64; keep ≥15% star) ---");
            set_prefetch_policy(PrefetchPolicy::Nta); // product default
            let c1_rounds = 11usize;
            let mut c1_rows: Vec<(usize, f64, f64, f64)> = Vec::new();
            for &k in &[0usize, 16, 32, 64] {
                set_c1_batch_k(k);
                for &s in star_seeds.iter().take(4) {
                    black_box(star_mmap(&mapped, s));
                }
                let mut star_ms = Vec::with_capacity(c1_rounds);
                let mut louds_ms = Vec::with_capacity(c1_rounds);
                for _ in 0..c1_rounds {
                    let t = Instant::now();
                    let mut total = 0u32;
                    for &s in &star_seeds {
                        total = total.wrapping_add(star_mmap(&mapped, s));
                    }
                    black_box(total);
                    star_ms.push(t.elapsed().as_secs_f64() * 1e3);

                    let t = Instant::now();
                    let mut total = 0u32;
                    for &s in &star_seeds {
                        total = total.wrapping_add(louds_star(&louds, s));
                    }
                    black_box(total);
                    louds_ms.push(t.elapsed().as_secs_f64() * 1e3);
                }
                let sm = median_f64(&mut star_ms);
                let lm = median_f64(&mut louds_ms);
                let ml = ratio(sm, lm);
                c1_rows.push((k, sm, lm, ml));
                println!("  K={k:<2}  star_med_ms={sm:.4}  louds_med_ms={lm:.4}  mmap/LOUDS={ml:.3}×");
            }
            set_c1_batch_k(C1_K_DEFAULT);
            let base_k0 = c1_rows
                .iter()
                .find(|r| r.0 == 0)
                .map(|r| r.1)
                .unwrap_or(m_star);
            let mut best_k = 0usize;
            let mut best_star_gain = 0.0f64;
            // Default best_ml to K=0 ratio so DROP reports a real number when no K improves.
            let mut best_ml = c1_rows
                .iter()
                .find(|r| r.0 == 0)
                .map(|r| r.3)
                .unwrap_or(f64::NAN);
            for &(k, sm, _lm, ml) in &c1_rows {
                let g = (base_k0 - sm) / base_k0.max(1e-12);
                println!(
                    "  vs K=0: K={k:<2}  star_gain={:+.2}%  mmap/LOUDS={ml:.3}×",
                    g * 100.0
                );
                if g > best_star_gain {
                    best_star_gain = g;
                    best_k = k;
                    best_ml = ml;
                }
            }

            let c1_keep = best_star_gain >= 0.15;
            let c1_weak = best_star_gain >= 0.05 && best_star_gain < 0.15;
            println!(
                "C1 decision: best_K={best_k} star_gain={:.1}% mmap/LOUDS={best_ml:.3}×  {}",
                best_star_gain * 100.0,
                if c1_keep {
                    "STRONG KEEP (≥15%)"
                } else if c1_weak {
                    "WEAK KEEP (5–15%; keep if simple)"
                } else {
                    "DROP (<5%)"
                }
            );
            println!(
                "RESULT e511_c1 best_k={} star_gain={:.4} mmap_louds={best_ml:.4} keep={}",
                best_k,
                best_star_gain,
                c1_keep || c1_weak
            );

    }
    #[cfg(not(feature = "diagnostics"))]
    if full_campaign {
        println!("(skipping B5/C1 matrices: rebuild with --features diagnostics)");
    }

    // ── E5.11 D1: two-range braided intersection ──────────────────────────
    // Pair subject ranges (range_s) and seek common objects on C_o.
    // Compare braided intersection_next_value2 vs dual-RNV leapfrog.
    println!();
    println!("--- D1 two-range braided intersection (C_o under paired range_s) ---");
    // Product defaults: C1 K=0, prefetch NTA (no mutation knobs on product path).

    // Build non-empty subject range pairs from star seeds.
    let mut d1_pairs: Vec<(RowRange, RowRange)> = Vec::new();
    for i in 0..star_seeds.len() {
        for j in (i + 1)..star_seeds.len().min(i + 8) {
            let ra = mapped.range_s(star_seeds[i]).unwrap_or(RowRange::empty());
            let rb = mapped.range_s(star_seeds[j]).unwrap_or(RowRange::empty());
            if !ra.is_empty() && !rb.is_empty() {
                d1_pairs.push((ra, rb));
            }
        }
    }
    // Cap for stable median
    if d1_pairs.len() > 256 {
        d1_pairs.truncate(256);
    }
    println!("D1 pairs: {}", d1_pairs.len());

    // Correctness: braided == dual RNV on a sample of pairs × targets.
    let mut d1_mismatches = 0u64;
    let mut d1_common_found = 0u64;
    let mut d1_none = 0u64;
    for (ra, rb) in d1_pairs.iter().take(64) {
        for &t in &[0u32, 1, 50, 100, 500, 1000, 5000] {
            let braid = mapped.intersection_next_value2(Col::O, *ra, *rb, t);
            let dual = mapped.intersection_next_value2_dual_rnv(Col::O, *ra, *rb, t);
            if braid != dual {
                d1_mismatches += 1;
            }
            match braid {
                Some(_) => d1_common_found += 1,
                None => d1_none += 1,
            }
        }
    }
    println!(
        "D1 correctness sample: mismatches={d1_mismatches} common_hits={d1_common_found} none={d1_none}"
    );
    if d1_mismatches > 0 {
        println!("STOP: D1 braided ≠ dual-RNV");
        std::process::exit(1);
    }

    // Aggregate software counters (diagnostics only; DROP-branch harness detail).
    #[cfg(feature = "diagnostics")]
    if full_campaign {
        let mut st_levels = 0u64;
        let mut st_expands = 0u64;
        let mut st_one_side = 0u64;
        let mut st_both_empty = 0u64;
        let mut st_children = 0u64;
        let mut st_hits = 0u64;
        let mut st_calls = 0u64;
        for &(ra, rb) in &d1_pairs {
            let (v, st) = mapped.intersection_next_value2_counted(Col::O, ra, rb, 0);
            st_calls += 1;
            st_levels += st.levels_visited as u64;
            st_expands += st.expands as u64;
            st_one_side += st.one_side_empty as u64;
            st_both_empty += st.both_empty as u64;
            st_children += st.children_pushed as u64;
            if v.is_some() {
                st_hits += 1;
            }
        }
        if st_calls > 0 {
            println!(
                "D1 counters (target=0, n={st_calls}): hit_rate={:.1}%  levels/call={:.2} expands/call={:.2} one_side_empty/call={:.2} both_empty/call={:.2} children/call={:.2}",
                100.0 * st_hits as f64 / st_calls as f64,
                st_levels as f64 / st_calls as f64,
                st_expands as f64 / st_calls as f64,
                st_one_side as f64 / st_calls as f64,
                st_both_empty as f64 / st_calls as f64,
                st_children as f64 / st_calls as f64
            );
        }
    }

    // Latency: braided vs dual-RNV (median of rounds; first common only).
    let d1_rounds = 11usize;
    for (ra, rb) in d1_pairs.iter().take(8) {
        black_box(mapped.intersection_next_value2(Col::O, *ra, *rb, 0));
        black_box(mapped.intersection_next_value2_dual_rnv(Col::O, *ra, *rb, 0));
    }
    let mut braid_ns = Vec::with_capacity(d1_rounds);
    let mut dual_ns = Vec::with_capacity(d1_rounds);
    for _ in 0..d1_rounds {
        let t = Instant::now();
        let mut acc = 0u64;
        for &(ra, rb) in &d1_pairs {
            if let Some(v) = mapped.intersection_next_value2(Col::O, ra, rb, 0) {
                acc = acc.wrapping_add(v as u64);
            }
        }
        black_box(acc);
        braid_ns.push(t.elapsed().as_nanos() as u64);

        let t = Instant::now();
        let mut acc = 0u64;
        for &(ra, rb) in &d1_pairs {
            if let Some(v) = mapped.intersection_next_value2_dual_rnv(Col::O, ra, rb, 0) {
                acc = acc.wrapping_add(v as u64);
            }
        }
        black_box(acc);
        dual_ns.push(t.elapsed().as_nanos() as u64);
    }
    braid_ns.sort_unstable();
    dual_ns.sort_unstable();
    let b_med = braid_ns[braid_ns.len() / 2] as f64;
    let d_med = dual_ns[dual_ns.len() / 2] as f64;
    let n_pairs = d1_pairs.len().max(1) as f64;
    let b_ns_pair = b_med / n_pairs;
    let d_ns_pair = d_med / n_pairs;
    let d1_speedup = if b_ns_pair > 0.0 {
        d_ns_pair / b_ns_pair
    } else {
        f64::NAN
    };
    let d1_gain = (d_ns_pair - b_ns_pair) / d_ns_pair.max(1e-12);
    println!(
        "D1 latency ns/pair (first common ≥0): braid={b_ns_pair:.1}  dual_rnv={d_ns_pair:.1}  dual/braid={d1_speedup:.3}×  braid_gain={:+.1}%",
        d1_gain * 100.0
    );

    // Streaming: enumerate all common values (successive target = prev+1).
    let mut stream_braid_ns = Vec::with_capacity(d1_rounds);
    let mut stream_dual_ns = Vec::with_capacity(d1_rounds);
    let mut stream_commons = 0u64;
    for _ in 0..d1_rounds {
        let t = Instant::now();
        let mut acc = 0u64;
        let mut ncom = 0u64;
        for &(ra, rb) in d1_pairs.iter().take(32) {
            let mut tgt = 0u32;
            while let Some(v) = mapped.intersection_next_value2(Col::O, ra, rb, tgt) {
                acc = acc.wrapping_add(v as u64);
                ncom += 1;
                tgt = v.saturating_add(1);
                if tgt == 0 {
                    break;
                }
            }
        }
        black_box(acc);
        stream_braid_ns.push(t.elapsed().as_nanos() as u64);
        stream_commons = ncom;

        let t = Instant::now();
        let mut acc = 0u64;
        for &(ra, rb) in d1_pairs.iter().take(32) {
            let mut tgt = 0u32;
            while let Some(v) = mapped.intersection_next_value2_dual_rnv(Col::O, ra, rb, tgt) {
                acc = acc.wrapping_add(v as u64);
                tgt = v.saturating_add(1);
                if tgt == 0 {
                    break;
                }
            }
        }
        black_box(acc);
        stream_dual_ns.push(t.elapsed().as_nanos() as u64);
    }
    stream_braid_ns.sort_unstable();
    stream_dual_ns.sort_unstable();
    let sb_med = stream_braid_ns[stream_braid_ns.len() / 2] as f64;
    let sd_med = stream_dual_ns[stream_dual_ns.len() / 2] as f64;
    let stream_gain = (sd_med - sb_med) / sd_med.max(1e-12);
    println!(
        "D1 stream (32 pairs, all commons): braid_ns={sb_med:.0} dual_ns={sd_med:.0} gain={:+.1}% commons/round={stream_commons}",
        stream_gain * 100.0
    );

    let d1_keep = d1_gain >= 0.15 || stream_gain >= 0.15;
    let d1_weak = !d1_keep && (d1_gain >= 0.05 || stream_gain >= 0.05);
    println!(
        "D1 decision: first_gain={:+.1}% stream_gain={:+.1}%  {}",
        d1_gain * 100.0,
        stream_gain * 100.0,
        if d1_keep {
            "STRONG KEEP (≥15%)"
        } else if d1_weak {
            "WEAK KEEP (5–15%)"
        } else {
            "DROP (<5%)"
        }
    );
    println!(
        "RESULT e511_d1 first_gain={:.4} stream_gain={:.4} dual_over_braid={d1_speedup:.4} mismatches={d1_mismatches} keep={}",
        d1_gain,
        stream_gain,
        d1_keep || d1_weak
    );

    // ── E5.11 D2: three-range braided intersection / product triangle ─────
    // The product triangle uses three subject ranges and a common object
    // successor. Compare the synchronized descent with three independent RNV
    // seeks, then repeat the comparison while streaming all common objects.
    println!();
    println!("--- D2 three-range braided intersection (product triangle) ---");
    let mut d2_triples: Vec<(RowRange, RowRange, RowRange)> = Vec::new();
    for &a in tri_seeds.iter() {
        for d in 1..=3u32 {
            let b = (a + d) % ns.max(1);
            let c = (b + 1) % ns.max(1);
            let ra = mapped.range_s(a).unwrap_or(RowRange::empty());
            let rb = mapped.range_s(b).unwrap_or(RowRange::empty());
            let rc = mapped.range_s(c).unwrap_or(RowRange::empty());
            if !ra.is_empty() && !rb.is_empty() && !rc.is_empty() {
                d2_triples.push((ra, rb, rc));
            }
        }
    }
    if d2_triples.len() > 192 {
        d2_triples.truncate(192);
    }
    println!("D2 triples: {}", d2_triples.len());

    let mut d2_mismatches = 0u64;
    let mut d2_hits = 0u64;
    for &(ra, rb, rc) in d2_triples.iter().take(96) {
        for &target in &[0u32, 1, 50, 100, 500, 1000, 5000] {
            let braid = mapped.intersection_next_value3(Col::O, ra, rb, rc, target);
            let dual = mapped.intersection_next_value3_dual_rnv(Col::O, ra, rb, rc, target);
            if braid != dual {
                d2_mismatches += 1;
            }
            if braid.is_some() {
                d2_hits += 1;
            }
        }
    }
    println!("D2 correctness sample: mismatches={d2_mismatches} common_hits={d2_hits}");
    if d2_mismatches > 0 {
        println!("STOP: D2 braided ≠ three-RNV");
        std::process::exit(1);
    }

    let d2_rounds = 11usize;
    for &(ra, rb, rc) in d2_triples.iter().take(8) {
        black_box(mapped.intersection_next_value3(Col::O, ra, rb, rc, 0));
        black_box(mapped.intersection_next_value3_dual_rnv(Col::O, ra, rb, rc, 0));
    }
    let mut d2_braid_ns = Vec::with_capacity(d2_rounds);
    let mut d2_dual_ns = Vec::with_capacity(d2_rounds);
    for _ in 0..d2_rounds {
        let t = Instant::now();
        let mut acc = 0u64;
        for &(ra, rb, rc) in &d2_triples {
            if let Some(v) = mapped.intersection_next_value3(Col::O, ra, rb, rc, 0) {
                acc = acc.wrapping_add(v as u64);
            }
        }
        black_box(acc);
        d2_braid_ns.push(t.elapsed().as_nanos() as u64);

        let t = Instant::now();
        let mut acc = 0u64;
        for &(ra, rb, rc) in &d2_triples {
            if let Some(v) = mapped.intersection_next_value3_dual_rnv(Col::O, ra, rb, rc, 0) {
                acc = acc.wrapping_add(v as u64);
            }
        }
        black_box(acc);
        d2_dual_ns.push(t.elapsed().as_nanos() as u64);
    }
    d2_braid_ns.sort_unstable();
    d2_dual_ns.sort_unstable();
    let d2_b_med = d2_braid_ns[d2_braid_ns.len() / 2] as f64;
    let d2_d_med = d2_dual_ns[d2_dual_ns.len() / 2] as f64;
    let n_d2 = d2_triples.len().max(1) as f64;
    let d2_b_ns = d2_b_med / n_d2;
    let d2_d_ns = d2_d_med / n_d2;
    let d2_first_gain = (d2_d_ns - d2_b_ns) / d2_d_ns.max(1e-12);
    let d2_dual_over_braid = d2_d_ns / d2_b_ns.max(1e-12);
    println!(
        "D2 latency ns/triple (first common ≥0): braid={d2_b_ns:.1} dual_rnv={d2_d_ns:.1} dual/braid={d2_dual_over_braid:.3}× gain={:+.1}%",
        d2_first_gain * 100.0
    );

    let mut d2_stream_braid_ns = Vec::with_capacity(d2_rounds);
    let mut d2_stream_dual_ns = Vec::with_capacity(d2_rounds);
    let mut d2_stream_commons = 0u64;
    for _ in 0..d2_rounds {
        let t = Instant::now();
        let mut acc = 0u64;
        let mut ncom = 0u64;
        for &(ra, rb, rc) in d2_triples.iter().take(32) {
            let mut target = 0u32;
            while let Some(v) = mapped.intersection_next_value3(Col::O, ra, rb, rc, target) {
                acc = acc.wrapping_add(v as u64);
                ncom += 1;
                target = v.saturating_add(1);
                if target == 0 {
                    break;
                }
            }
        }
        black_box(acc);
        d2_stream_braid_ns.push(t.elapsed().as_nanos() as u64);
        d2_stream_commons = ncom;

        let t = Instant::now();
        let mut acc = 0u64;
        for &(ra, rb, rc) in d2_triples.iter().take(32) {
            let mut target = 0u32;
            while let Some(v) = mapped.intersection_next_value3_dual_rnv(Col::O, ra, rb, rc, target)
            {
                acc = acc.wrapping_add(v as u64);
                target = v.saturating_add(1);
                if target == 0 {
                    break;
                }
            }
        }
        black_box(acc);
        d2_stream_dual_ns.push(t.elapsed().as_nanos() as u64);
    }
    d2_stream_braid_ns.sort_unstable();
    d2_stream_dual_ns.sort_unstable();
    let d2_sb_med = d2_stream_braid_ns[d2_stream_braid_ns.len() / 2] as f64;
    let d2_sd_med = d2_stream_dual_ns[d2_stream_dual_ns.len() / 2] as f64;
    let d2_stream_gain = (d2_sd_med - d2_sb_med) / d2_sd_med.max(1e-12);
    println!(
        "D2 stream (32 triples, all commons): braid_ns={d2_sb_med:.0} dual_ns={d2_sd_med:.0} gain={:+.1}% commons/round={d2_stream_commons}",
        d2_stream_gain * 100.0
    );

    let d2_keep = d2_first_gain >= 0.15 || d2_stream_gain >= 0.15;
    let d2_weak = !d2_keep && (d2_first_gain >= 0.05 || d2_stream_gain >= 0.05);
    println!(
        "D2 decision: first_gain={:+.1}% stream_gain={:+.1}%  {}",
        d2_first_gain * 100.0,
        d2_stream_gain * 100.0,
        if d2_keep {
            "STRONG KEEP (≥15%)"
        } else if d2_weak {
            "WEAK KEEP (5–15%)"
        } else {
            "DROP (<5%)"
        }
    );
    println!(
        "RESULT e511_d2 first_gain={d2_first_gain:.4} stream_gain={d2_stream_gain:.4} dual_over_braid={d2_dual_over_braid:.4} mismatches={d2_mismatches} triples={} keep={}",
        d2_triples.len(),
        d2_keep || d2_weak
    );

    // ── DROP-branch matrices (D3/D4/E1) — only with --full-campaign + diagnostics ──
    #[cfg(feature = "diagnostics")]
    if full_campaign {
            // ── E5.11 D3-A: zero-byte product-triangle decomposition ─────────────
            // Profile the already-wired D2 product path before making D3-B the
            // default.  The profile intentionally uses the counted diagnostic API;
            // the normal intersection successor remains allocation-free and
            // counter-free.  The current D2 product shape is range based, so F/LF
            // projection and result translation are explicitly reported as zero
            // rather than being approximated with unrelated work.
            println!();
            println!("--- D3-A measurement-first product triangle decomposition ---");
            let d3 = profile_d3(&mapped, &louds, &tri_seeds, ns);
            println!(
                "D3 validation: product_mismatches={} stream_mismatches={} baseline_mismatches={} zero_byte_bytes={}",
                d3.product_mismatches, d3.stream_mismatches, d3.baseline_mismatches, d3.persistent_bytes
            );
            println!(
                "D3 counters: triangle_seeds={} braid_calls={} successful_calls={} empty_calls={} root_restarts={} common_emitted={} levels_visited={} range_expands={}",
                d3.triangle_seeds,
                d3.braid_calls,
                d3.successful_calls,
                d3.empty_calls,
                d3.root_restarts,
                d3.common_emitted,
                d3.levels_visited,
                d3.range_expands
            );
            println!(
                "D3 rates: levels/common={:.2} expands/common={:.2} empty-call-rate={:.2}% singleton-rate={:.2}%",
                d3.levels_visited as f64 / d3.common_emitted.max(1) as f64,
                d3.range_expands as f64 / d3.common_emitted.max(1) as f64,
                100.0 * d3.empty_calls as f64 / d3.braid_calls.max(1) as f64,
                100.0 * d3.singleton_ranges as f64 / d3.range_count.max(1) as f64
            );
            println!("D3 range histogram (all projected ranges): {}", d3.hist);
            println!(
                "D3 timing totals (ms): range_construct={:.3} A_lookup={:.3} F/LF_projection={:.3} braid_init_traversal={:.3} successor_restarts={:.3} result_translation={:.3} loop_control={:.3} LOUDS_operations={:.3}",
                nanos_ms(d3.timing.range_construction),
                nanos_ms(d3.timing.a_lookup),
                nanos_ms(d3.timing.f_lf_projection),
                nanos_ms(d3.timing.braid_init_traversal),
                nanos_ms(d3.timing.successor_restarts),
                nanos_ms(d3.timing.result_translation),
                nanos_ms(d3.timing.loop_control),
                nanos_ms(d3.timing.louds_operations)
            );
            println!(
                "D3 per-seed: seed product_calls product_commons product_levels product_expands product_ms louds_ms"
            );
            for row in &d3.per_seed {
                println!(
                    "D3 seed: {} {} {} {} {} {:.3} {:.3}",
                    row.seed,
                    row.product_calls,
                    row.product_commons,
                    row.product_levels,
                    row.product_expands,
                    nanos_ms(row.product_ns),
                    nanos_ms(row.louds_ns)
                );
            }
            if d3.product_mismatches != 0 || d3.stream_mismatches != 0 || d3.baseline_mismatches != 0 {
                println!("STOP: D3 result validation mismatch");
                std::process::exit(1);
            }
            println!(
                "D3-A decision: measurement complete; zero_byte_bytes={} (image unchanged)",
                d3.persistent_bytes
            );

            // ── E5.11 D3-B: independent RNV vs repeated D2 vs resumable iterator ──
            // Same triple set as D2. Stream all commons three ways:
            //   1) three-RNV leapfrog (oracle baseline)
            //   2) repeated D2 successor (root restart each common)
            //   3) IntersectionIter3 (persistent stack; no root restart)
            // KEEP if iterator ≥10% faster than repeated D2 on the stream path.
            println!();
            println!("--- D3-B RNV vs repeated D2 vs resumable iterator (stream all commons) ---");
            let d3_stream_triples: &[(RowRange, RowRange, RowRange)] =
                &d2_triples[..d2_triples.len().min(32)];
            let d3b_rounds = 11usize;

            // Warm
            for &(ra, rb, rc) in d3_stream_triples.iter().take(4) {
                black_box(mapped.intersection_next_value3_dual_rnv(Col::O, ra, rb, rc, 0));
                black_box(mapped.intersection_next_value3(Col::O, ra, rb, rc, 0));
                black_box(mapped.intersection_iter3(Col::O, ra, rb, rc, 0).count());
            }

            let mut d3_rnv_ns = Vec::with_capacity(d3b_rounds);
            let mut d3_d2_ns = Vec::with_capacity(d3b_rounds);
            let mut d3_iter_ns = Vec::with_capacity(d3b_rounds);
            let mut d3_stream_commons = 0u64;
            let mut d3_iter_mismatches = 0u64;

            for _ in 0..d3b_rounds {
                // Independent three-RNV stream
                let t = Instant::now();
                let mut acc = 0u64;
                for &(ra, rb, rc) in d3_stream_triples {
                    let mut target = 0u32;
                    while let Some(v) = mapped.intersection_next_value3_dual_rnv(Col::O, ra, rb, rc, target)
                    {
                        acc = acc.wrapping_add(v as u64);
                        target = v.saturating_add(1);
                        if target == 0 {
                            break;
                        }
                    }
                }
                black_box(acc);
                d3_rnv_ns.push(t.elapsed().as_nanos() as u64);

                // Repeated D2 successor stream
                let t = Instant::now();
                let mut acc = 0u64;
                let mut ncom = 0u64;
                for &(ra, rb, rc) in d3_stream_triples {
                    let mut target = 0u32;
                    while let Some(v) = mapped.intersection_next_value3(Col::O, ra, rb, rc, target) {
                        acc = acc.wrapping_add(v as u64);
                        ncom += 1;
                        target = v.saturating_add(1);
                        if target == 0 {
                            break;
                        }
                    }
                }
                black_box(acc);
                d3_d2_ns.push(t.elapsed().as_nanos() as u64);
                d3_stream_commons = ncom;

                // Persistent iterator stream
                let t = Instant::now();
                let mut acc = 0u64;
                for &(ra, rb, rc) in d3_stream_triples {
                    for v in mapped.intersection_iter3(Col::O, ra, rb, rc, 0) {
                        acc = acc.wrapping_add(v as u64);
                    }
                }
                black_box(acc);
                d3_iter_ns.push(t.elapsed().as_nanos() as u64);
            }

            // Correctness: one-shot collect vs D2 stream on every triple.
            for &(ra, rb, rc) in d3_stream_triples {
                let mut d2_vals = Vec::new();
                let mut target = 0u32;
                while let Some(v) = mapped.intersection_next_value3(Col::O, ra, rb, rc, target) {
                    d2_vals.push(v);
                    target = v.saturating_add(1);
                    if target == 0 {
                        break;
                    }
                }
                let iter_vals: Vec<u32> = mapped.intersection_iter3(Col::O, ra, rb, rc, 0).collect();
                if iter_vals != d2_vals {
                    d3_iter_mismatches += 1;
                }
            }

            d3_rnv_ns.sort_unstable();
            d3_d2_ns.sort_unstable();
            d3_iter_ns.sort_unstable();
            let d3_rnv_med = d3_rnv_ns[d3_rnv_ns.len() / 2] as f64;
            let d3_d2_med = d3_d2_ns[d3_d2_ns.len() / 2] as f64;
            let d3_iter_med = d3_iter_ns[d3_iter_ns.len() / 2] as f64;
            // Gain of iterator vs repeated D2: positive ⇒ iterator faster.
            let d3_iter_vs_d2 = (d3_d2_med - d3_iter_med) / d3_d2_med.max(1e-12);
            // Gain of iterator vs three-RNV.
            let d3_iter_vs_rnv = (d3_rnv_med - d3_iter_med) / d3_rnv_med.max(1e-12);
            // Gain of repeated D2 vs three-RNV (should match D2 stream gain).
            let d3_d2_vs_rnv = (d3_rnv_med - d3_d2_med) / d3_rnv_med.max(1e-12);

            println!(
                "D3-B stream ({} triples, all commons): rnv_ns={d3_rnv_med:.0} d2_ns={d3_d2_med:.0} iter_ns={d3_iter_med:.0} commons/round={d3_stream_commons} mismatches={d3_iter_mismatches}",
                d3_stream_triples.len()
            );
            println!(
                "D3-B gains: iter_vs_d2={:+.1}%  iter_vs_rnv={:+.1}%  d2_vs_rnv={:+.1}%  zero_byte_bytes=0",
                d3_iter_vs_d2 * 100.0,
                d3_iter_vs_rnv * 100.0,
                d3_d2_vs_rnv * 100.0
            );

            if d3_iter_mismatches > 0 {
                println!("STOP: D3-B iterator ≠ repeated D2 stream");
                std::process::exit(1);
            }

            // Target: ≥10% triangle stream gain of iterator over repeated D2.
            let d3_keep = d3_iter_vs_d2 >= 0.10;
            let d3_weak = !d3_keep && d3_iter_vs_d2 >= 0.05;
            let d3_decision = if d3_keep {
                "STRONG KEEP (≥10% iter vs D2)"
            } else if d3_weak {
                "WEAK KEEP (5–10% iter vs D2)"
            } else if d3_iter_vs_d2 > 0.0 {
                "DROP (<5% iter vs D2; keep D2 successor)"
            } else {
                "DROP (iterator not faster than D2; keep D2 successor)"
            };
            println!(
                "D3 decision: iter_vs_d2={:+.1}%  {}",
                d3_iter_vs_d2 * 100.0,
                d3_decision
            );
            println!(
                "RESULT e511_d3 iter_vs_d2={d3_iter_vs_d2:.4} iter_vs_rnv={d3_iter_vs_rnv:.4} d2_vs_rnv={d3_d2_vs_rnv:.4} mismatches={d3_iter_mismatches} zero_bytes=0 keep={}",
                d3_keep || d3_weak
            );

            // ── E5.11 D4: expand locality (A) + mask-first fused expand3 prototype (B) ──
            // D4-A measures unique DataLine/Superblock overlap across the three independent
            // B2 expands at each D2 frame. D4-B is an algebraically identical successor that
            // forms occupancy masks before materializing children (still three expands/frame;
            // fusion opportunity is the unique-line bound from D4-A).
            // KEEP D4-B only if stream latency ≥10% faster than repeated D2.
            println!();
            println!("--- D4-A unique DataLine/Superblock overlap (product triangle expands) ---");
            let d4_triples: &[(RowRange, RowRange, RowRange)] = &d2_triples[..d2_triples.len().min(64)];
            let mut d4_frames = 0u64;
            let mut d4_line_indep = 0u64;
            let mut d4_sb_indep = 0u64;
            let mut d4_unique_lines = 0u64;
            let mut d4_unique_sbs = 0u64;
            let mut d4_all_same_line_shared = 0u64;
            let mut d4_all_same_line_distinct = 0u64;
            let mut d4_all_same_sb = 0u64;
            let mut d4_pair_line = 0u64;
            let mut d4_no_line = 0u64;
            let mut d4_mismatches = 0u64;
            for &(ra, rb, rc) in d4_triples {
                let mut target = 0u32;
                loop {
                    let expected = mapped.intersection_next_value3(Col::O, ra, rb, rc, target);
                    let (got, st) = mapped.intersection_next_value3_overlap(Col::O, ra, rb, rc, target);
                    if got != expected {
                        d4_mismatches += 1;
                    }
                    d4_frames += st.frames as u64;
                    d4_line_indep += st.line_loads_indep;
                    d4_sb_indep += st.sb_loads_indep;
                    d4_unique_lines += st.unique_lines;
                    d4_unique_sbs += st.unique_sbs;
                    d4_all_same_line_shared += st.all_same_line_shared as u64;
                    d4_all_same_line_distinct += st.all_same_line_distinct as u64;
                    d4_all_same_sb += st.all_same_sb as u64;
                    d4_pair_line += st.pair_line_overlap as u64;
                    d4_no_line += st.no_line_overlap as u64;
                    match expected {
                        None => break,
                        Some(v) => {
                            target = v.saturating_add(1);
                            if target == 0 {
                                break;
                            }
                        }
                    }
                }
            }
            let line_save = 1.0 - (d4_unique_lines as f64 / d4_line_indep.max(1) as f64);
            let sb_save = 1.0 - (d4_unique_sbs as f64 / d4_sb_indep.max(1) as f64);
            let shared_line_rate = d4_all_same_line_shared as f64 / d4_frames.max(1) as f64;
            let pair_line_rate = d4_pair_line as f64 / d4_frames.max(1) as f64;
            println!(
                "D4-A frames={d4_frames} line_loads_indep={d4_line_indep} unique_lines={d4_unique_lines} line_save={:+.1}%",
                line_save * 100.0
            );
            println!(
                "D4-A sb_loads_indep={d4_sb_indep} unique_sbs={d4_unique_sbs} sb_save={:+.1}%",
                sb_save * 100.0
            );
            println!(
                "D4-A rates: all_same_line_shared={:.1}% all_same_line_distinct={:.1}% all_same_sb={:.1}% pair_line={:.1}% no_line={:.1}% mismatches={d4_mismatches}",
                shared_line_rate * 100.0,
                d4_all_same_line_distinct as f64 / d4_frames.max(1) as f64 * 100.0,
                d4_all_same_sb as f64 / d4_frames.max(1) as f64 * 100.0,
                pair_line_rate * 100.0,
                d4_no_line as f64 / d4_frames.max(1) as f64 * 100.0,
            );
            if d4_mismatches > 0 {
                println!("STOP: D4-A overlap walker ≠ D2");
                std::process::exit(1);
            }
            println!(
                "RESULT e511_d4a frames={d4_frames} line_save={line_save:.4} sb_save={sb_save:.4} shared_line_rate={shared_line_rate:.4} pair_line_rate={pair_line_rate:.4} mismatches={d4_mismatches}"
            );

            println!();
            println!("--- D4-B mask-first fused expand3 prototype vs repeated D2 ---");
            let d4b_stream: &[(RowRange, RowRange, RowRange)] = &d2_triples[..d2_triples.len().min(32)];
            let d4b_rounds = 11usize;
            for &(ra, rb, rc) in d4b_stream.iter().take(4) {
                black_box(mapped.intersection_next_value3(Col::O, ra, rb, rc, 0));
                black_box(mapped.intersection_next_value3_fused(Col::O, ra, rb, rc, 0));
            }
            let mut d4_d2_ns = Vec::with_capacity(d4b_rounds);
            let mut d4_fu_ns = Vec::with_capacity(d4b_rounds);
            let mut d4_stream_mismatches = 0u64;
            let mut d4_mask_pruned = 0u64;
            let mut d4_expands_full = 0u64;
            let mut d4_levels = 0u64;
            for _ in 0..d4b_rounds {
                let t = Instant::now();
                let mut acc = 0u64;
                for &(ra, rb, rc) in d4b_stream {
                    let mut target = 0u32;
                    while let Some(v) = mapped.intersection_next_value3(Col::O, ra, rb, rc, target) {
                        acc = acc.wrapping_add(v as u64);
                        target = v.saturating_add(1);
                        if target == 0 {
                            break;
                        }
                    }
                }
                black_box(acc);
                d4_d2_ns.push(t.elapsed().as_nanos() as u64);

                let t = Instant::now();
                let mut acc = 0u64;
                for &(ra, rb, rc) in d4b_stream {
                    let mut target = 0u32;
                    loop {
                        let (v, st) = mapped.intersection_next_value3_fused(Col::O, ra, rb, rc, target);
                        d4_mask_pruned += st.mask_pruned as u64;
                        d4_expands_full += st.expands_full as u64;
                        d4_levels += st.levels_visited as u64;
                        match v {
                            None => break,
                            Some(x) => {
                                acc = acc.wrapping_add(x as u64);
                                target = x.saturating_add(1);
                                if target == 0 {
                                    break;
                                }
                            }
                        }
                    }
                }
                black_box(acc);
                d4_fu_ns.push(t.elapsed().as_nanos() as u64);
            }
            for &(ra, rb, rc) in d4b_stream {
                let mut d2_vals = Vec::new();
                let mut target = 0u32;
                while let Some(v) = mapped.intersection_next_value3(Col::O, ra, rb, rc, target) {
                    d2_vals.push(v);
                    target = v.saturating_add(1);
                    if target == 0 {
                        break;
                    }
                }
                let mut fu_vals = Vec::new();
                let mut target = 0u32;
                loop {
                    let (v, _) = mapped.intersection_next_value3_fused(Col::O, ra, rb, rc, target);
                    match v {
                        None => break,
                        Some(x) => {
                            fu_vals.push(x);
                            target = x.saturating_add(1);
                            if target == 0 {
                                break;
                            }
                        }
                    }
                }
                if fu_vals != d2_vals {
                    d4_stream_mismatches += 1;
                }
            }
            d4_d2_ns.sort_unstable();
            d4_fu_ns.sort_unstable();
            let d4_d2_med = d4_d2_ns[d4_d2_ns.len() / 2] as f64;
            let d4_fu_med = d4_fu_ns[d4_fu_ns.len() / 2] as f64;
            let d4_fu_vs_d2 = (d4_d2_med - d4_fu_med) / d4_d2_med.max(1e-12);
            println!(
                "D4-B stream ({} triples): d2_ns={d4_d2_med:.0} fused_ns={d4_fu_med:.0} fused_vs_d2={:+.1}% mismatches={d4_stream_mismatches}",
                d4b_stream.len(),
                d4_fu_vs_d2 * 100.0
            );
            println!(
                "D4-B counters: levels={d4_levels} expands_full={d4_expands_full} mask_pruned={d4_mask_pruned} unique_line_bound_from_A={d4_unique_lines}"
            );
            if d4_stream_mismatches > 0 {
                println!("STOP: D4-B fused ≠ D2 stream");
                std::process::exit(1);
            }
            let d4_keep = d4_fu_vs_d2 >= 0.10;
            let d4_weak = !d4_keep && d4_fu_vs_d2 >= 0.05;
            let d4_decision = if d4_keep {
                "STRONG KEEP (≥10% fused vs D2)"
            } else if d4_weak {
                "WEAK KEEP (5–10% fused vs D2)"
            } else if d4_fu_vs_d2 > 0.0 {
                "DROP (<5% fused vs D2; keep D2 successor)"
            } else {
                "DROP (fused not faster than D2; keep D2 successor)"
            };
            println!(
                "D4 decision: fused_vs_d2={:+.1}% line_save={:+.1}%  {}",
                d4_fu_vs_d2 * 100.0,
                line_save * 100.0,
                d4_decision
            );
            println!(
                "RESULT e511_d4 fused_vs_d2={d4_fu_vs_d2:.4} line_save={line_save:.4} sb_save={sb_save:.4} mismatches={d4_stream_mismatches} zero_bytes=0 keep={}",
                d4_keep || d4_weak
            );

            // ── E5.11 D4-C: true unique-load expand3 ────────────────────────────────
            // Physical load fusion: classify six endpoints, load each unique DataLine /
            // Superblock once, then derive three RangeExpand + common_mask. KEEP if
            // stream ≥10% faster than D2 (or ≥5% weak) with realized line savings near
            // D4-A unique-line bound.
            println!();
            println!("--- D4-C true unique-load expand3 vs repeated D2 ---");
            let d4c_stream: &[(RowRange, RowRange, RowRange)] = &d2_triples[..d2_triples.len().min(32)];
            let d4c_rounds = 11usize;
            for &(ra, rb, rc) in d4c_stream.iter().take(4) {
                black_box(mapped.intersection_next_value3(Col::O, ra, rb, rc, 0));
                black_box(mapped.intersection_next_value3_shared(Col::O, ra, rb, rc, 0));
            }
            let mut d4c_d2_ns = Vec::with_capacity(d4c_rounds);
            let mut d4c_sh_ns = Vec::with_capacity(d4c_rounds);
            let mut d4c_stream_mismatches = 0u64;
            let mut d4c_frames = 0u64;
            let mut d4c_logical_lines = 0u64;
            let mut d4c_unique_lines = 0u64;
            let mut d4c_logical_sbs = 0u64;
            let mut d4c_unique_sbs = 0u64;
            let mut d4c_same_line = 0u64;
            let mut d4c_two_line = 0u64;
            let mut d4c_shared_sb = 0u64;
            let mut d4c_general = 0u64;
            for _ in 0..d4c_rounds {
                let t = Instant::now();
                let mut acc = 0u64;
                for &(ra, rb, rc) in d4c_stream {
                    let mut target = 0u32;
                    while let Some(v) = mapped.intersection_next_value3(Col::O, ra, rb, rc, target) {
                        acc = acc.wrapping_add(v as u64);
                        target = v.saturating_add(1);
                        if target == 0 {
                            break;
                        }
                    }
                }
                black_box(acc);
                d4c_d2_ns.push(t.elapsed().as_nanos() as u64);

                let t = Instant::now();
                let mut acc = 0u64;
                for &(ra, rb, rc) in d4c_stream {
                    let mut target = 0u32;
                    loop {
                        let (v, st) = mapped.intersection_next_value3_shared(Col::O, ra, rb, rc, target);
                        d4c_frames += st.frames as u64;
                        d4c_logical_lines += st.logical_line_requests;
                        d4c_unique_lines += st.unique_line_loads;
                        d4c_logical_sbs += st.logical_sb_requests;
                        d4c_unique_sbs += st.unique_sb_loads;
                        d4c_same_line += st.all_same_line_fast_hits as u64;
                        d4c_two_line += st.two_line_fast_hits as u64;
                        d4c_shared_sb += st.shared_sb_hits as u64;
                        d4c_general += st.general_hits as u64;
                        match v {
                            None => break,
                            Some(x) => {
                                acc = acc.wrapping_add(x as u64);
                                target = x.saturating_add(1);
                                if target == 0 {
                                    break;
                                }
                            }
                        }
                    }
                }
                black_box(acc);
                d4c_sh_ns.push(t.elapsed().as_nanos() as u64);
            }
            // Correctness stream (once).
            for &(ra, rb, rc) in d4c_stream {
                let mut d2_vals = Vec::new();
                let mut target = 0u32;
                while let Some(v) = mapped.intersection_next_value3(Col::O, ra, rb, rc, target) {
                    d2_vals.push(v);
                    target = v.saturating_add(1);
                    if target == 0 {
                        break;
                    }
                }
                let mut sh_vals = Vec::new();
                let mut target = 0u32;
                loop {
                    let (v, _) = mapped.intersection_next_value3_shared(Col::O, ra, rb, rc, target);
                    match v {
                        None => break,
                        Some(x) => {
                            sh_vals.push(x);
                            target = x.saturating_add(1);
                            if target == 0 {
                                break;
                            }
                        }
                    }
                }
                if sh_vals != d2_vals {
                    d4c_stream_mismatches += 1;
                }
            }
            d4c_d2_ns.sort_unstable();
            d4c_sh_ns.sort_unstable();
            let d4c_d2_med = d4c_d2_ns[d4c_d2_ns.len() / 2] as f64;
            let d4c_sh_med = d4c_sh_ns[d4c_sh_ns.len() / 2] as f64;
            let d4c_vs_d2 = (d4c_d2_med - d4c_sh_med) / d4c_d2_med.max(1e-12);
            let d4c_line_save = 1.0 - (d4c_unique_lines as f64 / d4c_logical_lines.max(1) as f64);
            let d4c_sb_save = 1.0 - (d4c_unique_sbs as f64 / d4c_logical_sbs.max(1) as f64);
            let d4c_same_line_rate = d4c_same_line as f64 / d4c_frames.max(1) as f64;
            println!(
                "D4-C stream ({} triples): d2_ns={d4c_d2_med:.0} shared_ns={d4c_sh_med:.0} shared_vs_d2={:+.1}% mismatches={d4c_stream_mismatches}",
                d4c_stream.len(),
                d4c_vs_d2 * 100.0
            );
            println!(
                "D4-C loads: frames={d4c_frames} logical_lines={d4c_logical_lines} unique_lines={d4c_unique_lines} line_save={:+.1}% logical_sbs={d4c_logical_sbs} unique_sbs={d4c_unique_sbs} sb_save={:+.1}%",
                d4c_line_save * 100.0,
                d4c_sb_save * 100.0
            );
            println!(
                "D4-C paths: all_same_line={:.1}% two_line={:.1}% shared_sb={:.1}% general={:.1}%",
                d4c_same_line_rate * 100.0,
                d4c_two_line as f64 / d4c_frames.max(1) as f64 * 100.0,
                d4c_shared_sb as f64 / d4c_frames.max(1) as f64 * 100.0,
                d4c_general as f64 / d4c_frames.max(1) as f64 * 100.0,
            );
            if d4c_stream_mismatches > 0 {
                println!("STOP: D4-C shared ≠ D2 stream");
                std::process::exit(1);
            }
            let d4c_keep = d4c_vs_d2 >= 0.10;
            let d4c_weak = !d4c_keep && d4c_vs_d2 >= 0.05;
            let d4c_decision = if d4c_keep {
                "STRONG KEEP (≥10% shared vs D2)"
            } else if d4c_weak {
                "WEAK KEEP (5–10% shared vs D2)"
            } else if d4c_vs_d2 > 0.0 {
                "DROP (<5% shared vs D2; keep D2 successor)"
            } else {
                "DROP (shared not faster than D2; keep D2 successor)"
            };
            println!(
                "D4-C decision: shared_vs_d2={:+.1}% realized_line_save={:+.1}% (D4-A bound {:+.1}%)  {}",
                d4c_vs_d2 * 100.0,
                d4c_line_save * 100.0,
                line_save * 100.0,
                d4c_decision
            );
            println!(
                "RESULT e511_d4c shared_vs_d2={d4c_vs_d2:.4} line_save={d4c_line_save:.4} sb_save={d4c_sb_save:.4} same_line_rate={d4c_same_line_rate:.4} mismatches={d4c_stream_mismatches} zero_bytes=0 keep={}",
                d4c_keep || d4c_weak
            );

            // ── E5.11 E1: concentric presence summaries (transient oracle A/B) ───────
            // Build heap-only presence masks at cell sizes 64/128/256 for C_o, then
            // stream the same 32 product-triangle triples through summary-gated D2.
            // KEEP if any cell size is ≥10% faster than D2 (or ≥5% weak) with budget
            // ≤10% of Ring A image bytes. Image bytes are unchanged (table is transient).
            println!();
            println!("--- E1 concentric presence summaries vs repeated D2 ---");
            let e1_stream: &[(RowRange, RowRange, RowRange)] = &d2_triples[..d2_triples.len().min(32)];
            let e1_rounds = 11usize;
            let hot_o = mapped.col_hot(Col::O);
            let image_ring_a = image_bytes as f64;

            // Warm D2 once for the stream set.
            for &(ra, rb, rc) in e1_stream.iter().take(4) {
                black_box(mapped.intersection_next_value3(Col::O, ra, rb, rc, 0));
            }

            let mut e1_best_cell = PresenceCellSize::Quarter64;
            let mut e1_best_gain = f64::NEG_INFINITY;
            let mut e1_best_keep = false;
            let mut e1_best_weak = false;
            let mut e1_best_bytes = 0usize;
            let mut e1_best_budget = 0.0f64;
            let mut e1_any_mismatch = 0u64;
            let mut e1_d2_med_global = 0.0f64;

            for &cell in &PresenceCellSize::ALL {
                let t_build = Instant::now();
                let table = PresenceSummaryTable::build(hot_o, cell);
                let build_ms = t_build.elapsed().as_secs_f64() * 1e3;
                let budget_pct = 100.0 * table.bytes as f64 / image_ring_a.max(1.0);
                println!(
                    "E1-{} build: summary_bytes={} ({:.3} MiB) budget={:.2}% of image build={build_ms:.2} ms",
                    cell.as_str(),
                    table.bytes,
                    table.bytes as f64 / (1024.0 * 1024.0),
                    budget_pct
                );

                // Warm summary path.
                for &(ra, rb, rc) in e1_stream.iter().take(4) {
                    black_box(mapped.intersection_next_value3_summary(Col::O, &table, ra, rb, rc, 0));
                }

                let mut e1_d2_ns = Vec::with_capacity(e1_rounds);
                let mut e1_sm_ns = Vec::with_capacity(e1_rounds);
                let mut sum_checks = 0u64;
                let mut sum_zero = 0u64;
                let mut sum_fall = 0u64;
                let mut sum_avoided = 0u64;
                let mut sum_fp = 0u64;
                let mut sum_full_cells = 0u64;
                let mut sum_partial = 0u64;
                let mut sum_exact = 0u64;
                let mut sum_children = 0u64;
                let mut sum_hits = 0u64;

                for _ in 0..e1_rounds {
                    let t = Instant::now();
                    let mut acc = 0u64;
                    for &(ra, rb, rc) in e1_stream {
                        let mut target = 0u32;
                        while let Some(v) = mapped.intersection_next_value3(Col::O, ra, rb, rc, target) {
                            acc = acc.wrapping_add(v as u64);
                            target = v.saturating_add(1);
                            if target == 0 {
                                break;
                            }
                        }
                    }
                    black_box(acc);
                    e1_d2_ns.push(t.elapsed().as_nanos() as u64);

                    let t = Instant::now();
                    let mut acc = 0u64;
                    for &(ra, rb, rc) in e1_stream {
                        let mut target = 0u32;
                        loop {
                            let (v, st) =
                                mapped.intersection_next_value3_summary(Col::O, &table, ra, rb, rc, target);
                            sum_checks += st.summary_checks as u64;
                            sum_zero += st.summary_zero_rejects as u64;
                            sum_fall += st.summary_nonzero_fallthroughs as u64;
                            sum_avoided += st.exact_expands_avoided as u64;
                            sum_fp += st.false_positive_frames as u64;
                            sum_full_cells += st.fully_covered_cells;
                            sum_partial += st.partial_boundary_fragments;
                            sum_exact += st.frames_exact as u64;
                            sum_children += st.children_pushed as u64;
                            sum_hits += st.hit as u64;
                            match v {
                                None => break,
                                Some(x) => {
                                    acc = acc.wrapping_add(x as u64);
                                    target = x.saturating_add(1);
                                    if target == 0 {
                                        break;
                                    }
                                }
                            }
                        }
                    }
                    black_box(acc);
                    e1_sm_ns.push(t.elapsed().as_nanos() as u64);
                }

                // Correctness stream (once).
                let mut e1_mismatches = 0u64;
                for &(ra, rb, rc) in e1_stream {
                    let mut d2_vals = Vec::new();
                    let mut target = 0u32;
                    while let Some(v) = mapped.intersection_next_value3(Col::O, ra, rb, rc, target) {
                        d2_vals.push(v);
                        target = v.saturating_add(1);
                        if target == 0 {
                            break;
                        }
                    }
                    let mut sm_vals = Vec::new();
                    let mut target = 0u32;
                    loop {
                        let (v, _) =
                            mapped.intersection_next_value3_summary(Col::O, &table, ra, rb, rc, target);
                        match v {
                            None => break,
                            Some(x) => {
                                sm_vals.push(x);
                                target = x.saturating_add(1);
                                if target == 0 {
                                    break;
                                }
                            }
                        }
                    }
                    if sm_vals != d2_vals {
                        e1_mismatches += 1;
                    }
                }
                e1_any_mismatch += e1_mismatches;

                e1_d2_ns.sort_unstable();
                e1_sm_ns.sort_unstable();
                let e1_d2_med = e1_d2_ns[e1_d2_ns.len() / 2] as f64;
                let e1_sm_med = e1_sm_ns[e1_sm_ns.len() / 2] as f64;
                e1_d2_med_global = e1_d2_med;
                let e1_vs_d2 = (e1_d2_med - e1_sm_med) / e1_d2_med.max(1e-12);
                let reject_rate = sum_zero as f64 / sum_checks.max(1) as f64;
                let fp_rate = sum_fp as f64 / sum_fall.max(1) as f64;
                let budget_ok = budget_pct <= 10.0;

                println!(
                    "E1-{} stream ({} triples): d2_ns={e1_d2_med:.0} summary_ns={e1_sm_med:.0} summary_vs_d2={:+.1}% mismatches={e1_mismatches}",
                    cell.as_str(),
                    e1_stream.len(),
                    e1_vs_d2 * 100.0
                );
                println!(
                    "E1-{} counters: checks={sum_checks} zero_reject={sum_zero} ({:.1}%) fallthrough={sum_fall} avoided={sum_avoided} fp={sum_fp} ({:.1}%) full_cells={sum_full_cells} partial={sum_partial} exact_frames={sum_exact} children={sum_children} hits={sum_hits}",
                    cell.as_str(),
                    reject_rate * 100.0,
                    fp_rate * 100.0
                );

                if e1_mismatches > 0 {
                    println!("STOP: E1-{} summary ≠ D2 stream", cell.as_str());
                    std::process::exit(1);
                }

                let e1_keep = e1_vs_d2 >= 0.10 && budget_ok;
                let e1_weak = !e1_keep && e1_vs_d2 >= 0.05 && budget_ok;
                let e1_decision = if !budget_ok {
                    "DROP (budget >10% Ring A image)"
                } else if e1_keep {
                    "STRONG KEEP (≥10% summary vs D2, budget OK)"
                } else if e1_weak {
                    "WEAK KEEP (5–10% summary vs D2, budget OK)"
                } else if e1_vs_d2 > 0.0 {
                    "DROP (<5% summary vs D2; keep D2 successor)"
                } else {
                    "DROP (summary not faster than D2; keep D2 successor)"
                };
                println!(
                    "E1-{} decision: summary_vs_d2={:+.1}% budget={:.2}%  {}",
                    cell.as_str(),
                    e1_vs_d2 * 100.0,
                    budget_pct,
                    e1_decision
                );
                println!(
                    "RESULT e511_e1 cell={} summary_vs_d2={e1_vs_d2:.4} budget_pct={budget_pct:.4} bytes={} reject_rate={reject_rate:.4} fp_rate={fp_rate:.4} mismatches={e1_mismatches} keep={}",
                    cell.as_str(),
                    table.bytes,
                    e1_keep || e1_weak
                );

                if e1_vs_d2 > e1_best_gain {
                    e1_best_gain = e1_vs_d2;
                    e1_best_cell = cell;
                    e1_best_keep = e1_keep;
                    e1_best_weak = e1_weak;
                    e1_best_bytes = table.bytes;
                    e1_best_budget = budget_pct;
                }
            }

            let e1_overall = if e1_best_keep {
                "STRONG KEEP"
            } else if e1_best_weak {
                "WEAK KEEP"
            } else {
                "DROP (D2 remains product path)"
            };
            println!(
                "E1 overall: best_cell={} best_gain={:+.1}% budget={:.2}% bytes={e1_best_bytes} d2_ns={e1_d2_med_global:.0} mismatches={e1_any_mismatch}  {}",
                e1_best_cell.as_str(),
                e1_best_gain * 100.0,
                e1_best_budget,
                e1_overall
            );
            println!(
                "RESULT e511_e1_best cell={} summary_vs_d2={e1_best_gain:.4} budget_pct={e1_best_budget:.4} bytes={e1_best_bytes} mismatches={e1_any_mismatch} keep={}",
                e1_best_cell.as_str(),
                e1_best_keep || e1_best_weak
            );

            println!(
                "E5.11 campaign STATUS: B2/B1/B5 KEEP; C1 DROP; D1 KEEP; D2 KEEP; D3 {}; D4-B {}; D4-C {}; E1 {}",
                if d3_keep {
                    "KEEP"
                } else if d3_weak {
                    "WEAK KEEP"
                } else {
                    "DROP"
                },
                if d4_keep {
                    "KEEP"
                } else if d4_weak {
                    "WEAK KEEP"
                } else {
                    "DROP"
                },
                if d4c_keep {
                    "KEEP"
                } else if d4c_weak {
                    "WEAK KEEP"
                } else {
                    "DROP (D2 remains product path)"
                },
                if e1_best_keep {
                    "KEEP"
                } else if e1_best_weak {
                    "WEAK KEEP"
                } else {
                    "DROP (D2 remains product path)"
                }
            );    } else {
        println!();
        println!("--- product path complete (D1/D2) — pass --full-campaign for B5/C1/D3/D4/E1 ---");
        println!("E5.11 product STATUS: D1 KEEP; D2 KEEP (product triangle); C1/D3/D4-B/C/E1 DROP locked");
    }
    #[cfg(not(feature = "diagnostics"))]
    {
        println!();
        println!("--- product path complete (D1/D2) ---");
        println!("E5.11 product STATUS: D1 KEEP; D2 KEEP (product triangle); C1/D3/D4-B/C/E1 DROP locked");
        if full_campaign {
            println!("(full campaign matrices skipped: rebuild with --features diagnostics)");
        }
    }

}

// ── helpers ─────────────────────────────────────────────────────────────────

#[cfg(feature = "cyclic-ring-pilot")]
#[derive(Default, Clone, Copy)]
struct RdiAgg {
    opens: u64,
    symbols: u64,
    rank_probes: u64,
    frames: u64,
    empty_br: u64,
    children: u64,
    branch_tr: u64,
    unary: u64,
}

#[cfg(feature = "cyclic-ring-pilot")]
impl RdiAgg {
    fn fmt(&self) -> String {
        format!(
            "opens={} sym={} rank_probes={} frames={} empty={} children={} branch={} unary={}",
            self.opens,
            self.symbols,
            self.rank_probes,
            self.frames,
            self.empty_br,
            self.children,
            self.branch_tr,
            self.unary
        )
    }
}

#[cfg(feature = "cyclic-ring-pilot")]
fn densify_spo(quads: &[oxigraph_nova_core::Quad]) -> (Vec<[u32; 3]>, u32, u32, u32) {
    use oxigraph_nova_core::{Dictionary, GraphName, Subject, Term};
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

#[cfg(feature = "cyclic-ring-pilot")]
fn ratio(a: f64, b: f64) -> f64 {
    if b > 0.0 { a / b } else { f64::NAN }
}

#[cfg(feature = "cyclic-ring-pilot")]
fn median_f64(v: &mut [f64]) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

#[cfg(feature = "cyclic-ring-pilot")]
#[derive(Default, Clone, Copy)]
struct D3Histogram {
    empty: u64,
    singleton: u64,
    two_four: u64,
    five_sixteen: u64,
    seventeen_sixtyfour: u64,
    greater_sixtyfour: u64,
}

#[cfg(feature = "cyclic-ring-pilot")]
impl D3Histogram {
    fn observe(&mut self, len: usize) {
        match len {
            0 => self.empty += 1,
            1 => self.singleton += 1,
            2..=4 => self.two_four += 1,
            5..=16 => self.five_sixteen += 1,
            17..=64 => self.seventeen_sixtyfour += 1,
            _ => self.greater_sixtyfour += 1,
        }
    }
}

#[cfg(feature = "cyclic-ring-pilot")]
impl std::fmt::Display for D3Histogram {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "empty={} singleton={} 2-4={} 5-16={} 17-64={} >64={}",
            self.empty,
            self.singleton,
            self.two_four,
            self.five_sixteen,
            self.seventeen_sixtyfour,
            self.greater_sixtyfour
        )
    }
}

#[cfg(feature = "cyclic-ring-pilot")]
#[derive(Default, Clone, Copy)]
struct D3Timing {
    range_construction: u128,
    a_lookup: u128,
    f_lf_projection: u128,
    braid_init_traversal: u128,
    successor_restarts: u128,
    result_translation: u128,
    loop_control: u128,
    louds_operations: u128,
}

#[cfg(feature = "cyclic-ring-pilot")]
#[derive(Default)]
struct D3SeedRow {
    seed: u32,
    product_calls: u64,
    product_commons: u64,
    product_levels: u64,
    product_expands: u64,
    product_ns: u128,
    louds_ns: u128,
}

#[cfg(feature = "cyclic-ring-pilot")]
#[derive(Default)]
struct D3Profile {
    triangle_seeds: u64,
    braid_calls: u64,
    successful_calls: u64,
    empty_calls: u64,
    root_restarts: u64,
    common_emitted: u64,
    levels_visited: u64,
    range_expands: u64,
    range_count: u64,
    singleton_ranges: u64,
    hist: D3Histogram,
    timing: D3Timing,
    product_mismatches: u64,
    stream_mismatches: u64,
    baseline_mismatches: u64,
    persistent_bytes: usize,
    per_seed: Vec<D3SeedRow>,
}

#[cfg(feature = "cyclic-ring-pilot")]
fn nanos_ms(ns: u128) -> f64 {
    ns as f64 / 1_000_000.0
}

#[cfg(feature = "cyclic-ring-pilot")]
#[cfg(feature = "diagnostics")]
fn profile_d3(
    mapped: &oxigraph_nova_storage_ring::cyclic_ring::mapped_ring::MappedRingA,
    louds: &oxigraph_nova_storage_ring::LoudsTrie,
    seeds: &[u32],
    n_ent: u32,
) -> D3Profile {
    use oxigraph_nova_storage_ring::cyclic_ring::{Col, RowRange};

    let mut out = D3Profile {
        triangle_seeds: seeds.len() as u64,
        persistent_bytes: 0,
        ..D3Profile::default()
    };

    for &a in seeds {
        let mut row = D3SeedRow {
            seed: a,
            ..D3SeedRow::default()
        };
        let product_start = Instant::now();
        for d in 1..=3u32 {
            let t_loop = Instant::now();
            let b = (a + d) % n_ent.max(1);
            let c = (b + 1) % n_ent.max(1);
            out.timing.loop_control += t_loop.elapsed().as_nanos();

            let t_range = Instant::now();
            let ra = mapped.range_s(a).unwrap_or(RowRange::empty());
            let rb = mapped.range_s(b).unwrap_or(RowRange::empty());
            let rc = mapped.range_s(c).unwrap_or(RowRange::empty());
            out.timing.range_construction += t_range.elapsed().as_nanos();
            for r in [ra, rb, rc] {
                let len = r.len() as usize;
                out.range_count += 1;
                out.hist.observe(len);
                if len == 1 {
                    out.singleton_ranges += 1;
                }
            }

            // A lookup is separated from range construction for attribution.
            // range_s itself necessarily performs an A lookup; these reads are
            // diagnostic and black-boxed, with no image mutation or allocation.
            let t_a = Instant::now();
            black_box(mapped.a_at(Col::S, a));
            black_box(mapped.a_at(Col::S, b));
            black_box(mapped.a_at(Col::S, c));
            out.timing.a_lookup += t_a.elapsed().as_nanos();

            let t_braid = Instant::now();
            let (mut value, stats) = mapped.intersection_next_value3_counted(Col::O, ra, rb, rc, 0);
            out.timing.braid_init_traversal += t_braid.elapsed().as_nanos();
            out.braid_calls += 1;
            row.product_calls += 1;
            out.root_restarts += stats.root_restarts as u64;
            out.levels_visited += stats.levels_visited as u64;
            out.range_expands += stats.expands as u64;
            row.product_levels += stats.levels_visited as u64;
            row.product_expands += stats.expands as u64;

            // The current product path has no F/LF projection or mapped result
            // translation. Keep the buckets explicit and zero rather than
            // timing an unrelated operation.
            while let Some(v) = value {
                out.successful_calls += 1;
                out.common_emitted += 1;
                row.product_commons += 1;
                let t_translate = Instant::now();
                black_box(v);
                out.timing.result_translation += t_translate.elapsed().as_nanos();

                let next = v.saturating_add(1);
                if next == 0 {
                    break;
                }
                let t_restart = Instant::now();
                let (next_value, next_stats) =
                    mapped.intersection_next_value3_counted(Col::O, ra, rb, rc, next);
                out.timing.successor_restarts += t_restart.elapsed().as_nanos();
                out.braid_calls += 1;
                row.product_calls += 1;
                out.root_restarts += next_stats.root_restarts as u64;
                out.levels_visited += next_stats.levels_visited as u64;
                out.range_expands += next_stats.expands as u64;
                row.product_levels += next_stats.levels_visited as u64;
                row.product_expands += next_stats.expands as u64;
                value = next_value;
            }
            if value.is_none() {
                out.empty_calls += 1;
            }

            // Exact per-seek validation:
            // 1) D2 successor vs three-RNV oracle (stream_mismatches)
            // 2) D3 iterator vs D2 successive stream (product_mismatches)
            let mut target = 0u32;
            let mut d2_stream = Vec::new();
            loop {
                let got = mapped.intersection_next_value3(Col::O, ra, rb, rc, target);
                let expected = mapped.intersection_next_value3_dual_rnv(Col::O, ra, rb, rc, target);
                if got != expected {
                    out.stream_mismatches += 1;
                    break;
                }
                let Some(v) = got else { break };
                d2_stream.push(v);
                target = v.saturating_add(1);
                if target == 0 {
                    break;
                }
            }
            let iter_stream: Vec<u32> = mapped.intersection_iter3(Col::O, ra, rb, rc, 0).collect();
            if iter_stream != d2_stream {
                out.product_mismatches += 1;
            }
        }
        row.product_ns = product_start.elapsed().as_nanos();

        // Fair LOUDS baseline: same common-object existence as Ring D2
        // (louds_triangle). Self-check determinism; main gate is in main().
        let t_louds = Instant::now();
        let louds_result = louds_triangle(louds, a, n_ent);
        row.louds_ns = t_louds.elapsed().as_nanos();
        let expected_louds = louds_triangle(louds, a, n_ent);
        if louds_result != expected_louds {
            out.baseline_mismatches += 1;
        }

        out.timing.louds_operations += row.louds_ns;
        out.per_seed.push(row);
    }

    // Persistent stack lives only on the iterator; image bytes unchanged.
    // Zero-byte accounting: sizeof stack frame array is stack-local only.
    out.persistent_bytes = 0;
    out
}

#[cfg(feature = "cyclic-ring-pilot")]
fn star_heap(ring: &oxigraph_nova_storage_ring::cyclic_ring::CyclicRing, s: u32) -> u32 {
    use oxigraph_nova_storage_ring::cyclic_ring::Col;
    let r = ring.range_s(s);
    if r.is_empty() {
        return 0;
    }
    let mut n = 0u32;
    let mut it = ring.range_distinct_iter(Col::O, r);
    while let Some((_v, c)) = it.next() {
        black_box(c);
        n += 1;
    }
    n
}

#[cfg(feature = "cyclic-ring-pilot")]
fn star_mmap(
    mapped: &oxigraph_nova_storage_ring::cyclic_ring::mapped_ring::MappedRingA,
    s: u32,
) -> u32 {
    use oxigraph_nova_storage_ring::cyclic_ring::Col;
    let r = match mapped.range_s(s) {
        Some(r) if !r.is_empty() => r,
        _ => return 0,
    };
    let mut n = 0u32;
    let mut it = mapped.range_distinct_iter(Col::O, r).unwrap();
    while let Some((_v, c)) = it.next_symbol() {
        black_box(c);
        n += 1;
    }
    n
}

#[cfg(feature = "cyclic-ring-pilot")]
fn triangle_heap(
    ring: &oxigraph_nova_storage_ring::cyclic_ring::CyclicRing,
    a: u32,
    n_ent: u32,
) -> u64 {
    use oxigraph_nova_storage_ring::cyclic_ring::Col;
    let mut hits = 0u64;
    for d in 1..=3u32 {
        let b = (a + d) % n_ent;
        let ra = ring.range_s(a);
        let rb = ring.range_s(b);
        let c = (b + 1) % n_ent;
        let rc = ring.range_s(c);
        if ra.is_empty() || rb.is_empty() || rc.is_empty() {
            continue;
        }

        // Heap baseline for the product triangle: three independent guided
        // RNV seeks, synchronized by the usual leapfrog target.
        let mut target = 0u32;
        for _ in 0..=ring.universe.saturating_add(3) {
            let Some(x) = ring.range_next_value_native(Col::O, ra, target) else {
                break;
            };
            let Some(y) = ring.range_next_value_native(Col::O, rb, x) else {
                break;
            };
            let Some(z) = ring.range_next_value_native(Col::O, rc, y) else {
                break;
            };
            if x == y && y == z {
                hits += 1;
                break;
            }
            target = z;
        }
    }
    hits
}

#[cfg(feature = "cyclic-ring-pilot")]
fn triangle_mmap(
    mapped: &oxigraph_nova_storage_ring::cyclic_ring::mapped_ring::MappedRingA,
    a: u32,
    n_ent: u32,
) -> u64 {
    use oxigraph_nova_storage_ring::cyclic_ring::{Col, RowRange};
    let mut hits = 0u64;
    for d in 1..=3u32 {
        let b = (a + d) % n_ent;
        let ra = mapped.range_s(a).unwrap_or(RowRange::empty());
        let rb = mapped.range_s(b).unwrap_or(RowRange::empty());
        let c = (b + 1) % n_ent;
        let rc = mapped.range_s(c).unwrap_or(RowRange::empty());
        if mapped
            .intersection_next_value3(Col::O, ra, rb, rc, 0)
            .is_some()
        {
            hits += 1;
        }
    }
    hits
}

#[cfg(feature = "cyclic-ring-pilot")]
fn triangle_rnv_count(
    ring: &oxigraph_nova_storage_ring::cyclic_ring::CyclicRing,
    a: u32,
    n_ent: u32,
) -> u64 {
    let mut n = 0u64;
    for d in 1..=3u32 {
        let b = (a + d) % n_ent;
        let ra = ring.range_s(a);
        let rb = ring.range_s(b);
        let c = (b + 1) % n_ent;
        let rc = ring.range_s(c);
        if !ra.is_empty() && !rb.is_empty() && !rc.is_empty() {
            // This is the maximum number of guided RNV calls in one
            // three-range leapfrog seek; it is a diagnostic, not a count of
            // calls avoided by the braided mapped implementation.
            n += 3;
        }
    }
    n
}

#[cfg(feature = "cyclic-ring-pilot")]
fn louds_open_s(trie: &oxigraph_nova_storage_ring::LoudsTrie, s: u32) -> Option<(usize, usize)> {
    let deg = trie.root_degree();
    if deg == 0 {
        return None;
    }
    let lo = 1usize;
    let hi = deg;
    let k = trie.leap(lo, hi, s);
    if k > hi || trie.label_at(k) != s {
        return None;
    }
    let child = trie.child_from_label_pos(k);
    let d = trie.degree(child);
    if d == 0 {
        return None;
    }
    Some((child + 1, d))
}

/// Fixed upper bound for predicate children under one subject in this harness.
/// Realistic generator uses np=7; keep headroom without heap allocation.
#[cfg(feature = "cyclic-ring-pilot")]
const LOUDS_MAX_P_CHILDREN: usize = 32;

/// One predicate's sorted object-label window under a subject (shared alphabet).
#[cfg(feature = "cyclic-ring-pilot")]
#[derive(Clone, Copy)]
struct LoudsORun {
    /// Inclusive lower bound of the O-label window (kept for open diagnostics).
    #[allow(dead_code)]
    lo: usize,
    hi: usize, // inclusive
    cur: usize,
}

/// Allocation-free distinct-object successor over one subject in SPO LOUDS.
///
/// Within one (S,P) run, objects are sorted unique. Across predicates the same
/// object may recur; `next_ge` returns the least distinct shared-alphabet O
/// label ≥ target by multi-cursor leap + min selection.
#[cfg(feature = "cyclic-ring-pilot")]
struct LoudsDistinctOCursor<'a> {
    trie: &'a oxigraph_nova_storage_ring::LoudsTrie,
    runs: [LoudsORun; LOUDS_MAX_P_CHILDREN],
    n_runs: usize,
}

#[cfg(feature = "cyclic-ring-pilot")]
impl<'a> LoudsDistinctOCursor<'a> {
    fn open(trie: &'a oxigraph_nova_storage_ring::LoudsTrie, s: u32) -> Option<Self> {
        let (p_lo, p_deg) = louds_open_s(trie, s)?;
        debug_assert!(
            p_deg <= LOUDS_MAX_P_CHILDREN,
            "subject {s} has {p_deg} predicates; raise LOUDS_MAX_P_CHILDREN"
        );
        let mut runs = [LoudsORun {
            lo: 0,
            hi: 0,
            cur: 0,
        }; LOUDS_MAX_P_CHILDREN];
        let mut n_runs = 0usize;
        for i in 0..p_deg.min(LOUDS_MAX_P_CHILDREN) {
            let p_pos = p_lo + i;
            let sp_node = trie.child_from_label_pos(p_pos);
            let o_deg = trie.degree(sp_node);
            if o_deg == 0 {
                continue;
            }
            let o_lo = sp_node + 1;
            let o_hi = o_lo + o_deg - 1;
            runs[n_runs] = LoudsORun {
                lo: o_lo,
                hi: o_hi,
                cur: o_lo,
            };
            n_runs += 1;
        }
        if n_runs == 0 {
            return None;
        }
        Some(Self { trie, runs, n_runs })
    }

    /// Least distinct object label ≥ `target`, or None if exhausted.
    ///
    /// Leaves each run's cursor on its first label ≥ target (does **not**
    /// consume). Call [`Self::advance_past`] after a confirmed match so
    /// leapfrog can re-seek with a higher target without losing the value.
    #[inline]
    fn next_ge(&mut self, target: u32) -> Option<u32> {
        let mut best: Option<u32> = None;
        for i in 0..self.n_runs {
            let run = &mut self.runs[i];
            if run.cur > run.hi {
                continue;
            }
            // Advance this predicate's O window to first label ≥ target.
            if self.trie.label_at(run.cur) < target {
                let k = self.trie.leap(run.cur, run.hi, target);
                if k > run.hi {
                    run.cur = run.hi + 1;
                    continue;
                }
                run.cur = k;
            }
            let v = self.trie.label_at(run.cur);
            best = Some(match best {
                Some(b) if b <= v => b,
                _ => v,
            });
        }
        best
    }

    /// Advance past a confirmed distinct object `v` (all runs currently on `v`).
    #[inline]
    fn advance_past(&mut self, v: u32) {
        for i in 0..self.n_runs {
            let run = &mut self.runs[i];
            if run.cur <= run.hi && self.trie.label_at(run.cur) == v {
                run.cur += 1;
            }
        }
    }

    /// Count distinct objects under the subject (fair vs Ring RDI).
    fn count_distinct(mut self) -> u32 {
        let mut n = 0u32;
        let mut target = 0u32;
        while let Some(v) = self.next_ge(target) {
            black_box(v);
            n += 1;
            self.advance_past(v);
            target = v.saturating_add(1);
            if target == 0 {
                break;
            }
        }
        n
    }
}

/// Distinct object labels under subject `s` (union over predicates).
/// Fair vs Ring RDI on C_o under range_s — counts each O once even if it
/// appears under multiple predicates.
#[cfg(feature = "cyclic-ring-pilot")]
fn louds_star(trie: &oxigraph_nova_storage_ring::LoudsTrie, s: u32) -> u32 {
    match LoudsDistinctOCursor::open(trie, s) {
        Some(cur) => cur.count_distinct(),
        None => 0,
    }
}

/// First common shared-alphabet object under subjects a,b,c, or None.
/// Matches Ring D2 `intersection_next_value3(..., 0)` on the shared alphabet
/// (S∈[0,ns), P∈[ns,ns+np), O∈[ns+np,U)) used by both Ring and LOUDS builds.
#[cfg(feature = "cyclic-ring-pilot")]
fn louds_first_common_object(
    trie: &oxigraph_nova_storage_ring::LoudsTrie,
    a: u32,
    b: u32,
    c: u32,
) -> Option<u32> {
    let mut ca = LoudsDistinctOCursor::open(trie, a)?;
    let mut cb = LoudsDistinctOCursor::open(trie, b)?;
    let mut cc = LoudsDistinctOCursor::open(trie, c)?;
    let mut target = 0u32;
    // Bound iterations by alphabet size roughly (shared labels are u32).
    for _ in 0..1_000_000 {
        let x = ca.next_ge(target)?;
        let y = cb.next_ge(x)?;
        let z = cc.next_ge(y)?;
        if x == y && y == z {
            return Some(x);
        }
        target = z;
        if target == u32::MAX {
            break;
        }
    }
    None
}

/// Fair LOUDS product triangle: first-common-object existence under (a,b,c)
/// for the same seed offsets as Ring D2. Counts Boolean hits only.
#[cfg(feature = "cyclic-ring-pilot")]
fn louds_triangle(trie: &oxigraph_nova_storage_ring::LoudsTrie, a: u32, n_ent: u32) -> u64 {
    let mut hits = 0u64;
    for d in 1..=3u32 {
        let b = (a + d) % n_ent.max(1);
        let c = (b + 1) % n_ent.max(1);
        if louds_first_common_object(trie, a, b, c).is_some() {
            hits += 1;
        }
    }
    hits
}

/// Legacy diagnostic only: subject-open existence (NOT a fair G3 baseline).
#[cfg(feature = "cyclic-ring-pilot")]
fn louds_subject_open_probe(
    trie: &oxigraph_nova_storage_ring::LoudsTrie,
    a: u32,
    n_ent: u32,
) -> u64 {
    let mut hits = 0u64;
    for d in 1..=3u32 {
        let b = (a + d) % n_ent.max(1);
        let ra = louds_open_s(trie, a);
        let rb = louds_open_s(trie, b);
        if ra.is_some() && rb.is_some() {
            let c = (b + 1) % n_ent.max(1);
            if louds_open_s(trie, c).is_some() {
                hits += 1;
            }
        }
    }
    hits
}

#[cfg(not(feature = "cyclic-ring-pilot"))]
fn main() {
    eprintln!(
        "e511_ring_perf_profile requires --features cyclic-ring-pilot\n\
         cargo run -p oxigraph-nova-bench --release --features cyclic-ring-pilot --bin e511_ring_perf_profile"
    );
    std::process::exit(2);
}
