# Ring memory/CPU regression card (Phase 0)

**Date:** 2026-07-20  
**Harness:** `cargo run -p oxigraph-nova-bench --release --features cyclic-ring-pilot|ring-huffman-cp --bin ring_regression_card -- 20000 realistic`  
**Corpus:** N=20k realistic → **560,000 triples**  
**Slice approved:** Memory-first (Phase 0 → 1A → 1D → 1B → 1C)

## RESULTS_MEM process baseline (pre-change reference)

From `benches/external/RESULTS_MEM.md` (N=50k / 1.25M triples HTTP):

| Engine | Physical footprint | path_2hop p50 | triangle p50 | CPU % |
|--------|-------------------|---------------|--------------|-------|
| Nova (louds) | 103.6 MiB | 566 ms | 314 ms | 55.5% |
| Nova (ring) | 84.3 MiB | 1076 ms | 659 ms | 75.2% |

## Component matrix @ N=20k realistic (560k triples)

### Alphabet sizes

| Layout | ns | np | no | U |
|--------|----|----|----|---|
| role-local | 20 000 | 7 | 82 550 | **102 557** |
| shared-Σ compact (product) | 0 | 0 | 0 | **82 557** |

> **Critical Phase-0 finding:** role-local `U = ns+np+no` is **larger** than product compact `U` on this RDF corpus (S∩O overlap). Role-local is **not** automatically smaller.

### Heap Ring A bytes (complete = QWT + A + 136 shell)

| Config | C_o | C_p | C_s | A_sum | **total** | B/tri | vs prior notes |
|--------|-----|-----|-----|-------|-----------|-------|----------------|
| role-local Qwt | 1.356 | 1.206 | 1.206 | 1.174 | **4.941 MiB** | 9.25 | matches E5.7 shared-Σ class |
| shared-Σ Qwt (product) | 1.356 | **0.452** | 1.356 | 0.945 | **4.110 MiB** | 7.70 | **already ≤ G-mem-A 4.1** |
| shared-Σ Huff C_p | 1.356 | **0.173** | 1.356 | 0.945 | **3.830 MiB** | 7.17 | −0.28 MiB vs Qwt (−6.8%) |
| role-local Huff C_p | 1.356 | 0.173 | 1.206 | 1.174 | **3.909 MiB** | 7.32 | still > shared Huff |

### Dual residency (M3) — measured

| Mode | heap resident | mmap image | accounted (heap+mmap) |
|------|---------------|------------|------------------------|
| KEEP_HEAP (pre-1A default behavior) | 4.110 MiB | 4.118 MiB | **8.228 MiB** |
| DROP heap (Phase 1A default) | 0 | 4.118 MiB | **4.118 MiB** |
| DROP + Huff C_p | 0 | 3.841 MiB | **3.841 MiB** |

**M3 confirmed:** dual residency ≈ **2×** Ring A in the component account (~+4.1 MiB at N=20k). Phase 1A reclaim ≈ full heap payload.

`BraidedGraphImage::materialize_mapped` after 1A: `has_heap=false`, `has_mapped=true`.

### Gates vs targets

| Gate | Target | Status @ N=20k |
|------|--------|----------------|
| G-mem-A | ≤ ~4.1 MiB Huff / stretch 3.5 | **MET** at 3.83 MiB shared Huff; stretch 3.5 still open |
| G-mem-B | ≥ 55% vs 6× LOUDS | not re-run LOUDS 6× here; shared Qwt 7.70 B/tri is strong |
| G-mem-C | no dual-copy tax | **MET** after 1A (mmap-only product path) |
| G-correct | LF/enum/RDI parity | **81/81** lib tests pass |

## Root-cause update (post Phase 0)

| ID | Prior estimate | Measured | Action |
|----|----------------|----------|--------|
| **M3** dual residency | ~2× | **+4.11 MiB** accounted @ 20k | **1A SHIPPED** |
| **M1** fat A on U | +0.8 MiB vs role-seg | A_sum 0.945 MiB shared; role-local A is *worse* (1.174) | 1C still useful as segment/bitvector, not “role-local first” |
| **M2** fat C_p | +1.0 MiB | shared Qwt C_p already 0.452; Huff → 0.173 (−0.28) | **1D still +ROI**; smaller than notes’ 1.26→0.18 |
| **M4** product loses roles | blocks M1/M2 | Product compact U **beats** role-local U | **1B DEFER** — do not switch product alphabet to role-local on this corpus |
| **M5** Huff opt-in | easy −21% | −6.8% on product shared (C_p already small) | enable behind feature; optional default-on after lat gate |

