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
| Nova (louds) | 2.19 s |
| Nova (ring) | 2.18 s |
| Oxigraph | 2.00 s |
| QLever | 3.04 s |
| Fluree | 4.94 s |
| RDFox | 1.29 s |

![Dataset load time by engine (lower is better)](charts/mem/load_time.svg)

## Memory Usage (Physical Footprint)

Nova/QLever figures are macOS `vmmap -summary`'s "Physical footprint" (stable, allocator-retention-immune — see Methodology above); falls back to `ps -o rss` on non-macOS platforms.

| Engine | Memory | Storage model |
|---|---|---|
| Nova (louds) | 91.4 MiB | Pure heap (LOUDS) |
| Nova (ring) | 74.7 MiB | Pure heap (Ring) |
| Oxigraph | 338.2MiB | Pure heap (in-memory mode) |
| QLever | 91.4 MiB | Incl. memory-mapped index pages |
| Fluree | dynamic (not measured) | Ephemeral container FS; cache/import budgets host-relative |
| RDFox | 69.3 MiB | Pure heap (RDFox) |

![Memory usage by engine (lower is better)](charts/mem/memory.svg)

## CPU Usage (average % of one core during query phase)

| Engine | Avg CPU % |
|---|---|
| Nova (louds) | 36.3% |
| Nova (ring) | 43.5% |
| Oxigraph | 81.3% |
| QLever | 60.3% |
| Fluree | 94.0% |
| RDFox | 57.3% |

![CPU usage by engine (lower is better)](charts/mem/cpu.svg)

## Latency Results (milliseconds, HTTP round-trip via curl)

One sub-section per query, with each engine as a column and each percentile (p50, p95) as a row. Charts use p50 latency (lower is better). `path_2hop` and `triangle` are charted separately — their latencies are orders of magnitude higher and would crush the scale of the other queries.

![p50 latency by query and engine — light queries (lower is better)](charts/mem/latency_p50_overview.svg)

![p50 latency for path_2hop and triangle (lower is better)](charts/mem/latency_p50_heavy.svg)

### scan

| Metric | Nova (louds) | Nova (ring) | Oxigraph | QLever | Fluree | RDFox |
|---|---|---|---|---|---|---|
| p50 (ms) | 42.18 | 36.90 | 45.30 | 91.97 | 113.17 | 15.14 |
| p95 (ms) | 44.12 | 38.37 | 46.27 | 95.55 | 120.07 | 18.19 |

![scan p50 latency (lower is better)](charts/mem/latency_p50_scan.svg)

### 2join

| Metric | Nova (louds) | Nova (ring) | Oxigraph | QLever | Fluree | RDFox |
|---|---|---|---|---|---|---|
| p50 (ms) | 1.42 | 1.26 | 2.62 | 2.67 | 7.97 | 0.65 |
| p95 (ms) | 1.61 | 1.40 | 3.23 | 2.84 | 9.05 | 0.81 |

![2join p50 latency (lower is better)](charts/mem/latency_p50_2join.svg)

### feature_lookup

| Metric | Nova (louds) | Nova (ring) | Oxigraph | QLever | Fluree | RDFox |
|---|---|---|---|---|---|---|
| p50 (ms) | 0.68 | 0.78 | 3.62 | 1.21 | 3.39 | 0.43 |
| p95 (ms) | 0.83 | 0.99 | 4.77 | 1.24 | 6.82 | 0.67 |

![feature_lookup p50 latency (lower is better)](charts/mem/latency_p50_feature_lookup.svg)

### star_with_features

| Metric | Nova (louds) | Nova (ring) | Oxigraph | QLever | Fluree | RDFox |
|---|---|---|---|---|---|---|
| p50 (ms) | 12.97 | 13.74 | 17.29 | 37.29 | 44.79 | 4.52 |
| p95 (ms) | 13.30 | 14.77 | 17.83 | 38.14 | 48.95 | 4.90 |

![star_with_features p50 latency (lower is better)](charts/mem/latency_p50_star_with_features.svg)

### path_2hop

| Metric | Nova (louds) | Nova (ring) | Oxigraph | QLever | Fluree | RDFox |
|---|---|---|---|---|---|---|
| p50 (ms) | 428.32 | 446.53 | 514.24 | 1250.13 | 1307.03 | 138.01 |
| p95 (ms) | 437.83 | 452.75 | 523.70 | 1262.30 | 2100.18 | 146.46 |

![path_2hop p50 latency (lower is better)](charts/mem/latency_p50_path_2hop.svg)

### triangle

