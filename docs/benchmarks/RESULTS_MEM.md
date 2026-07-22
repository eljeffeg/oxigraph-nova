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
| Nova (louds) | 2.15 s |
| Nova (ring) | 2.17 s |
| Oxigraph | 2.05 s |
| QLever | 3.49 s |
| Fluree | 5.20 s |
| RDFox | 1.28 s |



## Memory Usage (Physical Footprint)

Nova/QLever figures are macOS `vmmap -summary`'s "Physical footprint" (stable, allocator-retention-immune — see Methodology above); falls back to `ps -o rss` on non-macOS platforms.

| Engine | Memory | Storage model |
|---|---|---|
| Nova (louds) | 91.4 MiB | Pure heap (LOUDS) |
| Nova (ring) | 74.1 MiB | Pure heap (Ring) |
| Oxigraph | 338.4MiB | Pure heap (in-memory mode) |
| QLever | 89.9 MiB | Incl. memory-mapped index pages |
| Fluree | 7.873GiB | Ephemeral container FS |
| RDFox | 69.3 MiB | Pure heap (RDFox) |



## CPU Usage (average % of one core during query phase)

| Engine | Avg CPU % |
|---|---|
| Nova (louds) | 42.3% |
| Nova (ring) | 53.4% |
| Oxigraph | 81.8% |
| QLever | 58.9% |
| Fluree | 89.6% |
| RDFox | 59.7% |



## Latency Results (milliseconds, HTTP round-trip via curl)

One sub-section per query, with each engine as a column and each percentile (p50, p95) as a row. Charts use p50 latency (lower is better). `path_2hop` and `triangle` are charted separately — their latencies are orders of magnitude higher and would crush the scale of the other queries.





### scan

| Metric | Nova (louds) | Nova (ring) | Oxigraph | QLever | Fluree | RDFox |
|---|---|---|---|---|---|---|
| p50 (ms) | 47.71 | 36.27 | 45.83 | 94.26 | 111.13 | 18.37 |
| p95 (ms) | 52.85 | 38.02 | 46.67 | 100.45 | 120.64 | 19.40 |



### 2join

| Metric | Nova (louds) | Nova (ring) | Oxigraph | QLever | Fluree | RDFox |
|---|---|---|---|---|---|---|
| p50 (ms) | 1.80 | 1.25 | 2.57 | 2.71 | 7.82 | 0.51 |
| p95 (ms) | 2.04 | 1.47 | 2.82 | 2.87 | 10.39 | 0.56 |



### feature_lookup

| Metric | Nova (louds) | Nova (ring) | Oxigraph | QLever | Fluree | RDFox |
|---|---|---|---|---|---|---|
| p50 (ms) | 0.82 | 0.72 | 3.56 | 1.09 | 3.46 | 0.36 |
| p95 (ms) | 0.90 | 0.76 | 4.80 | 1.16 | 5.61 | 0.46 |



### star_with_features

| Metric | Nova (louds) | Nova (ring) | Oxigraph | QLever | Fluree | RDFox |
|---|---|---|---|---|---|---|
| p50 (ms) | 14.81 | 14.29 | 17.26 | 38.65 | 44.36 | 4.90 |
| p95 (ms) | 15.65 | 14.76 | 18.02 | 40.04 | 46.98 | 9.43 |



### path_2hop

| Metric | Nova (louds) | Nova (ring) | Oxigraph | QLever | Fluree | RDFox |
|---|---|---|---|---|---|---|
| p50 (ms) | 463.12 | 510.31 | 512.52 | 1285.52 | 1447.65 | 137.92 |
| p95 (ms) | 475.19 | 519.40 | 522.28 | 1325.39 | 2587.68 | 147.21 |



### triangle

| Metric | Nova (louds) | Nova (ring) | Oxigraph | QLever | Fluree | RDFox |
|---|---|---|---|---|---|---|
| p50 (ms) | 224.57 | 233.17 | 335.95 | 430.94 | 642.90 | 74.47 |
| p95 (ms) | 233.23 | 237.14 | 341.06 | 502.03 | 650.10 | 83.95 |



## Raw per-query summary (mean, stddev, n)

