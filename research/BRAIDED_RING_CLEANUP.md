# Braided Ring cleanup summary

**Date:** 2026-07-17
**Status:** Phase 0–2 **executed** on `braided-ring-productize` (diagnostics freeze, path extract, crate split with LOUDS re-export). Phases 3–4 still planned.
**Active status page:** [`BRAIDED_RING.md`](./BRAIDED_RING.md)
**Goal:** stop calling the product path “PRISM,” land the cyclic/braided index as `nova-storage-ring`, isolate the production LOUDS backend as `nova-storage-louds`, and strip research/test ballast.

---

## 1. Why cleanup now

Research waves (E0–E5.11 / F0) mixed three different things in one crate:

| Thing | Reality today | Product fate |
|---|---|---|
| **Six-order LOUDS `RingStore`** | Default production index (`louds`, `ring`, `cltj`, `store`, `delta`, `snapshot`) | Keep as baseline; move to **`nova-storage-louds`** |
| **Cyclic / braided Ring A** | Feature-gated pilot (`cyclic_ring`, `NOVARNG1`, D2 triangle) | Product candidate; **own `nova-storage-ring`** |
| **Research dead ends** | `prism-pilot`, `ultra-pi`, `hybrid-l2`, URing oracles, E1/D3/D4 diagnostics | Archive or delete; not product |

Calling everything “PRISM” or “Ring” is now misleading:

- **PRISM** was a residency / page-touch program + FOR pilot that **failed** as the footprint strategy.
- **Ring** currently means both LOUDS `RingStore` and paper cyclic Ring A.
- The winning triangle primitive is **braided multi-range QWT intersection** (`intersection_next_value3` / D2), not a PRISM accelerator.

**Proposed product name:** **Braided Ring** (implementation crate still `nova-storage-ring`).

---

## 2. Target crate layout

```text
crates/
  nova-storage-common/     # WAL / MANIFEST / dict persistence (unchanged)
  nova-storage-memory/     # MemoryStore (unchanged)
  nova-storage-louds/      # NEW (or rename of current production path)
    louds, GraphRing, CLTJ, RingStore, delta, snapshot
  nova-storage-ring/       # REFOCUS: Braided Ring only
    CyclicRing, MappedRingA, NOVARNG1/NOVAQWT1, RDI/RNV/D2
  nova-store/              # facade: choose backend
```

### 2.1 `nova-storage-louds` (production baseline)

**Move from current `nova-storage-ring`:**

| Module | Notes |
|---|---|
| `louds.rs` | LOUDS trie (~1.2k LOC) |
| `ring.rs` | six-order `GraphRing` |
| `cltj.rs` | CLTJ / LFTJ over LOUDS |
| `store.rs` | `RingStore` (consider rename → `LoudsStore` later) |
| `delta.rs`, `snapshot.rs` | LSM + mmap reopen |
| `fulltext` feature glue | if still LOUDS-store-bound |

**Public identity (near-term):**

- Crate: `oxigraph-nova-storage-louds`
- Keep `RingStore` type alias or re-export for one release if needed
- Docs: “six-order LOUDS CompactLTJ backend”

**Do not carry into louds crate:** `cyclic_ring`, `prism`, `ultra_pi` product wiring (except temporary re-exports during split).

### 2.2 `nova-storage-ring` (Braided Ring product)

**Keep / promote:**

| Module | Product role |
|---|---|
| `cyclic_ring/mod.rs` | `CyclicRing` heap pilot + RDI/RNV |
| `cyclic_ring/mapped_qwt.rs` | mapped QWT + hot path + **D2 only** |
| `cyclic_ring/mapped_ring.rs` | `MappedRingA`, `NOVARNG1` open/write |
| `vendor/qwt` | build-time / algorithm substrate (or path dep) |

**Rename map (docs + modules, not overnight API churn):**

| Old | New |
|---|---|
| PRISM | retire as program name |
| cyclic-ring-pilot | `braided-ring` or just default feature |
| `CyclicRing` | keep or alias `BraidedRing` |
| product triangle | braided D2 (`intersection_next_value3`) |
| research “Ring A” | Braided Ring (single orientation) |

**Cargo description today (stale):**

> CompactLTJ LOUDS trie index…

**After cleanup:**

> Braided Ring: cyclic QWT columns + mmap image + braided multi-range intersection.

### 2.3 What leaves both crates

| Feature / tree | LOC (order) | Action |
|---|---:|---|
| `prism/` + `prism-pilot` | FOR pilot | **Delete or archive** under `research/archive/prism-for/` |
| `ultra_pi` + feature | virtual reverse | **Archive** (E5.8 NO) |
| `hybrid_l2` + feature | Prototype B L2 | **Archive** (not product) |
| URing / condensed B | notes + benches | **Archive only** |
| E1 presence summaries | in `mapped_qwt` | **Remove product API**; keep note of DROP |
| D3 `IntersectionIter3` | diagnostic | **cfg(test) or remove** |
| D4-B/C fused/shared | diagnostic | **cfg(test) or remove** |
| D4-A overlap oracle | useful | keep under `#[cfg(test)]` or `diagnostics` feature |

