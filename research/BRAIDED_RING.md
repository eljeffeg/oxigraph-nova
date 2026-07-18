# Braided Ring — product status

**Date:** 2026-07-17  
**Status:** Phase 0–4b done on `braided-ring-productize` (ID facade + `join_scan` TrieIterator seam + canonical-ID image adapter). **Not** the default SPARQL backend.



---

## What it is

**Braided Ring** is the cyclic QWT index pilot:

| Piece | Role |
|---|---|
| `CyclicRing` | Heap pilot (3× QWT256 last-columns + A arrays) |
| `MappedRingA` / `NOVARNG1` | Page-aligned mmap image |
| RDI / RNV | Distinct-symbol scan + range successor |
| **D2** `intersection_next_value3` | **Product triangle** (braided multi-range QWT) |

It is **not** six-order LOUDS `RingStore` (still production SPARQL/LFTJ).  
It is **not** PRISM (FOR / page-touch footprint program — failed).

Feature: `cyclic-ring-pilot` on `oxigraph-nova-storage-ring`.

---

## F0 locks (fair LOUDS baseline, N=200k realistic)

| Gate | Result |
|---|---|
| **G2** star mmap/LOUDS | **MET** ≈ **0.76×** |
| **G3** triangle mmap/LOUDS | **MET** ≈ **0.62×** |
| Space vs six LOUDS | ≈ **45–47%** |
| **G1** star mmap/heap | **OPEN** ≈ **1.22×** residual |
| Product triangle | **D2** only |

Campaign detail: [`notes/e5.11-ring-performance.md`](./notes/e5.11-ring-performance.md).

### DROP locked (do not reopen without new measurement)

C1 short-range batch · D3 iterator · D4-B/C fused/shared · E1 presence summaries · URing · FOR/PRISM codecs.

### KEEP

B1 hot views · B2 `range_counts4` · B5 NTA prefetch · D1 two-range braid · **D2 three-range braid**.

---

## Public product surface (braided)

```text
access / rank / select
range_next_value
range_distinct_iter
intersection_next_value2
intersection_next_value3      // product triangle
intersection_next_value*_dual_rnv   // correctness oracle
open/write NOVARNG1 / NOVAQWT1
```

Research/diagnostic APIs (D3/D4/E1, C1 mutation knobs, counted paths, `IntersectionIter3`) require:

```text
--features cyclic-ring-pilot,diagnostics
```

and, for the e511 matrix harness, CLI `--full-campaign`.

---

## What is **not** product (deprecated research features)

| Feature | Fate |
|---|---|
| `prism-pilot` | Archive — FOR footprint strategy failed |
| `ultra-pi` | Archive — E5.8 NO |
| `hybrid-l2` | Archive — not product L codec |

---

## Next steps (cleanup plan)

See [`BRAIDED_RING_CLEANUP.md`](./BRAIDED_RING_CLEANUP.md):

0. **Done:** diagnostics gating, deprecation comments, status docs  
1. **Done:** extract winners onto clean branch `braided-ring-productize` from `main`  
2. **Done (compat re-export):** split `nova-storage-louds` (production LOUDS) vs `nova-storage-ring` (Braided Ring + temporary re-export of LOUDS so dependents stay green)  
3. **Done (research ballast):** archive e4x/e5x probe bins under `benches/archive/`; gate Ring B builders + URing behind `#[cfg(any(test, feature = "diagnostics"))]`; docs point LOUDS paths at `storage-louds`  
4. **Done (ID facade + diffs):** `cyclic_ring::facade::BraidedRingIndex` (heap + optional `NOVARNG1` mmap); differentials for enumerate / lead-range / D2 vs dual_rnv + sorted-list oracle. **Not** `QuadStore` / SPARQL.  
4b. **Done (ID LFTJ seam + image adapter):** `BraidedRingIndex::join_scan` → `TrieIterator` (flat target-field scan, LFTJ contract); `BraidedGraphImage` / `IdRemap` for external↔dense remap, dedup, mmap materialize, external SPO round-trip; multi-scan leapfrog + D2 product-path differentials. Still **not** `QuadStore` / SPARQL / dictionary / live delta.

**Still open before SPARQL cutover:** term dictionary + live delta + full `QuadStore` / store-level LFTJ wiring; G1 polish if required; upstream qwt patches in parallel.

**Do not** wire Braided Ring into default query until those integration gates are green.  
**Do not** redesign StarView until a measurement microbench with hard KEEP/DROP gates.


---

## Perf harness

```bash
# Product gates (fair LOUDS + D1/D2):
cargo run -p oxigraph-nova-bench --release --features cyclic-ring-pilot \
  --bin e511_ring_perf_profile -- 200000 realistic

# Full historical campaign matrices:
cargo run -p oxigraph-nova-bench --release \
  --features cyclic-ring-pilot,diagnostics \
  --bin e511_ring_perf_profile -- 200000 realistic --full-campaign
```
