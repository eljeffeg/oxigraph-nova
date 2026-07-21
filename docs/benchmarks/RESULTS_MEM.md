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
| Nova (louds) | 2.16 s |
| Nova (ring) | 2.17 s |
| Oxigraph | 2.03 s |
| QLever | 3.13 s |
| Fluree | 5.42 s |
| RDFox | 1.23 s |



## Memory Usage (Physical Footprint)

Nova/QLever figures are macOS `vmmap -summary`'s "Physical footprint" (stable, allocator-retention-immune — see Methodology above); falls back to `ps -o rss` on non-macOS platforms.

| Engine | Memory | Storage model |
|---|---|---|
| Nova (louds) | 98.8 MiB | Pure heap (LOUDS) |
| Nova (ring) | 81.4 MiB | Pure heap (Ring) |
| Oxigraph | 338.4MiB | Pure heap (in-memory mode) |
| QLever | 90.6 MiB | Incl. memory-mapped index pages |
| Fluree | 5.579GiB | Ephemeral container FS |
| RDFox | 69.2 MiB | Pure heap (RDFox) |



## CPU Usage (average % of one core during query phase)

| Engine | Avg CPU % |
|---|---|
| Nova (louds) | 45.4% |
| Nova (ring) | 54.8% |
| Oxigraph | 91.0% |
| QLever | 58.6% |
| Fluree | 89.7% |
| RDFox | 57.7% |



## Latency Results (milliseconds, HTTP round-trip via curl)

One sub-section per query, with each engine as a column and each percentile (p50, p95) as a row. Charts use p50 latency (lower is better). `path_2hop` and `triangle` are charted separately — their latencies are orders of magnitude higher and would crush the scale of the other queries.





### scan

| Metric | Nova (louds) | Nova (ring) | Oxigraph | QLever | Fluree | RDFox |
|---|---|---|---|---|---|---|
| p50 (ms) | 48.73 | 39.53 | 46.05 | 99.63 | 115.28 | 18.91 |
| p95 (ms) | 50.69 | 43.58 | 51.63 | 112.53 | 121.10 | 19.30 |



### 2join

| Metric | Nova (louds) | Nova (ring) | Oxigraph | QLever | Fluree | RDFox |
|---|---|---|---|---|---|---|
| p50 (ms) | 1.75 | 1.22 | 3.00 | 3.10 | 6.67 | 1.02 |
| p95 (ms) | 1.96 | 1.50 | 3.28 | 3.13 | 7.06 | 1.13 |



### feature_lookup

| Metric | Nova (louds) | Nova (ring) | Oxigraph | QLever | Fluree | RDFox |
|---|---|---|---|---|---|---|
| p50 (ms) | 0.75 | 0.75 | 2.87 | 1.41 | 4.21 | 0.48 |
| p95 (ms) | 0.83 | 0.83 | 4.34 | 1.51 | 6.67 | 0.49 |



### star_with_features

| Metric | Nova (louds) | Nova (ring) | Oxigraph | QLever | Fluree | RDFox |
|---|---|---|---|---|---|---|
| p50 (ms) | 16.04 | 19.35 | 17.17 | 39.98 | 44.44 | 5.12 |
| p95 (ms) | 17.44 | 19.79 | 17.36 | 42.89 | 47.66 | 5.30 |



### path_2hop

| Metric | Nova (louds) | Nova (ring) | Oxigraph | QLever | Fluree | RDFox |
|---|---|---|---|---|---|---|
| p50 (ms) | 458.76 | 590.09 | 515.20 | 1319.26 | 1489.69 | 138.74 |
| p95 (ms) | 478.52 | 609.97 | 563.51 | 1341.84 | 1879.31 | 142.10 |



### triangle

| Metric | Nova (louds) | Nova (ring) | Oxigraph | QLever | Fluree | RDFox |
|---|---|---|---|---|---|---|
| p50 (ms) | 224.78 | 342.26 | 340.01 | 446.42 | 641.76 | 85.27 |
| p95 (ms) | 229.05 | 358.00 | 343.35 | 540.00 | 662.85 | 97.82 |



## Raw per-query summary (mean, stddev, n)

