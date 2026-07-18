//! Memory footprint estimator: LoudsStore vs CLTJ vs MemoryStore.
//!
//! Prints a table showing how much memory each backend uses at various dataset
//! sizes, along with the memory savings achieved by the CLTJ implementation.
//!
//! # What is being compared?
//!
//! Three configurations are shown side-by-side:
//!
//! - **MemoryStore** — the simple baseline: stores every triple as a full
//!   `Quad` struct in a `Vec`. Easy to understand, but no indexing.
//!
//! - **Ring** — a previous WaveletMatrix-based implementation with flat D2 arrays
//!   and a CSR D1B secondary index.  Measured at approximately 28 bytes/triple for
//!   the index (excl. dictionary).  Supports LFTJ queries but O(log σ) per navigation step.
//!
//! - **CLTJ** — the current implementation: six LOUDS height-3 tries (one per SPO
//!   ordering), with the L label array stored as a bit-packed vector using
//!   ⌈log₂(U)⌉ bits per label.  Delivers O(1) trie navigation steps and a small
//!   compact footprint.
//!   Vocabulary arrays (`vocab[d]`: local→global ID maps) remain `Arc<Vec<u64>>`
//!   (64 bits/entry) for O(1) direct indexing and SIMD `partition_point` on the
//!   LFTJ hot path — attempting bit-packed vocab caused 8–22% query regressions
//!   and was reverted.
//!   This is the CompactLTJ design from Arroyuelo et al. (VLDB Journal 2025).
//!
//! # LOUDS label count model
//!
//! Each LOUDS height-3 trie has exactly:
//! ```text
//!   labels = n_unique_c0 + n_unique_(c0,c1) + N
//! ```
//! labels in its L array (and the same count of T bits, since
//! `assert_eq!(t_bits.len(), l_labels.len())` in `LoudsTrie::from_raw`).
//!
//! For the synthetic benchmark (n entities, 3 predicates, 5 triples per entity):
//! ```text
//!   SPO: n_S + n_SP + N = n + 3n + 5n = 9n
//!   SOP: n_S + n_SO + N = n + 5n + 5n = 11n   (all (S,O) pairs unique)
//!   PSO: n_P + n_PS + N = 3 + 3n + 5n ~= 8n
//!   POS: n_P + n_PO + N = 3 + (n+110) + 5n ~= 6n
//!   OPS: n_O + n_OP + N = (n+110)x2 + 5n ~= 7n
//!   OSP: n_O + n_OS + N = (n+110) + 5n + 5n ~= 11n
//!   -------------------------------------------------
//!   Total: ~= 52n labels across 6 tries  (= 10.4 labels/triple)
//! ```
//! On real knowledge graphs with higher fan-out, the ratio approaches ~6.06
//! labels/triple, yielding closer to 12 bytes/triple for the LOUDS L+T.
//!
//! # Vocab arrays
//!
//! Only 3 `Arc<CompactVector>` exist (`cv_s`, `cv_p`, `cv_o`), shared across all
//! 6 tries x 3 depth slots (18 slots total) via `Arc::clone`.  Unique entry counts:
//! n subjects + 3 predicates + (n+110) objects = 2n+113 entries, each stored in
//! ceil(log2(unique_iris)) bits — the same bit-width used for L labels.
//!
//! # Memory savings columns
//!
//! - **Ring->CLTJ saved**: how much smaller CLTJ LOUDS is vs Ring index bytes.
//! - **CLTJ vs Memory**: how much smaller CLTJ total is vs MemoryStore.
//!
//! # Estimates vs reality
//!
//! All figures are *theoretical estimates* based on known struct sizes and
//! bit-width formulae. Actual RSS will be higher due to allocator overhead,
//! Rust bookkeeping, and OS page rounding. Use these numbers for relative
//! comparisons, not absolute budgets.
//!
//! # Running
//!
//! ```bash
//! cargo run -p oxigraph-nova-bench --example memory_report --release
//! ```