## Phase 1A implementation (shipped this slice)

- `BraidedRingIndex.heap: Option<CyclicRing>` + cached `n`/`universe`
- `materialize_mapped` → drop heap unless `NOVA_RING_KEEP_HEAP=1`
- `materialize_mapped_ex(path, keep_heap)` for tests (no env races)
- `RingRef` / `ring_nav.rs` — mmap-only nav for MiddleRuns / match / VEO
- `RingMemBreakdown` on `CyclicRing`
- Product `BraidedGraphImage` compact path drops heap after mmap

## Revised sequence (post card)

1. ~~Phase 0~~ **done**
2. ~~1A dual-residency drop~~ **done**
3. ~~**1D** Huff C_p product default~~ **done** (`ring-backend` ⇒ `ring-huffman-cp`; A/B via `ring-backend-qwt`)
4. **1B role-preserving alphabet — DEFER / NO-GO for product default** on realistic N=20k (increases U and total bytes)
5. **1C** compact A: prefer paper bitvector / Elias-Fano on **shared** U (not role-segment depending on 1B); target stretch ≤ 3.5 MiB
6. CPU 2A/2B only after memory gates stable

## K11 / RESULTS_MEM latency (not re-measured this run)

Phase 0 counter probe deferred; live code has `get_or_prepare_wedge` / K11 cache. Next CPU slice should snapshot `SPARQL_PATH` under `nova_serve --backend ring` triangle HTTP.



## Phase 1D — Huffman C_p product default (shipped)

**Policy:** `nova-server` feature `ring-backend` now enables `ring-huffman-cp`.
A/B plain QWT: `ring-backend-qwt`. Harness default Huffman; `NOVA_RING_HUFFMAN=0` for Qwt A/B.

### Post-1D @ N=20k realistic (product shared-Σ + DROP heap)

| Config | mmap-only accounted | B/tri |
|--------|---------------------|-------|
| Qwt C_p (pre-1D default) | 4.118 MiB | 7.70 |
| **Huff C_p (1D default)** | **3.841 MiB** | **7.17** |

Δ vs Qwt: **−0.28 MiB (−6.8%)** on Ring A; C_p 0.452 → 0.173 MiB.

### Fair LOUDS (e511, Huff on via cyclic-ring-pilot)

| Gate | Result | Status |
|------|--------|--------|
| G1 star mmap/heap | 0.991× | MET |
| G2 star mmap/LOUDS | **0.613×** | MET — beats LOUDS |
| G3 tri mmap/fair LOUDS | **0.608×** | MET — beats LOUDS |
| Semantic mismatches | 0 | PASS |
| D1/D2 KEEP | +83–85% | KEEP |

## Fair LOUDS gates (e511_ring_perf_profile @ N=20k realistic) — post-1A

Harness: `cargo run -p oxigraph-nova-bench --release --features cyclic-ring-pilot --bin e511_ring_perf_profile -- 20000 realistic`

| Gate | Target | Result | Status |
|------|--------|--------|--------|
| G1 star mmap/heap | ≤ 1.10× | **1.049×** | **MET** |
| G2 star mmap/LOUDS | ≤ 1.50× | **0.586×** | **MET — beats LOUDS** |
| G3 tri mmap/fair LOUDS | ≤ 1.75× | **0.609×** | **MET — beats LOUDS** |
| Triangle semantic | 0 mismatches | **0** (64/64 hits match) | **PASS** |
| RDI heap↔mmap counters | identity | **MATCH** | **PASS** |
| D1 braid vs dual_rnv | KEEP ≥15% | +84.7% first / +82.5% stream | **KEEP** |
| D2 braid vs dual_rnv | KEEP ≥15% | +83.1% first / +79.6% stream | **KEEP** |

Star median ms: heap=0.093  mmap=0.098  **louds=0.167**  
Triangle median ms: heap=0.706  mmap=0.117  **fair_louds=0.192**

**Conclusion:** After Phase 1A (mmap-only product residency), fair microbenches still **beat** LOUDS on both star and triangle kernels with full semantic parity. Product HTTP residual (RESULTS_MEM path/triangle) remains an orchestration issue (Phase 2), not a QWT/D2 substrate regression.

## Repro

```bash
cargo test -p oxigraph-nova-storage-ring --features cyclic-ring-pilot --lib
cargo run -p oxigraph-nova-bench --release --features cyclic-ring-pilot   --bin ring_regression_card -- 20000 realistic
cargo run -p oxigraph-nova-bench --release --features ring-huffman-cp   --bin ring_regression_card -- 20000 realistic
```