| Metric | Nova (louds) | Nova (ring) | Oxigraph | QLever | Fluree | RDFox |
|---|---|---|---|---|---|---|
| p50 (ms) | 180.32 | 207.27 | 337.43 | 418.69 | 619.69 | 70.18 |
| p95 (ms) | 185.20 | 210.30 | 339.71 | 421.44 | 641.84 | 74.45 |

![triangle p50 latency (lower is better)](charts/mem/latency_p50_triangle.svg)

## Raw per-query summary (mean, stddev, n)

One sub-section per query, with each engine as a column and each statistic (n, mean, stddev, min, max) as a row.

### scan

| Metric | Nova (louds) | Nova (ring) | Oxigraph | QLever | Fluree | RDFox |
|---|---|---|---|---|---|---|
| n | 10 | 10 | 10 | 10 | 10 | 10 |
| mean (ms) | 42.54 | 37.24 | 45.25 | 92.54 | 114.38 | 15.52 |
| stddev (ms) | 0.98 | 0.86 | 0.98 | 1.85 | 3.70 | 2.30 |
| min (ms) | 41.97 | 36.36 | 43.01 | 90.73 | 109.38 | 12.94 |
| max (ms) | 45.25 | 38.49 | 46.29 | 97.23 | 120.71 | 18.28 |

### 2join

| Metric | Nova (louds) | Nova (ring) | Oxigraph | QLever | Fluree | RDFox |
|---|---|---|---|---|---|---|
| n | 10 | 10 | 10 | 10 | 10 | 10 |
| mean (ms) | 1.45 | 1.28 | 2.69 | 2.70 | 7.95 | 0.65 |
| stddev (ms) | 0.10 | 0.07 | 0.36 | 0.09 | 0.70 | 0.10 |
| min (ms) | 1.38 | 1.20 | 2.28 | 2.61 | 6.97 | 0.54 |
| max (ms) | 1.69 | 1.40 | 3.39 | 2.90 | 9.42 | 0.85 |

### feature_lookup

| Metric | Nova (louds) | Nova (ring) | Oxigraph | QLever | Fluree | RDFox |
|---|---|---|---|---|---|---|
| n | 10 | 10 | 10 | 10 | 10 | 10 |
| mean (ms) | 0.70 | 0.81 | 3.28 | 1.21 | 4.32 | 0.48 |
| stddev (ms) | 0.09 | 0.11 | 1.44 | 0.03 | 1.81 | 0.11 |
| min (ms) | 0.59 | 0.70 | 1.13 | 1.16 | 2.35 | 0.39 |
| max (ms) | 0.92 | 1.06 | 4.92 | 1.24 | 6.84 | 0.67 |

### star_with_features

| Metric | Nova (louds) | Nova (ring) | Oxigraph | QLever | Fluree | RDFox |
|---|---|---|---|---|---|---|
| n | 10 | 10 | 10 | 10 | 10 | 10 |
| mean (ms) | 12.98 | 13.95 | 17.25 | 37.43 | 45.41 | 4.52 |
| stddev (ms) | 0.28 | 0.50 | 0.38 | 0.40 | 2.10 | 0.28 |
| min (ms) | 12.39 | 13.52 | 16.76 | 36.98 | 43.54 | 4.14 |
| max (ms) | 13.31 | 15.12 | 18.04 | 38.17 | 50.29 | 4.92 |

### path_2hop

| Metric | Nova (louds) | Nova (ring) | Oxigraph | QLever | Fluree | RDFox |
|---|---|---|---|---|---|---|
| n | 10 | 10 | 10 | 10 | 10 | 10 |
| mean (ms) | 428.67 | 446.56 | 515.43 | 1252.28 | 1469.40 | 138.42 |
| stddev (ms) | 5.96 | 4.14 | 5.77 | 6.05 | 333.01 | 5.10 |
| min (ms) | 422.74 | 440.50 | 507.06 | 1245.80 | 1275.52 | 129.77 |
| max (ms) | 443.67 | 453.29 | 525.19 | 1262.97 | 2118.16 | 148.66 |

### triangle

| Metric | Nova (louds) | Nova (ring) | Oxigraph | QLever | Fluree | RDFox |
|---|---|---|---|---|---|---|
| n | 10 | 10 | 10 | 10 | 10 | 10 |
| mean (ms) | 181.16 | 207.60 | 337.21 | 418.66 | 622.23 | 70.63 |
| stddev (ms) | 2.51 | 1.55 | 1.98 | 2.21 | 11.99 | 2.12 |
| min (ms) | 178.61 | 205.95 | 333.26 | 413.96 | 609.95 | 68.74 |
| max (ms) | 186.77 | 210.59 | 340.46 | 422.25 | 652.27 | 75.35 |