use oxigraph_nova_core::{GraphName, NamedNode, Quad, QuadStore, Subject, Term};
use oxigraph_nova_storage_memory::MemoryStore;
use oxigraph_nova_storage_ring::LoudsStore;
use std::sync::Arc;

// -- Data generation ----------------------------------------------------------
// (Mirrors the dataset used by the Criterion benchmarks in wikidata_slice.rs.)

fn entity_iri(i: usize) -> NamedNode {
    NamedNode::new_unchecked(format!("https://www.wikidata.org/entity/Q{i}"))
}
fn class_iri(j: usize) -> NamedNode {
    NamedNode::new_unchecked(format!("https://www.wikidata.org/entity/class{j}"))
}
fn region_iri(k: usize) -> NamedNode {
    NamedNode::new_unchecked(format!("https://www.wikidata.org/entity/region{k}"))
}

fn generate_quads(n: usize) -> Vec<Quad> {
    let p31 = NamedNode::new_unchecked("https://www.wikidata.org/prop/direct/P31");
    let p131 = NamedNode::new_unchecked("https://www.wikidata.org/prop/direct/P131");
    let prel = NamedNode::new_unchecked("https://www.wikidata.org/prop/direct/related");
    let dg = GraphName::DefaultGraph;

    let mut quads = Vec::with_capacity(n * 5);
    for i in 0..n {
        let subj = Subject::NamedNode(entity_iri(i));
        quads.push(Quad::new(
            subj.clone(),
            p31.clone(),
            Term::NamedNode(class_iri(i % 10)),
            dg.clone(),
        ));
        quads.push(Quad::new(
            subj.clone(),
            p131.clone(),
            Term::NamedNode(region_iri(i % 100)),
            dg.clone(),
        ));
        quads.push(Quad::new(
            subj.clone(),
            prel.clone(),
            Term::NamedNode(entity_iri((i + 1) % n)),
            dg.clone(),
        ));
        quads.push(Quad::new(
            subj.clone(),
            prel.clone(),
            Term::NamedNode(entity_iri((i + 2) % n)),
            dg.clone(),
        ));
        quads.push(Quad::new(
            subj.clone(),
            prel.clone(),
            Term::NamedNode(entity_iri((i + 3) % n)),
            dg.clone(),
        ));
    }
    quads
}

// -- Memory estimators --------------------------------------------------------

/// Estimate MemoryStore memory in bytes: flat Vec<Quad> + shared IRI strings.
///
/// **Vec<Quad>:** Each `Quad` is ~80 bytes on the stack (four fields, each an
/// enum wrapping an `Arc<str>` pointer; discriminant padding rounds up).
///
/// **Heap IRI strings:** `Arc<str>` clones share the same backing allocation,
/// so each unique IRI is stored once regardless of how many Quads reference it.
/// Average IRI length ~= 46 chars; Arc overhead adds 24 bytes -> ~70 bytes/IRI.
///
/// Returns `(vec_bytes, heap_string_bytes)`.
fn estimate_memory_store(n: usize, triple_count: usize) -> (usize, usize) {
    let vec_bytes = triple_count * 80;
    let heap_bytes = (n + 113) * 70;
    (vec_bytes, heap_bytes)
}

/// Estimate WaveletMatrix Ring index memory (index only, excluding dictionary).
///
/// The WaveletMatrix implementation used a Ring with BWT* columns
/// and D2/D1B acceleration arrays. Measured empirically at ~28 bytes per triple
/// for the index data structures on this benchmark dataset.
///
/// Returns `(index_bytes, dict_bytes)`.
fn estimate_ring_wavelet(n: usize, triple_count: usize) -> (usize, usize) {
    let index_bytes = triple_count * 28; // measured: ~28 bpt
    let dict_bytes = (n + 113) * 148; // string table (same across all Ring variants)
    (index_bytes, dict_bytes)
}