One sub-section per query, with each engine as a column and each statistic (n, mean, stddev, min, max) as a row.

### scan

| Metric | Nova (louds) | Nova (ring) | Oxigraph | QLever | Fluree | RDFox |
|---|---|---|---|---|---|---|
| n | 10 | 10 | 10 | 10 | 10 | 10 |
| mean (ms) | 48.85 | 40.06 | 47.09 | 101.71 | 115.13 | 18.88 |
| stddev (ms) | 1.24 | 2.27 | 2.72 | 6.22 | 3.89 | 0.40 |
| min (ms) | 47.14 | 37.79 | 44.34 | 96.00 | 108.60 | 18.10 |
| max (ms) | 51.83 | 44.96 | 52.86 | 116.20 | 123.30 | 19.33 |

### 2join

| Metric | Nova (louds) | Nova (ring) | Oxigraph | QLever | Fluree | RDFox |
|---|---|---|---|---|---|---|
| n | 10 | 10 | 10 | 10 | 10 | 10 |
| mean (ms) | 1.76 | 1.29 | 2.99 | 3.07 | 6.60 | 1.01 |
| stddev (ms) | 0.14 | 0.14 | 0.24 | 0.08 | 0.36 | 0.09 |
| min (ms) | 1.57 | 1.13 | 2.54 | 2.92 | 5.96 | 0.88 |
| max (ms) | 1.96 | 1.52 | 3.32 | 3.14 | 7.08 | 1.17 |

### feature_lookup

| Metric | Nova (louds) | Nova (ring) | Oxigraph | QLever | Fluree | RDFox |
|---|---|---|---|---|---|---|
| n | 10 | 10 | 10 | 10 | 10 | 10 |
| mean (ms) | 0.75 | 0.73 | 3.09 | 1.41 | 4.30 | 0.48 |
| stddev (ms) | 0.06 | 0.07 | 0.67 | 0.07 | 1.72 | 0.02 |
| min (ms) | 0.63 | 0.63 | 2.60 | 1.30 | 2.15 | 0.45 |
| max (ms) | 0.85 | 0.87 | 4.57 | 1.54 | 7.21 | 0.50 |

### star_with_features

| Metric | Nova (louds) | Nova (ring) | Oxigraph | QLever | Fluree | RDFox |
|---|---|---|---|---|---|---|
| n | 10 | 10 | 10 | 10 | 10 | 10 |
| mean (ms) | 15.99 | 19.29 | 16.99 | 40.71 | 45.26 | 5.11 |
| stddev (ms) | 1.07 | 0.38 | 0.45 | 1.66 | 1.61 | 0.13 |
| min (ms) | 14.73 | 18.64 | 15.96 | 38.91 | 43.90 | 4.86 |
| max (ms) | 17.62 | 19.93 | 17.37 | 42.97 | 47.91 | 5.30 |

### path_2hop

| Metric | Nova (louds) | Nova (ring) | Oxigraph | QLever | Fluree | RDFox |
|---|---|---|---|---|---|---|
| n | 10 | 10 | 10 | 10 | 10 | 10 |
| mean (ms) | 462.85 | 591.02 | 522.74 | 1322.00 | 1536.78 | 139.00 |
| stddev (ms) | 10.83 | 12.65 | 27.08 | 12.10 | 211.27 | 2.05 |
| min (ms) | 451.66 | 576.84 | 506.80 | 1308.83 | 1356.81 | 135.98 |
| max (ms) | 478.80 | 616.98 | 598.46 | 1343.01 | 2050.91 | 142.56 |

### triangle

| Metric | Nova (louds) | Nova (ring) | Oxigraph | QLever | Fluree | RDFox |
|---|---|---|---|---|---|---|
| n | 10 | 10 | 10 | 10 | 10 | 10 |
| mean (ms) | 224.88 | 345.39 | 339.75 | 463.48 | 643.40 | 87.17 |
| stddev (ms) | 3.20 | 8.37 | 3.36 | 47.19 | 12.12 | 6.02 |
| min (ms) | 219.68 | 335.32 | 332.45 | 430.93 | 628.63 | 80.00 |
| max (ms) | 229.59 | 358.53 | 343.76 | 591.28 | 669.78 | 99.74 |

