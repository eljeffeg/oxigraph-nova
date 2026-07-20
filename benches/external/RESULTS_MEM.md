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
| Nova (louds) | 2.16 s |
| Nova (ring) | 2.18 s |
| Oxigraph (in-memory) | 2.84 s |
| QLever (mmap, warmed) | 3.41 s |
| Fluree (ephemeral container) | 6.26 s |
| RDFox (in-memory) | 1.22 s |

![Dataset load time by engine (lower is better)](charts/mem/load_time.svg)

## Memory Usage (Physical Footprint)

Nova/QLever figures are macOS `vmmap -summary`'s "Physical footprint" (stable, allocator-retention-immune — see Methodology above); falls back to `ps -o rss` on non-macOS platforms.

| Engine | Memory | Storage model |
|---|---|---|
| Nova (louds) | 103.6 MiB | Pure heap (LOUDS) |
| Nova (ring) | 84.3 MiB | Pure heap (Ring) |
| Oxigraph (in-memory) | 338.2MiB | Pure heap (in-memory mode) |
| QLever (mmap, warmed) | 95.9 MiB | Incl. memory-mapped index pages |
| Fluree (ephemeral container) | 4.917GiB | Ephemeral container FS |
| RDFox (in-memory) | 69.3 MiB | Pure heap (RDFox) |

![Memory usage by engine (lower is better)](charts/mem/memory.svg)

## CPU Usage (average % of one core during query phase)

| Engine | Avg CPU % |
|---|---|
| Nova (louds) | 55.5% |
| Nova (ring) | 75.2% |
| Oxigraph (in-memory) | 83.5% |
| QLever (mmap, warmed) | 60.0% |
| Fluree (ephemeral container) | 92.5% |
| RDFox (in-memory) | 57.0% |

![CPU usage by engine (lower is better)](charts/mem/cpu.svg)

## Latency Results (milliseconds, HTTP round-trip via curl)

One sub-section per query, with each engine as a column and each percentile (p50, p95) as a row. Charts use p50 latency (lower is better).

![p50 latency by query and engine (lower is better)](charts/mem/latency_p50_overview.svg)

### scan

| Metric | Nova (louds) | Nova (ring) | Oxigraph (in-memory) | QLever (mmap, warmed) | Fluree (ephemeral container) | RDFox (in-memory) |
|---|---|---|---|---|---|---|
| p50 (ms) | 51.91 | 45.12 | 47.19 | 98.47 | 117.92 | 18.64 |
| p95 (ms) | 54.12 | 52.96 | 51.01 | 100.21 | 121.40 | 19.25 |

![scan p50 latency (lower is better)](charts/mem/latency_p50_scan.svg)

### 2join

| Metric | Nova (louds) | Nova (ring) | Oxigraph (in-memory) | QLever (mmap, warmed) | Fluree (ephemeral container) | RDFox (in-memory) |
|---|---|---|---|---|---|---|
| p50 (ms) | 1.74 | 4.02 | 3.41 | 2.86 | 6.35 | 0.85 |
| p95 (ms) | 2.01 | 4.26 | 3.79 | 2.99 | 6.62 | 0.88 |

![2join p50 latency (lower is better)](charts/mem/latency_p50_2join.svg)

### feature_lookup

| Metric | Nova (louds) | Nova (ring) | Oxigraph (in-memory) | QLever (mmap, warmed) | Fluree (ephemeral container) | RDFox (in-memory) |
|---|---|---|---|---|---|---|
| p50 (ms) | 0.74 | 0.93 | 2.69 | 1.27 | 2.39 | 0.55 |
| p95 (ms) | 0.82 | 0.99 | 5.13 | 1.31 | 4.57 | 0.59 |

![feature_lookup p50 latency (lower is better)](charts/mem/latency_p50_feature_lookup.svg)

### star_with_features

| Metric | Nova (louds) | Nova (ring) | Oxigraph (in-memory) | QLever (mmap, warmed) | Fluree (ephemeral container) | RDFox (in-memory) |
|---|---|---|---|---|---|---|
| p50 (ms) | 15.39 | 18.86 | 17.08 | 40.43 | 48.00 | 4.97 |
| p95 (ms) | 16.20 | 19.21 | 17.97 | 40.95 | 49.44 | 5.12 |

![star_with_features p50 latency (lower is better)](charts/mem/latency_p50_star_with_features.svg)

### path_2hop

| Metric | Nova (louds) | Nova (ring) | Oxigraph (in-memory) | QLever (mmap, warmed) | Fluree (ephemeral container) | RDFox (in-memory) |
|---|---|---|---|---|---|---|
| p50 (ms) | 566.17 | 1076.35 | 513.08 | 1320.73 | 1517.89 | 138.10 |
| p95 (ms) | 576.40 | 1092.82 | 533.05 | 1327.96 | 1745.76 | 141.22 |

![path_2hop p50 latency (lower is better)](charts/mem/latency_p50_path_2hop.svg)

### triangle