One sub-section per query, with each engine as a column and each statistic (n, mean, stddev, min, max) as a row.

### scan

| Metric | Nova (louds) | Nova (ring) | Oxigraph | QLever | Fluree | RDFox |
|---|---|---|---|---|---|---|
| n | 10 | 10 | 10 | 10 | 10 | 10 |
| mean (ms) | 47.21 | 36.51 | 45.95 | 94.97 | 112.77 | 18.06 |
| stddev (ms) | 3.88 | 1.00 | 0.51 | 3.38 | 5.28 | 1.26 |
| min (ms) | 43.13 | 35.22 | 45.29 | 92.46 | 106.74 | 15.74 |
| max (ms) | 56.36 | 38.61 | 46.70 | 104.08 | 121.59 | 19.75 |

### 2join

| Metric | Nova (louds) | Nova (ring) | Oxigraph | QLever | Fluree | RDFox |
|---|---|---|---|---|---|---|
| n | 10 | 10 | 10 | 10 | 10 | 10 |
| mean (ms) | 1.85 | 1.30 | 2.56 | 2.70 | 8.26 | 0.51 |
| stddev (ms) | 0.11 | 0.10 | 0.17 | 0.13 | 1.29 | 0.04 |
| min (ms) | 1.72 | 1.21 | 2.32 | 2.47 | 7.02 | 0.46 |
| max (ms) | 2.07 | 1.47 | 2.89 | 2.91 | 11.02 | 0.57 |

### feature_lookup

| Metric | Nova (louds) | Nova (ring) | Oxigraph | QLever | Fluree | RDFox |
|---|---|---|---|---|---|---|
| n | 10 | 10 | 10 | 10 | 10 | 10 |
| mean (ms) | 0.82 | 0.70 | 3.77 | 1.08 | 3.82 | 0.37 |
| stddev (ms) | 0.05 | 0.05 | 0.63 | 0.07 | 1.23 | 0.06 |
| min (ms) | 0.74 | 0.62 | 3.21 | 0.98 | 2.54 | 0.32 |
| max (ms) | 0.91 | 0.76 | 5.17 | 1.17 | 5.81 | 0.53 |

### star_with_features

| Metric | Nova (louds) | Nova (ring) | Oxigraph | QLever | Fluree | RDFox |
|---|---|---|---|---|---|---|
| n | 10 | 10 | 10 | 10 | 10 | 10 |
| mean (ms) | 14.91 | 14.36 | 17.22 | 38.67 | 44.85 | 5.64 |
| stddev (ms) | 0.50 | 0.30 | 0.57 | 0.85 | 1.32 | 2.36 |
| min (ms) | 14.11 | 13.94 | 16.10 | 37.75 | 43.87 | 4.24 |
| max (ms) | 15.98 | 14.80 | 18.15 | 40.44 | 48.31 | 12.20 |

### path_2hop

| Metric | Nova (louds) | Nova (ring) | Oxigraph | QLever | Fluree | RDFox |
|---|---|---|---|---|---|---|
| n | 10 | 10 | 10 | 10 | 10 | 10 |
| mean (ms) | 463.57 | 510.16 | 512.90 | 1288.45 | 1648.97 | 139.81 |
| stddev (ms) | 8.29 | 6.16 | 6.37 | 21.44 | 550.62 | 4.51 |
| min (ms) | 450.83 | 499.81 | 504.76 | 1267.75 | 1349.39 | 134.87 |
| max (ms) | 476.44 | 522.97 | 526.96 | 1341.38 | 3137.28 | 150.00 |

### triangle

| Metric | Nova (louds) | Nova (ring) | Oxigraph | QLever | Fluree | RDFox |
|---|---|---|---|---|---|---|
| n | 10 | 10 | 10 | 10 | 10 | 10 |
| mean (ms) | 225.34 | 233.07 | 335.99 | 442.54 | 641.41 | 75.75 |
| stddev (ms) | 5.63 | 3.07 | 3.39 | 38.71 | 8.90 | 4.86 |
| min (ms) | 217.83 | 229.05 | 331.88 | 420.99 | 621.11 | 70.67 |
| max (ms) | 235.35 | 238.21 | 342.50 | 550.99 | 650.83 | 83.96 |