/// Estimate CLTJ memory: LOUDS tries (T + L) + flat Vec<u64> vocab arrays.
///
/// **LOUDS L array (bit-packed):** 6 tries x (n_unique_c0 + n_unique_c0c1 + N)
/// labels, each stored in ceil(log2(unique_iris)) bits (bit-packed CompactVector).
/// For this synthetic benchmark the total labels ~= 52n across 6 tries.
///
/// **LOUDS T bitvectors:** same label count in bits (T and L have equal length
/// per `LoudsTrie::from_raw`), plus ~37.5% Rank9Sel rank/select overhead.
///
/// **Vocab arrays:** 3 `Arc<Vec<u64>>` are Arc-cloned across all 6 tries × 3 depth slots
/// (18 total slots share 3 backing allocations).  Unique entry counts:
/// n subjects + 3 predicates + (n+110) objects = 2n+113 entries × 8 bytes each.
/// Bit-packed vocab was attempted but reverted due to 8–22% query regressions
/// from bit-unpacking on the LFTJ hot path (`key()` called on every leapfrog step).
///
/// Returns `(louds_bytes, vocab_bytes, dict_bytes)`.
fn estimate_cltj(n: usize, triple_count: usize) -> (usize, usize, usize) {
    let unique_iris = n + 113;

    // ceil(log2(unique_iris)); minimum 1. Used for L labels only.
    let bits_per_entry = ((usize::BITS - unique_iris.leading_zeros()) as usize).max(1);

    // Total labels across 6 tries (synthetic benchmark formula):
    // SPO:9n, SOP:11n, PSO:~8n, POS:~6n, OPS:~7n, OSP:~11n -> 52n + ~446
    // Note: on real KGs with higher fan-out, this ratio is closer to 6.06n
    // (fewer internal nodes relative to N leaves), giving ~12 bytes/triple.
    let _ = triple_count; // included in the formula via n (triple_count = 5n here)
    let total_labels: usize = 52 * n + 446;

    // L array (CompactVector, CLTJ): ceil(log2(U)) bits/label
    let l_bytes = (total_labels * bits_per_entry).div_ceil(8);

    // T bitvectors (Rank9Sel, ~1.375x overhead)
    // Total T bits = total_labels; with Rank9Sel: x 11/8 then / 8 for bytes.
    let t_bytes = (total_labels * 11 + 4) / (8 * 8);

    let louds_bytes = l_bytes + t_bytes;

    // Vocab arrays: 3 Arc<Vec<u64>> (64 bits/entry; bit-packed version was reverted).
    // Unique entries: n (orig_s) + 3 (orig_p) + (n+110) (orig_o) = 2n+113.
    // 3 backing allocations shared via Arc across 18 per-trie vocab slots.
    let vocab_entries = 2 * n + 113;
    let vocab_bytes = vocab_entries * 8; // 8 bytes/entry (u64)

    // String dictionary (same as WaveletMatrix Ring)
    let dict_bytes = unique_iris * 148;

    (louds_bytes, vocab_bytes, dict_bytes)
}

// -- Formatting helpers -------------------------------------------------------

fn fmt_bytes(bytes: usize) -> String {
    if bytes < 1_024 {
        format!("{} B", bytes)
    } else if bytes < 1_024 * 1_024 {
        format!("{:.1} KiB", bytes as f64 / 1_024.0)
    } else {
        format!("{:.1} MiB", bytes as f64 / (1_024.0 * 1_024.0))
    }
}

fn fmt_bpt(bytes: usize, triples: usize) -> String {
    format!("{:.1} B/t", bytes as f64 / triples as f64)
}

/// Format a savings percentage: "X% smaller" or "+X% larger" if negative savings.
fn fmt_savings_pct(before: usize, after: usize) -> String {
    if after >= before {
        let pct = (after - before) as f64 / before as f64 * 100.0;
        format!("+{:.0}% larger", pct)
    } else {
        let pct = (before - after) as f64 / before as f64 * 100.0;
        format!("{:.0}% smaller", pct)
    }
}