| Metric | Nova (louds) | Nova (ring) | Oxigraph (in-memory) | QLever (mmap, warmed) | Fluree (ephemeral container) | RDFox (in-memory) |
|---|---|---|---|---|---|---|
| p50 (ms) | 313.52 | 658.72 | 350.80 | 445.15 | 688.72 | 81.48 |
| p95 (ms) | 315.33 | 668.71 | 354.38 | 453.32 | 704.06 | 82.19 |

![triangle p50 latency (lower is better)](charts/mem/latency_p50_triangle.svg)

## Raw per-query summary (mean, stddev, n)

One sub-section per query, with each engine as a column and each statistic (n, mean, stddev, min, max) as a row.

### scan

| Metric | Nova (louds) | Nova (ring) | Oxigraph (in-memory) | QLever (mmap, warmed) | Fluree (ephemeral container) | RDFox (in-memory) |
|---|---|---|---|---|---|---|
| n | 10 | 10 | 10 | 10 | 10 | 10 |
| mean (ms) | 52.27 | 46.27 | 47.74 | 98.68 | 118.38 | 18.62 |
| stddev (ms) | 1.07 | 4.10 | 2.18 | 1.00 | 1.78 | 0.47 |
| min (ms) | 51.32 | 43.27 | 46.03 | 97.65 | 115.94 | 17.90 |
| max (ms) | 54.84 | 57.32 | 53.81 | 100.90 | 121.84 | 19.31 |

### 2join

| Metric | Nova (louds) | Nova (ring) | Oxigraph (in-memory) | QLever (mmap, warmed) | Fluree (ephemeral container) | RDFox (in-memory) |
|---|---|---|---|---|---|---|
| n | 10 | 10 | 10 | 10 | 10 | 10 |
| mean (ms) | 1.78 | 4.07 | 3.47 | 2.87 | 6.35 | 0.84 |
| stddev (ms) | 0.15 | 0.15 | 0.19 | 0.08 | 0.17 | 0.04 |
| min (ms) | 1.60 | 3.81 | 3.26 | 2.76 | 6.14 | 0.76 |
| max (ms) | 2.04 | 4.26 | 3.95 | 3.03 | 6.62 | 0.89 |

### feature_lookup

| Metric | Nova (louds) | Nova (ring) | Oxigraph (in-memory) | QLever (mmap, warmed) | Fluree (ephemeral container) | RDFox (in-memory) |
|---|---|---|---|---|---|---|
| n | 10 | 10 | 10 | 10 | 10 | 10 |
| mean (ms) | 0.73 | 0.92 | 2.93 | 1.26 | 2.80 | 0.54 |
| stddev (ms) | 0.08 | 0.07 | 1.33 | 0.04 | 0.93 | 0.04 |
| min (ms) | 0.62 | 0.79 | 1.47 | 1.19 | 2.23 | 0.47 |
| max (ms) | 0.83 | 0.99 | 5.37 | 1.32 | 4.62 | 0.60 |

### star_with_features

| Metric | Nova (louds) | Nova (ring) | Oxigraph (in-memory) | QLever (mmap, warmed) | Fluree (ephemeral container) | RDFox (in-memory) |
|---|---|---|---|---|---|---|
| n | 10 | 10 | 10 | 10 | 10 | 10 |
| mean (ms) | 15.45 | 18.86 | 17.12 | 40.47 | 48.02 | 4.99 |
| stddev (ms) | 0.47 | 0.30 | 0.62 | 0.34 | 1.09 | 0.11 |
| min (ms) | 14.72 | 18.37 | 15.98 | 40.04 | 46.09 | 4.83 |
| max (ms) | 16.25 | 19.21 | 18.11 | 41.16 | 49.45 | 5.12 |

### path_2hop

| Metric | Nova (louds) | Nova (ring) | Oxigraph (in-memory) | QLever (mmap, warmed) | Fluree (ephemeral container) | RDFox (in-memory) |
|---|---|---|---|---|---|---|
| n | 10 | 10 | 10 | 10 | 10 | 10 |
| mean (ms) | 566.87 | 1074.72 | 515.24 | 1320.99 | 1538.82 | 138.46 |
| stddev (ms) | 6.36 | 14.37 | 11.11 | 4.63 | 122.07 | 1.84 |
| min (ms) | 557.77 | 1053.21 | 500.48 | 1316.19 | 1387.16 | 135.82 |
| max (ms) | 578.29 | 1093.16 | 537.00 | 1330.77 | 1806.07 | 142.00 |

### triangle

| Metric | Nova (louds) | Nova (ring) | Oxigraph (in-memory) | QLever (mmap, warmed) | Fluree (ephemeral container) | RDFox (in-memory) |
|---|---|---|---|---|---|---|
| n | 10 | 10 | 10 | 10 | 10 | 10 |
| mean (ms) | 313.59 | 659.07 | 350.52 | 446.70 | 686.93 | 81.13 |
| stddev (ms) | 1.26 | 6.31 | 3.18 | 4.47 | 13.74 | 1.07 |
| min (ms) | 311.72 | 651.18 | 345.76 | 442.04 | 667.76 | 79.77 |
| max (ms) | 315.71 | 673.98 | 354.80 | 453.77 | 706.84 | 82.22 |