---

## 3. Test / harness cleanup

### 3.1 In-crate unit tests (keep vs cut)

**Keep (product invariants):**

- LOUDS: build, leap, label, child/degree identity (`louds.rs`)
- LOUDS store: insert/compact/pattern/WAL basics (`store.rs`, `ring.rs`, `cltj.rs`)
- Braided Ring: LF cycle, RNV, RDI symbol identity heap vs mmap
- D2: `intersection_next_value3` == three-RNV dual oracle
- Format: `NOVARNG1` / `NOVAQWT1` open + checksum + access/rank parity

**Move out of default `cargo test` (or delete):**

| Area | Why |
|---|---|
| E1 summary stream vs D2 | DROP branch; only historical |
| D3 iterator vs D2 latency | DROP; correctness optional once |
| D4 fused/shared stream | DROP |
| Ultra-π / hybrid-l2 unit suites | research-only modules |
| PRISM FOR page-touch suite | cancelled strategy |

**Rule:** default CI features = production paths only.

```text
# intended default CI
cargo test -p oxigraph-nova-storage-louds
cargo test -p oxigraph-nova-storage-ring   # braided, no research features

# research (optional job, not default)
cargo test -p oxigraph-nova-storage-ring --features diagnostics
```

### 3.2 Bench binaries (`benches/src/bin`)

**Keep as regression / product gates:**

| Bin | Role after cleanup |
|---|---|
| `e511_ring_perf_profile` | rename → `braided_ring_perf` (fair LOUDS + D2) |
| `e510_mmap_format_gate` | rename → `novarng1_format_gate` |

**Archive under `benches/archive/` or `research/probes/` (do not build in default bench package):**

```text
e47c_*, e48_*, e50_*, e51_*, e54_*, e54b_*, e56_*, e57_*, e58_*,
e59a_*, e59b_*, hybrid_l2_*, ultra_pi_*, virtual_*, leap_bitmap_*,
cross_perm_l_probe, profile_eval*
```

Historical results stay in `research/notes/`; binaries need not stay in the default package graph.

### 3.3 Diagnostic APIs in `mapped_qwt.rs` (~5.5k LOC)

Today product + research share one file. Cleanup target:

```text
mapped_qwt.rs
  ├── product: get/rank/select, RNV, RDI, intersection_next_value2/3
  ├── #[cfg(feature = "diagnostics")] or #[cfg(test)]:
  │     counted, dual_rnv, overlap, fused, shared, summary, IntersectionIter3
  └── unit tests only exercise product + dual_rnv oracle
```

**Product public surface (braided ring):**

```text
access / rank / select
range_next_value
range_distinct_iter
intersection_next_value2
intersection_next_value3   // product triangle
open/write NOVARNG1
```

**Not product surface:**

```text
intersection_next_value3_{counted,overlap,fused,shared,summary}
PresenceSummaryTable
IntersectionIter3          // unless a future multi-common stream needs it
prefetch policy matrix knobs (keep internal default NTA)
c1_batch_k product default 0; matrix only under diagnostics
```

### 3.4 Fair LOUDS helpers in `e511_ring_perf_profile.rs`

After crate split:

- fair LOUDS star/triangle helpers either:
  - live in `nova-storage-louds` as `bench_utils` / `fair_baseline`, or
  - stay in the single product bench crate and depend on both storage crates
- semantic gate (`mismatches=0` shared-alphabet O) stays mandatory for any LOUDS comparison claim

---

## 4. Naming cleanup (docs + code)

| Do | Don’t |
|---|---|
| Braided Ring | PRISM (for this index) |
| `nova-storage-ring` = cyclic QWT product | “Ring” = LOUDS store |
| `nova-storage-louds` = six LOUDS | “LOUDS triangle” without fair cursor |
| D2 / braided intersection | “PRISM triangle” |
| Fair G2/G3 (post-F0) | Legacy ~2.1× G3 claims |
| Research archive | Leave DROP code on default path |

**Doc moves:**

| From | To |
|---|---|
| `research/PRISM_PLAN_UPDATED.md` | `research/archive/prism/PRISM_PLAN_UPDATED.md` (historical) |
| Active plan | new short `research/BRAIDED_RING.md` (product status + gates) |
| `e5.11-ring-performance.md` | keep as campaign note; link from Braided Ring doc |
| E0–E4 notes | `research/archive/` by wave |

Do **not** rewrite history in notes; add a header: “Superseded by Braided Ring product path.”

---

## 5. Suggested execution phases

### Phase 0 — freeze product surface (1 PR, no move) — **DONE**

1. ~~Document Braided Ring lock (this file + short `BRAIDED_RING.md`).~~
2. ~~Mark features deprecated in `Cargo.toml` comments:~~
   - `prism-pilot`, `ultra-pi`, `hybrid-l2` → `deprecated / research only`
3. ~~Gate D3/D4/E1 public APIs behind `diagnostics` or `cfg(test)`.~~
4. ~~Trim `e511` default path: no E1/D3/D4 matrices unless `--full-campaign` + `diagnostics`.~~
5. ~~README note: LOUDS production + Braided Ring pilot.~~