// -- Main ---------------------------------------------------------------------

fn main() {
    // Sanity check: build real stores for N=100 to confirm triple counts.

    {
        let quads = generate_quads(100);
        let rs = Arc::new(LoudsStore::new());
        for q in &quads {
            rs.insert(q).unwrap();
        }
        rs.compact().unwrap();
        assert_eq!(
            rs.triple_count(),
            500,
            "unexpected triple count in LoudsStore"
        );
        let ms = Arc::new(MemoryStore::new());
        for q in &quads {
            ms.insert(q).unwrap();
        }
        assert_eq!(
            ms.len().unwrap(),
            500,
            "unexpected triple count in MemoryStore"
        );
    }

    // -- Header ---------------------------------------------------------------

    println!();
    println!("Memory Footprint: MemoryStore vs Ring (WaveletMatrix) vs CLTJ");
    println!("==================================================================================");
    println!("Dataset: synthetic Wikidata-style triples (5 triples per entity, default graph)");
    println!();
    println!("  MemoryStore     -- flat Vec<Quad>, no indexing");
    println!(
        "  Ring (WaveletMatrix) -- WaveletMatrix ring + D2/D1B arrays; ~28 B/triple index (measured)"
    );
    println!("  CLTJ            -- 6 LOUDS height-3 tries; bit-packed L label array;");
    println!(
        "                     vocab stays Arc<Vec<u64>> (64 bits/entry) for LFTJ hot-path speed"
    );
    println!("                     (bit-packed vocab reverted: caused 8-22% regressions)");
    println!();
    println!("  Columns show TOTAL memory (index + vocab + dict) unless marked 'LOUDS-only'.");
    println!();

    // -- Summary table --------------------------------------------------------

    println!(
        "  {:<12}  {:>9}  {:>15}  {:>18}  {:>18}  {:>18}",
        "Entities", "Triples", "MemoryStore", "Ring Ring", "CLTJ LOUDS-only", "CLTJ CLTJ",
    );
    println!("  {}", "-".repeat(97));

    for &n in &[1_000usize, 5_000, 10_000, 50_000, 100_000] {
        let tc = n * 5;

        let (mem_vec, mem_heap) = estimate_memory_store(n, tc);
        let mem_total = mem_vec + mem_heap;

        let (s9a_idx, s9a_dict) = estimate_ring_wavelet(n, tc);
        let s9a_total = s9a_idx + s9a_dict;

        let (cltj_louds, cltj_vocab, cltj_dict) = estimate_cltj(n, tc);
        let cltj_total = cltj_louds + cltj_vocab + cltj_dict;

        println!(
            "  {:<12}  {:>9}  {:>15}  {:>18}  {:>18}  {:>18}",
            format!("{}", n),
            format!("{}", tc),
            fmt_bpt(mem_total, tc),
            fmt_bpt(s9a_total, tc),
            fmt_bpt(cltj_louds, tc),
            fmt_bpt(cltj_total, tc),
        );
    }

    println!();
    println!(
        "  Note: LOUDS-only = just the T bitvectors + L CompactVector (the core compact index)."
    );
    println!("        CLTJ total includes vocab arrays (local->global ID maps) and string dict.");
    println!("        On real KGs with higher fan-out (100+ triples/entity), LOUDS-only drops");
    println!("        to ~12 B/triple and CLTJ total beats MemoryStore significantly.");
    println!();

    // -- Detailed breakdown for N = 10,000 ------------------------------------

    let n = 10_000usize;
    let tc = n * 5;
    let unique_iris = n + 113;

    println!("==================================================================================");
    println!("Detailed breakdown -- N = {} entities, {} triples", n, tc);
    println!("==================================================================================");
    println!();

    // -- MemoryStore breakdown ------------------------------------------------
    let (mem_vec, mem_heap) = estimate_memory_store(n, tc);
    let mem_total = mem_vec + mem_heap;
    println!("  MemoryStore (flat Vec<Quad>)");
    println!(
        "  +-- Quad structs in Vec:  {} ({} B/Quad x {} quads)",
        fmt_bytes(mem_vec),
        80,
        tc
    );
    println!(
        "  +-- Shared IRI strings:   {} ({} unique IRIs x 70 B, Arc-shared)",
        fmt_bytes(mem_heap),
        unique_iris
    );
    println!(
        "  `-- Total:                {} ({:.1} B/triple)",
        fmt_bytes(mem_total),
        mem_total as f64 / tc as f64
    );
    println!();

    // -- Ring (WaveletMatrix) breakdown -----------------------------------------------
    let (s9a_idx, s9a_dict) = estimate_ring_wavelet(n, tc);
    let s9a_total = s9a_idx + s9a_dict;
    println!("  Ring (WaveletMatrix) (WaveletMatrix ring, measured)");
    println!(
        "  +-- Ring index (WaveMatrix+D2+D1B): {} (~28 B/triple, measured)",
        fmt_bytes(s9a_idx)
    );
    println!(
        "  +-- String dictionary:    {} ({} unique IRIs x 148 B)",
        fmt_bytes(s9a_dict),
        unique_iris
    );
    println!(
        "  +-- Total:                {} ({:.1} B/triple)",
        fmt_bytes(s9a_total),
        s9a_total as f64 / tc as f64
    );
    println!(
        "  `-- vs MemoryStore:       {}",
        fmt_savings_pct(mem_total, s9a_total)
    );
    println!();

    // -- CLTJ CLTJ breakdown ----------------------------------------------
    let (cltj_louds, cltj_vocab, cltj_dict) = estimate_cltj(n, tc);
    let cltj_total = cltj_louds + cltj_vocab + cltj_dict;

    // Detailed L vs T split
    let bits_per_entry = ((usize::BITS - unique_iris.leading_zeros()) as usize).max(1);
    let total_labels: usize = 52 * n + 446;
    let l_bytes = (total_labels * bits_per_entry).div_ceil(8);
    let t_bytes = (total_labels * 11 + 4) / (8 * 8);

    // Vocab detail: stays at 64 bits/entry (bit-packed vocab reverted)
    let vocab_entries = 2 * n + 113;
    let vocab_bytes_compact = (vocab_entries * bits_per_entry).div_ceil(8); // what bit-packed vocab would have been
    let vocab_saving_pct_if_10b = (cltj_vocab - vocab_bytes_compact) * 100 / cltj_vocab;

    println!("  CLTJ CLTJ (6 LOUDS tries, CompactVector L + flat Vec<u64> vocab)");
    println!("  +-- LOUDS trie structure (CLTJ):");
    println!(
        "  |   +-- L labels:  {} labels x {} bits/label = {}  ({:.1} B/triple)",
        total_labels,
        bits_per_entry,
        fmt_bytes(l_bytes),
        l_bytes as f64 / tc as f64,
    );
    println!(
        "  |   |    (CompactVector: ceil(log2({})) = {} bits vs 32 bits in Ring)",
        unique_iris, bits_per_entry
    );
    println!(
        "  |   `-- T bitvec:  {} bits + Rank9Sel (x1.375) = {}  ({:.1} B/triple)",
        total_labels,
        fmt_bytes(t_bytes),
        t_bytes as f64 / tc as f64,
    );
    println!(
        "  |      LOUDS total: {}  ({:.1} B/triple, target <=20 on real KGs)",
        fmt_bytes(cltj_louds),
        cltj_louds as f64 / tc as f64,
    );
    println!(
        "  +-- Vocab arrays (Vec<u64>, 64 bits/entry): {} ({}k entries x 8 B, Arc-shared x3)",
        fmt_bytes(cltj_vocab),
        vocab_entries / 1000,
    );
    println!(
        "  |    (bit-packed vocab bit-packed vocab would save {}% -> {} but caused 8-22% query regression)",
        vocab_saving_pct_if_10b,
        fmt_bytes(vocab_bytes_compact),
    );
    println!(
        "  +-- String dictionary: {} ({} unique IRIs x 148 B)",
        fmt_bytes(cltj_dict),
        unique_iris
    );
    println!(
        "  +-- Total:                {} ({:.1} B/triple)",
        fmt_bytes(cltj_total),
        cltj_total as f64 / tc as f64
    );
    println!(
        "  +-- LOUDS-only vs Ring index: {}  ({} saved)",
        fmt_savings_pct(s9a_idx, cltj_louds),
        fmt_bytes(s9a_idx.saturating_sub(cltj_louds))
    );
    println!(
        "  `-- Total vs MemoryStore:       {}",
        fmt_savings_pct(mem_total, cltj_total)
    );
    println!();

    // -- Key takeaways --------------------------------------------------------

    let (s9a_idx_100k, s9a_dict_100k) = estimate_ring_wavelet(100_000, 100_000 * 5);
    let s9a_100k = s9a_idx_100k + s9a_dict_100k;
    let (cltj_l_100k, cltj_v_100k, cltj_d_100k) = estimate_cltj(100_000, 100_000 * 5);
    let cltj_100k = cltj_l_100k + cltj_v_100k + cltj_d_100k;
    let (mv, mh) = estimate_memory_store(100_000, 100_000 * 5);
    let mem_100k = mv + mh;

    println!("==================================================================================");
    println!("Key takeaways");
    println!("==================================================================================");
    println!();
    println!("  1. CompactVector L array (CLTJ) saves ~50% vs Vec<u32> (WaveletMatrix Ring).");
    println!(
        "     At {} bits/label (N=10k) vs 32 bits: L shrinks from {} to {}.",
        bits_per_entry,
        fmt_bytes(total_labels * 4),
        fmt_bytes(l_bytes),
    );
    println!("     Vocab stays Arc<Vec<u64>> (64 bits/entry) — bit-packed vocab bit-packed vocab");
    println!(
        "     was reverted: would have saved {}% ({}) but caused 8-22% query regression",
        vocab_saving_pct_if_10b,
        fmt_bytes(cltj_vocab - vocab_bytes_compact),
    );
    println!("     because key() (hottest LFTJ op) became a bit-unpack on every call.");
    println!();
    println!(
        "  2. LOUDS-only ({:.1} B/triple at N=10k) meets the <=20 B/triple target.",
        cltj_louds as f64 / tc as f64
    );
    println!("     On real knowledge graphs (fan-out 100+), internal-node ratio drops from");
    println!("     10.4 labels/triple (synthetic) to ~6.1 labels/triple -> ~12 B/triple.");
    println!();
    println!("  3. CLTJ CLTJ replaces O(log sigma) WaveletMatrix steps with O(1) LOUDS");
    println!("     navigation -- faster queries AND a smaller compact L array.");
    println!();
    println!("  4. At N = 100,000 entities (500,000 triples):");
    println!(
        "     * Ring Ring total  ~= {}  vs  CLTJ CLTJ total ~= {}  ({})",
        fmt_bytes(s9a_100k),
        fmt_bytes(cltj_100k),
        fmt_savings_pct(s9a_100k, cltj_100k)
    );
    println!(
        "     * MemoryStore ~= {}  (CLTJ is {})",
        fmt_bytes(mem_100k),
        fmt_savings_pct(mem_100k, cltj_100k)
    );
    println!();
    println!("Run the speed benchmarks to see query performance side-by-side:");
    println!("  cargo bench -p oxigraph-nova-bench                   # all benchmarks");
    println!("  cargo bench -p oxigraph-nova-bench -- query/triangle # triangle detection only");
    println!("  cargo bench -p oxigraph-nova-bench -- compact        # compact build time");
    println!();
}
