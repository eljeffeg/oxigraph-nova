# Comparative Benchmark: Nova vs Oxigraph vs QLever vs Fluree vs RDFox

Dataset: 50,000 synthetic BSBM-style entities (1,250,000 triples), identical N-Triples file loaded into all 6 engines.

## Methodology & Storage Model

All 6 engines were benchmarked over the SPARQL 1.1 HTTP Protocol (`curl` to each engine's `/sparql` or query endpoint) using **byte-identical SPARQL query text** against a **byte-identical dataset**. Each query was run with a warm-up pass (discarded) before N timed iterations, so all reported latencies reflect steady-state (not cold-cache) performance.

**Storage model per engine** (this matters — see below):

| Engine | Storage model | Notes |
|---|---|---|
| **Nova (louds)** | Pure in-process heap (`LoudsStore`) | Default production in-memory backend; LOUDS + LFTJ index. |
| **Nova (ring)** | Pure in-process heap (`RingStore` pilot) | Cyclic QWT ring backend (`--backend ring`); mem-only (no WAL/disk yet). |
| **Oxigraph** | Pure in-memory (`serve` run **without** `--location`) | Deliberately run in-memory (not its default RocksDB-backed mode) to match Nova's memory model. |
| **QLever** | Memory-mapped disk index (mmap) | QLever has **no pure in-memory mode**. After warm-up the OS page cache holds the working set resident — consistent with QLever's published methodology. |
| **Fluree** | Ephemeral container FS (`fluree/server`, no host volume) | Default file storage lives inside the container and is destroyed with it — functionally in-memory for this bench. SPARQL is connection-scoped; the harness injects `FROM <ledger>` into each query (addressing only). |
| **RDFox** | In-memory datastore (sandbox/daemon, `parallel-nn`) | Optional: requires a valid `RDFox.lic` (not the evaluation EULA text). Binary defaults to `research/rdfox/RDFox`. |

**Memory usage** is reported as *physical footprint* for Nova/QLever (macOS `vmmap -summary <pid>`'s `Physical footprint:` line — falls back to `ps -o rss` on platforms without `vmmap`, e.g. Linux) and container memory for Oxigraph (`docker stats`). `vmmap`'s physical footprint is used instead of raw `ps -o rss` because on macOS, `ps` RSS includes allocator-retained-but-freed memory (`libmalloc` keeps large freed regions mapped for fast reuse rather than returning them to the OS immediately) and was observed to vary by 10x+ (30-300+ MB) run-to-run for the *identical* process and workload with zero code changes. `vmmap`'s physical footprint is the same figure macOS's Activity Monitor and the kernel's own memory accounting report, and is stable across repeated runs. For QLever, this figure includes memory-mapped index pages resident via the OS page cache — architecturally different from Nova/Oxigraph's pure heap allocations, but it answers the same practical question ("how much RAM does this process hold to serve the workload"), so it is used as the common denominator across engines. This asymmetry is called out explicitly here rather than left implicit.

**CPU usage** is sampled every ~0.3s throughout each engine's query phase (`ps -o %cpu` for Nova/QLever; `docker stats --format '{{.CPUPerc}}'` for Oxigraph) and averaged. Values are percent of one CPU core (e.g. 150% means 1.5 cores kept busy on average) — this is a coarse approximation, not a precise profiler measurement, but useful for relative comparison.

**Process isolation (Nova backends).** Nova (louds) and Nova (ring) are launched as **independent fresh processes** and measured in **separate phases** (start → load → warm-up → timed queries → resource sample → kill), not selected by flipping a backend flag inside one long-running process. Each backend uses its own release binary (`nova_serve` default vs `nova_serve --backend ring` built with `--features ring-backend`). This keeps RSS/CPU samples attributable to a single backend and avoids cross-backend heap or page-cache contamination within the Nova process.

**Latency variability.** Primary latency comparisons use **medians (p50)** (with p95 for tail behavior). Within-process iteration stddev can be material — e.g. Ring `path_2hop` stddev around **66.47 ms** versus about **23.20 ms** for LOUDS on the same query shape — so means alone are easy to over-read. Future optimization runs should keep medians as the headline metric, use enough timed rounds after warm-up, and may add **process-level repetitions** (full restart → load → query phase) on top of within-process query iterations when comparing backends or tracking regressions.


## Dataset Load Time

Wall-clock time to load the identical N-Triples dataset and become ready to serve queries (includes parsing + index construction for all engines; for Nova this is parse + `compact()` into the LOUDS or Ring index, for QLever this is the separate `qlever-index` build step, for Oxigraph this is the HTTP bulk-load POST into the in-memory store).

| Engine | Load time |
|---|---|
| Nova (louds) | 2.18 s |
| Nova (ring) | 2.17 s |
| Oxigraph (in-memory) | 1.98 s |
| QLever (mmap, warmed) | 3.11 s |
| Fluree (ephemeral container) | 5.34 s |
| RDFox (in-memory) | 1.27 s |



## Memory Usage (Physical Footprint)

Nova/QLever figures are macOS `vmmap -summary`'s "Physical footprint" (stable, allocator-retention-immune — see Methodology above); falls back to `ps -o rss` on non-macOS platforms.

| Engine | Memory | Storage model |
|---|---|---|
| Nova (louds) | 103.6 MiB | Pure heap (LOUDS) |
| Nova (ring) | 80.3 MiB | Pure heap (Ring) |
| Oxigraph (in-memory) | 338.5MiB | Pure heap (in-memory mode) |
| QLever (mmap, warmed) | 88.7 MiB | Incl. memory-mapped index pages |
| Fluree (ephemeral container) | 6.294GiB | Ephemeral container FS |
| RDFox (in-memory) | 69.3 MiB | Pure heap (RDFox) |



## CPU Usage (average % of one core during query phase)

| Engine | Avg CPU % |
|---|---|
| Nova (louds) | 51.2% |
| Nova (ring) | 66.2% |
| Oxigraph (in-memory) | 84.6% |
| QLever (mmap, warmed) | 58.8% |
| Fluree (ephemeral container) | 94.0% |
| RDFox (in-memory) | 60.0% |



## Latency Results (milliseconds, HTTP round-trip via curl)

One sub-section per query, with each engine as a column and each percentile (p50, p95) as a row. Charts use p50 latency (lower is better).



### scan

| Metric | Nova (louds) | Nova (ring) | Oxigraph (in-memory) | QLever (mmap, warmed) | Fluree (ephemeral container) | RDFox (in-memory) |
|---|---|---|---|---|---|---|
| p50 (ms) | 52.41 | 43.82 | 45.66 | 96.12 | 116.99 | 19.11 |
| p95 (ms) | 56.11 | 47.62 | 49.54 | 97.58 | 123.95 | 19.41 |



### 2join

| Metric | Nova (louds) | Nova (ring) | Oxigraph (in-memory) | QLever (mmap, warmed) | Fluree (ephemeral container) | RDFox (in-memory) |
|---|---|---|---|---|---|---|
| p50 (ms) | 1.96 | 1.71 | 3.28 | 2.93 | 6.50 | 0.87 |
| p95 (ms) | 2.20 | 1.89 | 3.61 | 3.05 | 7.86 | 0.99 |



### feature_lookup

| Metric | Nova (louds) | Nova (ring) | Oxigraph (in-memory) | QLever (mmap, warmed) | Fluree (ephemeral container) | RDFox (in-memory) |
|---|---|---|---|---|---|---|
| p50 (ms) | 0.84 | 1.11 | 3.52 | 1.29 | 3.17 | 0.55 |
| p95 (ms) | 0.93 | 1.26 | 5.01 | 1.43 | 5.66 | 0.58 |



### star_with_features

| Metric | Nova (louds) | Nova (ring) | Oxigraph (in-memory) | QLever (mmap, warmed) | Fluree (ephemeral container) | RDFox (in-memory) |
|---|---|---|---|---|---|---|
| p50 (ms) | 15.87 | 21.80 | 17.34 | 39.61 | 47.86 | 5.33 |
| p95 (ms) | 17.46 | 24.63 | 18.00 | 44.20 | 51.80 | 5.55 |



### path_2hop

| Metric | Nova (louds) | Nova (ring) | Oxigraph (in-memory) | QLever (mmap, warmed) | Fluree (ephemeral container) | RDFox (in-memory) |
|---|---|---|---|---|---|---|
| p50 (ms) | 515.95 | 636.03 | 505.87 | 1302.68 | 1512.16 | 140.84 |
| p95 (ms) | 537.93 | 653.78 | 512.59 | 1317.21 | 2028.49 | 142.57 |



### triangle

| Metric | Nova (louds) | Nova (ring) | Oxigraph (in-memory) | QLever (mmap, warmed) | Fluree (ephemeral container) | RDFox (in-memory) |
|---|---|---|---|---|---|---|
| p50 (ms) | 316.14 | 635.82 | 341.16 | 436.02 | 648.37 | 83.12 |
| p95 (ms) | 349.08 | 650.21 | 342.85 | 445.51 | 699.44 | 85.78 |



## Raw per-query summary (mean, stddev, n)

One sub-section per query, with each engine as a column and each statistic (n, mean, stddev, min, max) as a row.

### scan

| Metric | Nova (louds) | Nova (ring) | Oxigraph (in-memory) | QLever (mmap, warmed) | Fluree (ephemeral container) | RDFox (in-memory) |
|---|---|---|---|---|---|---|
| n | 10 | 10 | 10 | 10 | 10 | 10 |
| mean (ms) | 52.95 | 44.22 | 46.38 | 96.33 | 117.46 | 18.96 |
| stddev (ms) | 1.94 | 1.98 | 2.05 | 0.85 | 4.10 | 0.45 |
| min (ms) | 50.85 | 41.63 | 44.28 | 95.42 | 112.66 | 18.15 |
| max (ms) | 56.65 | 47.99 | 49.91 | 97.58 | 127.72 | 19.51 |

### 2join

| Metric | Nova (louds) | Nova (ring) | Oxigraph (in-memory) | QLever (mmap, warmed) | Fluree (ephemeral container) | RDFox (in-memory) |
|---|---|---|---|---|---|---|
| n | 10 | 10 | 10 | 10 | 10 | 10 |
| mean (ms) | 1.99 | 1.73 | 3.31 | 2.91 | 6.70 | 0.85 |
| stddev (ms) | 0.13 | 0.10 | 0.19 | 0.12 | 0.71 | 0.11 |
| min (ms) | 1.77 | 1.54 | 3.12 | 2.65 | 6.18 | 0.67 |
| max (ms) | 2.22 | 1.92 | 3.75 | 3.07 | 8.62 | 1.05 |

### feature_lookup

| Metric | Nova (louds) | Nova (ring) | Oxigraph (in-memory) | QLever (mmap, warmed) | Fluree (ephemeral container) | RDFox (in-memory) |
|---|---|---|---|---|---|---|
| n | 10 | 10 | 10 | 10 | 10 | 10 |
| mean (ms) | 0.85 | 1.13 | 3.25 | 1.29 | 3.58 | 0.53 |
| stddev (ms) | 0.07 | 0.10 | 1.37 | 0.09 | 1.15 | 0.05 |
| min (ms) | 0.71 | 0.96 | 1.38 | 1.17 | 2.55 | 0.47 |
| max (ms) | 0.94 | 1.27 | 5.55 | 1.50 | 6.08 | 0.59 |

### star_with_features

| Metric | Nova (louds) | Nova (ring) | Oxigraph (in-memory) | QLever (mmap, warmed) | Fluree (ephemeral container) | RDFox (in-memory) |
|---|---|---|---|---|---|---|
| n | 10 | 10 | 10 | 10 | 10 | 10 |
| mean (ms) | 16.01 | 22.36 | 17.20 | 40.51 | 48.39 | 5.24 |
| stddev (ms) | 0.82 | 1.42 | 0.72 | 2.05 | 2.01 | 0.29 |
| min (ms) | 15.07 | 21.25 | 16.13 | 38.81 | 46.32 | 4.78 |
| max (ms) | 17.77 | 25.90 | 18.12 | 45.30 | 52.90 | 5.60 |

### path_2hop

| Metric | Nova (louds) | Nova (ring) | Oxigraph (in-memory) | QLever (mmap, warmed) | Fluree (ephemeral container) | RDFox (in-memory) |
|---|---|---|---|---|---|---|
| n | 10 | 10 | 10 | 10 | 10 | 10 |
| mean (ms) | 519.24 | 638.93 | 506.25 | 1303.97 | 1617.94 | 140.73 |
| stddev (ms) | 10.96 | 8.48 | 4.90 | 8.23 | 248.02 | 1.42 |
| min (ms) | 509.49 | 627.89 | 497.69 | 1294.71 | 1393.92 | 138.15 |
| max (ms) | 544.98 | 656.85 | 513.23 | 1322.10 | 2163.04 | 142.75 |

### triangle

| Metric | Nova (louds) | Nova (ring) | Oxigraph (in-memory) | QLever (mmap, warmed) | Fluree (ephemeral container) | RDFox (in-memory) |
|---|---|---|---|---|---|---|
| n | 10 | 10 | 10 | 10 | 10 | 10 |
| mean (ms) | 321.58 | 637.32 | 341.01 | 437.85 | 657.99 | 82.99 |
| stddev (ms) | 15.83 | 8.42 | 1.51 | 5.17 | 23.81 | 2.00 |
| min (ms) | 311.27 | 631.70 | 338.81 | 432.72 | 641.06 | 79.95 |
| max (ms) | 363.97 | 660.29 | 343.28 | 448.39 | 717.78 | 86.11 |

