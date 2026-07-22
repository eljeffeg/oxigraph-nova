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
| **Fluree** | Ephemeral container FS (`fluree/server`, no host volume) | Default file storage lives inside the container and is destroyed with it — functionally in-memory for this bench. Memory footprint is **dynamic (not measured)**: LeafletCache/import budgets are host-relative and not comparable to pure-heap engines. SPARQL is connection-scoped; the harness injects `FROM <ledger>` into each query (addressing only). |
| **RDFox** | In-memory datastore (sandbox/daemon, `parallel-nn`) | Optional comparator: licensed RDFox binary + `.lic` (auto-skipped when missing; `research/` is gitignored and not required). |

**Memory usage** is reported as *physical footprint* for Nova/QLever (macOS `vmmap -summary <pid>`'s `Physical footprint:` line — falls back to `ps -o rss` on platforms without `vmmap`, e.g. Linux) and container memory for Oxigraph (`docker stats`). `vmmap`'s physical footprint is used instead of raw `ps -o rss` because on macOS, `ps` RSS includes allocator-retained-but-freed memory (`libmalloc` keeps large freed regions mapped for fast reuse rather than returning them to the OS immediately) and was observed to vary by 10x+ (30-300+ MB) run-to-run for the *identical* process and workload with zero code changes. `vmmap`'s physical footprint is the same figure macOS's Activity Monitor and the kernel's own memory accounting report, and is stable across repeated runs. For QLever, this figure includes memory-mapped index pages resident via the OS page cache — architecturally different from Nova/Oxigraph's pure heap allocations, but it answers the same practical question ("how much RAM does this process hold to serve the workload"), so it is used as the common denominator across engines. This asymmetry is called out explicitly here rather than left implicit.

**CPU usage** is sampled every ~0.3s throughout each engine's query phase (`ps -o %cpu` for Nova/QLever; `docker stats --format '{{.CPUPerc}}'` for Oxigraph) and averaged. Values are percent of one CPU core (e.g. 150% means 1.5 cores kept busy on average) — this is a coarse approximation, not a precise profiler measurement, but useful for relative comparison.

**Process isolation (Nova backends).** Nova (louds) and Nova (ring) are launched as **independent fresh processes** and measured in **separate phases** (start → load → warm-up → timed queries → resource sample → kill), not selected by flipping a backend flag inside one long-running process. Each backend uses its own release binary (`nova_serve` default vs `nova_serve --backend ring` built with `--features ring-backend`). This keeps RSS/CPU samples attributable to a single backend and avoids cross-backend heap or page-cache contamination within the Nova process.

**Latency variability.** Primary latency comparisons use **medians (p50)** (with p95 for tail behavior). Within-process iteration stddev can be material — e.g. Ring `path_2hop` stddev around **66.47 ms** versus about **23.20 ms** for LOUDS on the same query shape — so means alone are easy to over-read. Future optimization runs should keep medians as the headline metric, use enough timed rounds after warm-up, and may add **process-level repetitions** (full restart → load → query phase) on top of within-process query iterations when comparing backends or tracking regressions.


## Dataset Load Time

Wall-clock time to load the identical N-Triples dataset and become ready to serve queries (includes parsing + index construction for all engines; for Nova this is parse + `compact()` into the LOUDS or Ring index, for QLever this is the separate `qlever-index` build step, for Oxigraph this is the HTTP bulk-load POST into the in-memory store).

| Engine | Load time |
|---|---|
| Nova (louds) | 2.17 s |
| Nova (ring) | 2.17 s |
| Oxigraph | 2.84 s |
| QLever | 3.35 s |
| Fluree | 5.41 s |
| RDFox | 1.23 s |

![Dataset load time by engine (lower is better)](charts/mem/load_time.svg)

## Memory Usage (Physical Footprint)

Nova/QLever figures are macOS `vmmap -summary`'s "Physical footprint" (stable, allocator-retention-immune — see Methodology above); falls back to `ps -o rss` on non-macOS platforms.

| Engine | Memory | Storage model |
|---|---|---|
| Nova (louds) | 91.4 MiB | Pure heap (LOUDS) |
| Nova (ring) | 74.0 MiB | Pure heap (Ring) |
| Oxigraph | 338.4MiB | Pure heap (in-memory mode) |
| QLever | 89.7 MiB | Incl. memory-mapped index pages |
| Fluree | dynamic (not measured) | Ephemeral container FS; cache/import budgets host-relative |
| RDFox | 69.2 MiB | Pure heap (RDFox) |

![Memory usage by engine (lower is better)](charts/mem/memory.svg)

## CPU Usage (average % of one core during query phase)

| Engine | Avg CPU % |
|---|---|
| Nova (louds) | 42.3% |
| Nova (ring) | 46.0% |
| Oxigraph | 81.5% |
| QLever | 58.0% |
| Fluree | 94.2% |
| RDFox | 58.4% |

![CPU usage by engine (lower is better)](charts/mem/cpu.svg)

## Latency Results (milliseconds, HTTP round-trip via curl)

One sub-section per query, with each engine as a column and each percentile (p50, p95) as a row. Charts use p50 latency (lower is better). `path_2hop` and `triangle` are charted separately — their latencies are orders of magnitude higher and would crush the scale of the other queries.

![p50 latency by query and engine — light queries (lower is better)](charts/mem/latency_p50_overview.svg)

![p50 latency for path_2hop and triangle (lower is better)](charts/mem/latency_p50_heavy.svg)

### scan

| Metric | Nova (louds) | Nova (ring) | Oxigraph | QLever | Fluree | RDFox |
|---|---|---|---|---|---|---|
| p50 (ms) | 43.58 | 36.84 | 45.40 | 92.46 | 111.65 | 14.68 |
| p95 (ms) | 45.18 | 40.06 | 46.19 | 94.88 | 117.40 | 18.34 |

![scan p50 latency (lower is better)](charts/mem/latency_p50_scan.svg)

### 2join

| Metric | Nova (louds) | Nova (ring) | Oxigraph | QLever | Fluree | RDFox |
|---|---|---|---|---|---|---|
| p50 (ms) | 1.41 | 1.27 | 2.51 | 2.46 | 8.37 | 0.55 |
| p95 (ms) | 1.75 | 1.37 | 3.21 | 2.58 | 9.82 | 0.59 |

![2join p50 latency (lower is better)](charts/mem/latency_p50_2join.svg)

### feature_lookup

| Metric | Nova (louds) | Nova (ring) | Oxigraph | QLever | Fluree | RDFox |
|---|---|---|---|---|---|---|
| p50 (ms) | 0.68 | 0.75 | 3.80 | 1.13 | 2.39 | 0.42 |
| p95 (ms) | 0.77 | 0.89 | 4.29 | 1.37 | 4.63 | 0.48 |

![feature_lookup p50 latency (lower is better)](charts/mem/latency_p50_feature_lookup.svg)

### star_with_features

| Metric | Nova (louds) | Nova (ring) | Oxigraph | QLever | Fluree | RDFox |
|---|---|---|---|---|---|---|
| p50 (ms) | 13.71 | 13.57 | 16.81 | 37.44 | 44.44 | 4.49 |
| p95 (ms) | 14.42 | 14.41 | 17.52 | 38.36 | 47.29 | 4.92 |

![star_with_features p50 latency (lower is better)](charts/mem/latency_p50_star_with_features.svg)

### path_2hop

| Metric | Nova (louds) | Nova (ring) | Oxigraph | QLever | Fluree | RDFox |
|---|---|---|---|---|---|---|
| p50 (ms) | 432.82 | 451.41 | 503.39 | 1253.51 | 1430.88 | 137.23 |
| p95 (ms) | 447.30 | 457.08 | 508.79 | 1268.74 | 1938.40 | 139.89 |

![path_2hop p50 latency (lower is better)](charts/mem/latency_p50_path_2hop.svg)

### triangle

| Metric | Nova (louds) | Nova (ring) | Oxigraph | QLever | Fluree | RDFox |
|---|---|---|---|---|---|---|
| p50 (ms) | 210.91 | 208.54 | 334.72 | 423.28 | 641.11 | 73.45 |
| p95 (ms) | 216.92 | 214.50 | 341.19 | 499.75 | 660.76 | 79.00 |

![triangle p50 latency (lower is better)](charts/mem/latency_p50_triangle.svg)

## Raw per-query summary (mean, stddev, n)

One sub-section per query, with each engine as a column and each statistic (n, mean, stddev, min, max) as a row.

### scan

| Metric | Nova (louds) | Nova (ring) | Oxigraph | QLever | Fluree | RDFox |
|---|---|---|---|---|---|---|
| n | 10 | 10 | 10 | 10 | 10 | 10 |
| mean (ms) | 43.64 | 37.25 | 45.36 | 92.87 | 111.58 | 15.23 |
| stddev (ms) | 0.99 | 1.77 | 0.73 | 1.30 | 4.00 | 1.73 |
| min (ms) | 42.26 | 35.26 | 44.00 | 91.54 | 106.73 | 13.66 |
| max (ms) | 45.35 | 41.21 | 46.32 | 94.88 | 119.52 | 18.54 |

### 2join

| Metric | Nova (louds) | Nova (ring) | Oxigraph | QLever | Fluree | RDFox |
|---|---|---|---|---|---|---|
| n | 10 | 10 | 10 | 10 | 10 | 10 |
| mean (ms) | 1.48 | 1.27 | 2.61 | 2.46 | 8.54 | 0.55 |
| stddev (ms) | 0.16 | 0.07 | 0.37 | 0.08 | 0.79 | 0.04 |
| min (ms) | 1.35 | 1.14 | 2.27 | 2.36 | 7.71 | 0.49 |
| max (ms) | 1.90 | 1.41 | 3.33 | 2.64 | 10.51 | 0.59 |

### feature_lookup

| Metric | Nova (louds) | Nova (ring) | Oxigraph | QLever | Fluree | RDFox |
|---|---|---|---|---|---|---|
| n | 10 | 10 | 10 | 10 | 10 | 10 |
| mean (ms) | 0.69 | 0.77 | 3.74 | 1.15 | 3.11 | 0.42 |
| stddev (ms) | 0.05 | 0.08 | 0.45 | 0.14 | 1.15 | 0.05 |
| min (ms) | 0.62 | 0.67 | 3.04 | 0.99 | 1.98 | 0.34 |
| max (ms) | 0.79 | 0.90 | 4.45 | 1.49 | 4.74 | 0.49 |

### star_with_features

| Metric | Nova (louds) | Nova (ring) | Oxigraph | QLever | Fluree | RDFox |
|---|---|---|---|---|---|---|
| n | 10 | 10 | 10 | 10 | 10 | 10 |
| mean (ms) | 13.71 | 13.75 | 16.87 | 37.54 | 44.97 | 4.48 |
| stddev (ms) | 0.53 | 0.36 | 0.49 | 0.51 | 1.47 | 0.28 |
| min (ms) | 12.93 | 13.44 | 16.07 | 36.91 | 43.96 | 4.00 |
| max (ms) | 14.51 | 14.53 | 17.54 | 38.58 | 48.95 | 4.96 |

### path_2hop

| Metric | Nova (louds) | Nova (ring) | Oxigraph | QLever | Fluree | RDFox |
|---|---|---|---|---|---|---|
| n | 10 | 10 | 10 | 10 | 10 | 10 |
| mean (ms) | 435.41 | 448.69 | 502.37 | 1255.17 | 1517.54 | 136.92 |
| stddev (ms) | 7.72 | 7.03 | 5.65 | 7.75 | 241.83 | 2.38 |
| min (ms) | 425.19 | 437.44 | 492.34 | 1246.84 | 1326.57 | 132.87 |
| max (ms) | 453.58 | 457.28 | 509.75 | 1270.82 | 2113.28 | 139.92 |

### triangle

| Metric | Nova (louds) | Nova (ring) | Oxigraph | QLever | Fluree | RDFox |
|---|---|---|---|---|---|---|
| n | 10 | 10 | 10 | 10 | 10 | 10 |
| mean (ms) | 211.14 | 208.78 | 336.14 | 436.97 | 640.16 | 72.73 |
| stddev (ms) | 4.07 | 3.96 | 2.80 | 42.69 | 15.88 | 4.54 |
| min (ms) | 206.47 | 204.08 | 333.92 | 421.30 | 611.00 | 66.99 |
| max (ms) | 217.97 | 216.82 | 341.78 | 558.31 | 666.00 | 79.54 |