### Phase 1 — extract winners onto clean branch — **DONE** (`braided-ring-productize`)

Path-extract KEEP only (no hybrid/PRISM/ultra) from `main` + research freeze surface.

### Phase 2 — crate split LOUDS vs Braided Ring — **DONE** (compat re-export)

1. ~~`git mv` LOUDS modules into `crates/nova-storage-louds`.~~
2. ~~Workspace member + louds `Cargo.toml` / `lib.rs`.~~
3. ~~`nova-storage-ring` owns Braided Ring + temporary re-export:~~
   ```rust
   // oxigraph_nova_storage_ring (compat)
   pub use oxigraph_nova_storage_louds::*;
   ```
4. ~~Features: `fulltext`/`mmap` forward to louds; `cyclic-ring-pilot`/`diagnostics` stay on ring.~~
5. ~~Green: louds 83 tests, cyclic_ring 39 tests, store 17 tests, server + e511 check.~~

**Remaining Phase-2 polish (optional, not blocking):** drop re-export and migrate dependents to `oxigraph-nova-storage-louds`; rename `cyclic-ring-pilot` → `braided-ring`.

### Phase 3 — delete / archive research code

1. Remove `prism/`, `ultra_pi.rs`, `hybrid_l2.rs` from default tree (or `research/archive/src/`).
2. Strip `cltj.rs` / `louds.rs` cfg branches for those features after extract.
3. Archive e4x/e5x probe bins.
4. CI matrix: louds + braided only.

### Phase 4 — optional productization

1. Braided Ring behind `RingStore`-like facade or parallel store type.
2. SPARQL / LFTJ cutover **only after** integration gates (not part of crate split).
3. G1 polish (star mmap/heap ≤1.10× at N=200k) if cutover requires it.

---

## 6. Current inventory (cleanup inputs)

### Crate bulk (`nova-storage-ring` today)

| File | ~LOC | Destination |
|---|---:|---|
| `store.rs` | 2.8k | louds |
| `cltj.rs` | 2.5k | louds |
| `ultra_pi.rs` | 2.3k | archive |
| `hybrid_l2.rs` | 2.0k | archive |
| `ring.rs` | 1.3k | louds |
| `louds.rs` | 1.2k | louds |
| `cyclic_ring/mod.rs` | 1.3k | braided ring |
| `cyclic_ring/mapped_ring.rs` | 1.1k | braided ring |
| `cyclic_ring/mapped_qwt.rs` | 5.5k | braided ring (**trim diagnostics**) |
| `prism/*` | small pilot | archive |
| `delta` / `snapshot` | ~0.7k | louds (shared patterns → common later) |

### Features

| Feature | Default | Cleanup |
|---|---|---|
| `mmap` | on | keep both crates |
| `fulltext` | off | stay with louds store |
| `cyclic-ring-pilot` | off | promote → braided default |
| `prism-pilot` | off | remove |
| `ultra-pi` | off | remove |
| `hybrid-l2` | off | remove |
| `diagnostics` | **add** | optional research APIs |

### F0 product facts to preserve (do not “clean away”)

- Product triangle = **D2** `intersection_next_value3`
- Fair LOUDS G2 **MET** (~0.76×), fair G3 **MET** (~0.62×) at N=200k
- Space ~**45–47%** vs six LOUDS
- Residual **G1 OPEN** (~1.22× mmap/heap at N=200k)
- DROP locked: C1, D3, D4-B/C, E1, URing, FOR/PRISM codecs

---

## 7. Risks and non-goals

**Risks**

- `RingStore` name collision after split — need aliases / migration note.
- `cltj.rs` is tangled with `ultra-pi` / `hybrid-l2` cfg — extract carefully.
- Benches package lists many bins — archiving must update `benches/Cargo.toml`.
- Docs/README still say LOUDS-only for `storage-ring`.

**Non-goals for this cleanup**

- SPARQL planner changes
- New triangle algorithms
- Rewriting research notes into a single narrative
- Forcing Braided Ring as default store in the same PR as the split

---

## 8. Definition of done

1. Two clear crates: **louds** (production baseline) and **ring** (braided product).
2. No PRISM/FOR/ultra-π/hybrid-l2 on default build graph.
3. Default `cargo test` for both crates is minutes-scale, product-only.
4. One product perf bin with fair LOUDS baselines and D2 semantic gate.
5. Active docs say **Braided Ring**; PRISM plans live under archive.
6. F0 numbers and DROP decisions still citable from `research/notes/e5.11-ring-performance.md`.

---

## 9. Recommended first PR (smallest useful cut)

If execution starts now, do **Phase 0 only**:

1. Add this file + `research/BRAIDED_RING.md` status page.
2. `#[cfg(any(test, feature = "diagnostics"))]` on D3/D4/E1 APIs.
3. Cargo feature comments: research features deprecated.
4. Rename plan note in README: storage-ring heading → “Braided Ring (pilot) + LOUDS store (prod)”.

Crate split (Phases 1–2) is the next mechanical PR after that.
