# Comparative Benchmark: Nova vs Oxigraph vs QLever vs Fluree vs RDFox

Dataset: 50,000 synthetic BSBM-style entities (1,250,000 triples), identical N-Triples file loaded into all 6 engines.

## Methodology & Storage Model

All 6 engines were benchmarked over the SPARQL 1.1 HTTP Protocol (`curl` to each engine's `/sparql` or query endpoint) using **byte-identical SPARQL query text** against a **byte-identical dataset**. Each query was run with a warm-up pass (discarded) before N timed iterations, so all reported latencies reflect steady-state (not cold-cache) performance.

**Storage model per engine** (this matters — see below):

| Engine | Storage model | Notes |
|---|---|---|
| **Nova (louds)** | Pure in-process heap (`LoudsStore`) | Default production in-memory backend; LOUDS + LFTJ index. |
| **Nova (ring)** | Pure in-process heap (`RingStore`) | Cyclic QWT ring backend (`--backend ring`); in-memory bulk_load (WAL available via `--location` on disk runs). |
| **Oxigraph** | Pure in-memory (`serve` run **without** `--location`) | Deliberately run in-memory (not its default RocksDB-backed mode) to match Nova's memory model. |
| **QLever** | Memory-mapped disk index (mmap) | QLever has **no pure in-memory mode**. After warm-up the OS page cache holds the working set resident — consistent with QLever's published methodology. |
| **Fluree** | Ephemeral container FS (`fluree/server`, no host volume) | Default file storage lives inside the container and is destroyed with it — functionally in-memory for this bench. SPARQL is connection-scoped; the harness injects `FROM <ledger>` into each query (addressing only). |
| **RDFox** | In-memory datastore (sandbox/daemon, `parallel-nn`) | Optional comparator: licensed RDFox binary + `.lic` (auto-skipped when missing; `research/` is gitignored and not required). |

**Memory usage** is reported as *physical footprint* for Nova/QLever (macOS `vmmap -summary <pid>`'s `Physical footprint:` line — falls back to `ps -o rss` on platforms without `vmmap`, e.g. Linux) and container memory for Oxigraph (`docker stats`). `vmmap`'s physical footprint is used instead of raw `ps -o rss` because on macOS, `ps` RSS includes allocator-retained-but-freed memory (`libmalloc` keeps large freed regions mapped for fast reuse rather than returning them to the OS immediately) and was observed to vary by 10x+ (30-300+ MB) run-to-run for the *identical* process and workload with zero code changes. `vmmap`'s physical footprint is the same figure macOS's Activity Monitor and the kernel's own memory accounting report, and is stable across repeated runs. For QLever, this figure includes memory-mapped index pages resident via the OS page cache — architecturally different from Nova/Oxigraph's pure heap allocations, but it answers the same practical question ("how much RAM does this process hold to serve the workload"), so it is used as the common denominator across engines. This asymmetry is called out explicitly here rather than left implicit.

**CPU usage** is sampled every ~0.3s throughout each engine's query phase (`ps -o %cpu` for Nova/QLever; `docker stats --format '{{.CPUPerc}}'` for Oxigraph) and averaged. Values are percent of one CPU core (e.g. 150% means 1.5 cores kept busy on average) — this is a coarse approximation, not a precise profiler measurement, but useful for relative comparison.

**Process isolation (Nova backends).** Nova (louds) and Nova (ring) are launched as **independent fresh processes** and measured in **separate phases** (start → load → warm-up → timed queries → resource sample → kill), not selected by flipping a backend flag inside one long-running process. Each backend uses its own release binary (`nova_serve` default vs `nova_serve --backend ring` built with `--features ring-backend`). This keeps RSS/CPU samples attributable to a single backend and avoids cross-backend heap or page-cache contamination within the Nova process.

**Latency variability.** Primary latency comparisons use **medians (p50)** (with p95 for tail behavior). Within-process iteration stddev can be material — e.g. Ring `path_2hop` stddev around **66.47 ms** versus about **23.20 ms** for LOUDS on the same query shape — so means alone are easy to over-read. Future optimization runs should keep medians as the headline metric, use enough timed rounds after warm-up, and may add **process-level repetitions** (full restart → load → query phase) on top of within-process query iterations when comparing backends or tracking regressions.


## Dataset Load Time

Wall-clock time to load the identical N-Triples dataset and become ready to serve queries (includes parsing + index construction for all engines; for Nova this is parse + `compact()` into the LOUDS or Ring index, for QLever this is the separate `qlever-index` build step, for Oxigraph this is the HTTP bulk-load POST into the in-memory store).

| Engine | Load time |
|---|---|
| Nova (louds) | 2.19 s |
| Nova (ring) | 2.17 s |
| Oxigraph | 1.98 s |
| QLever | 3.13 s |
| Fluree | 5.26 s |
| RDFox | 1.28 s |



## Memory Usage (Physical Footprint)

Nova/QLever figures are macOS `vmmap -summary`'s "Physical footprint" (stable, allocator-retention-immune — see Methodology above); falls back to `ps -o rss` on non-macOS platforms.

| Engine | Memory | Storage model |
|---|---|---|
| Nova (louds) | 104.1 MiB | Pure heap (LOUDS) |
| Nova (ring) | 81.8 MiB | Pure heap (Ring) |
| Oxigraph | 338.9MiB | Pure heap (in-memory mode) |
| QLever | 97.8 MiB | Incl. memory-mapped index pages |
| Fluree | 6.071GiB | Ephemeral container FS |
| RDFox | 69.3 MiB | Pure heap (RDFox) |



## CPU Usage (average % of one core during query phase)

| Engine | Avg CPU % |
|---|---|
| Nova (louds) | 47.2% |
| Nova (ring) | 57.3% |
| Oxigraph | 80.8% |
| QLever | 60.1% |
| Fluree | 94.0% |
| RDFox | 57.1% |



## Latency Results (milliseconds, HTTP round-trip via curl)

One sub-section per query, with each engine as a column and each percentile (p50, p95) as a row. Charts use p50 latency (lower is better).



### scan

| Metric | Nova (louds) | Nova (ring) | Oxigraph | QLever | Fluree | RDFox |
|---|---|---|---|---|---|---|
| p50 (ms) | 51.64 | 41.56 | 44.73 | 93.32 | 114.49 | 17.01 |
| p95 (ms) | 55.66 | 42.89 | 48.44 | 95.20 | 125.54 | 19.75 |



### 2join

| Metric | Nova (louds) | Nova (ring) | Oxigraph | QLever | Fluree | RDFox |
|---|---|---|---|---|---|---|
| p50 (ms) | 1.49 | 1.38 | 3.42 | 2.73 | 6.64 | 0.53 |
| p95 (ms) | 2.12 | 1.44 | 3.90 | 2.79 | 7.37 | 0.59 |



### feature_lookup

| Metric | Nova (louds) | Nova (ring) | Oxigraph | QLever | Fluree | RDFox |
|---|---|---|---|---|---|---|
| p50 (ms) | 0.63 | 0.84 | 3.44 | 1.13 | 3.12 | 0.41 |
| p95 (ms) | 0.74 | 0.90 | 4.29 | 1.21 | 5.96 | 0.50 |



### star_with_features

| Metric | Nova (louds) | Nova (ring) | Oxigraph | QLever | Fluree | RDFox |
|---|---|---|---|---|---|---|
| p50 (ms) | 15.74 | 19.78 | 17.15 | 38.04 | 43.39 | 4.71 |
| p95 (ms) | 16.94 | 22.95 | 18.26 | 39.59 | 47.69 | 5.28 |



### path_2hop

| Metric | Nova (louds) | Nova (ring) | Oxigraph | QLever | Fluree | RDFox |
|---|---|---|---|---|---|---|
| p50 (ms) | 501.84 | 612.15 | 515.20 | 1279.44 | 1420.72 | 135.21 |
| p95 (ms) | 518.49 | 629.18 | 529.34 | 1303.68 | 1887.31 | 140.53 |



### triangle

| Metric | Nova (louds) | Nova (ring) | Oxigraph | QLever | Fluree | RDFox |
|---|---|---|---|---|---|---|
| p50 (ms) | 237.36 | 349.13 | 338.33 | 425.72 | 624.96 | 70.30 |
| p95 (ms) | 240.36 | 356.60 | 351.57 | 430.87 | 652.26 | 75.02 |



## Raw per-query summary (mean, stddev, n)

One sub-section per query, with each engine as a column and each statistic (n, mean, stddev, min, max) as a row.

### scan

| Metric | Nova (louds) | Nova (ring) | Oxigraph | QLever | Fluree | RDFox |
|---|---|---|---|---|---|---|
| n | 10 | 10 | 10 | 10 | 10 | 10 |
| mean (ms) | 52.44 | 41.60 | 45.37 | 93.61 | 116.29 | 17.22 |
| stddev (ms) | 1.94 | 0.79 | 2.37 | 1.03 | 7.13 | 1.75 |
| min (ms) | 50.45 | 40.53 | 42.22 | 92.09 | 107.76 | 13.99 |
| max (ms) | 56.28 | 43.13 | 49.19 | 95.59 | 125.74 | 20.06 |

### 2join

| Metric | Nova (louds) | Nova (ring) | Oxigraph | QLever | Fluree | RDFox |
|---|---|---|---|---|---|---|
| n | 10 | 10 | 10 | 10 | 10 | 10 |
| mean (ms) | 1.66 | 1.38 | 3.34 | 2.73 | 6.78 | 0.53 |
| stddev (ms) | 0.30 | 0.04 | 0.40 | 0.04 | 0.38 | 0.05 |
| min (ms) | 1.41 | 1.32 | 2.83 | 2.67 | 6.35 | 0.47 |
| max (ms) | 2.19 | 1.45 | 4.14 | 2.82 | 7.58 | 0.59 |

### feature_lookup

| Metric | Nova (louds) | Nova (ring) | Oxigraph | QLever | Fluree | RDFox |
|---|---|---|---|---|---|---|
| n | 10 | 10 | 10 | 10 | 10 | 10 |
| mean (ms) | 0.64 | 0.84 | 3.42 | 1.13 | 4.04 | 0.41 |
| stddev (ms) | 0.06 | 0.05 | 0.68 | 0.06 | 1.57 | 0.06 |
| min (ms) | 0.58 | 0.78 | 2.43 | 1.05 | 2.51 | 0.32 |
| max (ms) | 0.77 | 0.92 | 4.38 | 1.26 | 6.05 | 0.54 |

### star_with_features

| Metric | Nova (louds) | Nova (ring) | Oxigraph | QLever | Fluree | RDFox |
|---|---|---|---|---|---|---|
| n | 10 | 10 | 10 | 10 | 10 | 10 |
| mean (ms) | 15.91 | 20.37 | 17.35 | 38.23 | 44.17 | 4.78 |
| stddev (ms) | 0.60 | 1.44 | 0.52 | 0.90 | 1.92 | 0.33 |
| min (ms) | 15.26 | 19.10 | 16.87 | 37.59 | 42.95 | 4.22 |
| max (ms) | 17.05 | 23.46 | 18.59 | 40.72 | 49.02 | 5.33 |

### path_2hop

| Metric | Nova (louds) | Nova (ring) | Oxigraph | QLever | Fluree | RDFox |
|---|---|---|---|---|---|---|
| n | 10 | 10 | 10 | 10 | 10 | 10 |
| mean (ms) | 502.36 | 612.72 | 517.03 | 1282.32 | 1487.79 | 135.21 |
| stddev (ms) | 10.48 | 9.66 | 8.13 | 12.56 | 247.11 | 3.63 |
| min (ms) | 492.36 | 600.92 | 505.26 | 1270.05 | 1312.12 | 130.60 |
| max (ms) | 527.43 | 633.91 | 536.34 | 1308.18 | 2140.72 | 141.94 |

### triangle

| Metric | Nova (louds) | Nova (ring) | Oxigraph | QLever | Fluree | RDFox |
|---|---|---|---|---|---|---|
| n | 10 | 10 | 10 | 10 | 10 | 10 |
| mean (ms) | 236.82 | 348.07 | 340.02 | 426.47 | 627.29 | 70.65 |
| stddev (ms) | 2.60 | 6.26 | 7.24 | 2.99 | 15.96 | 2.71 |
| min (ms) | 233.16 | 336.08 | 332.12 | 422.30 | 608.44 | 68.06 |
| max (ms) | 240.71 | 357.69 | 356.58 | 431.52 | 663.09 | 77.29 |

