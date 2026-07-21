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
| Nova (ring) | 2.16 s |
| Oxigraph | 1.98 s |
| QLever | 3.04 s |
| Fluree | 4.95 s |
| RDFox | 1.26 s |



## Memory Usage (Physical Footprint)

Nova/QLever figures are macOS `vmmap -summary`'s "Physical footprint" (stable, allocator-retention-immune — see Methodology above); falls back to `ps -o rss` on non-macOS platforms.

| Engine | Memory | Storage model |
|---|---|---|
| Nova (louds) | 91.3 MiB | Pure heap (LOUDS) |
| Nova (ring) | 74.1 MiB | Pure heap (Ring) |
| Oxigraph | 338.6MiB | Pure heap (in-memory mode) |
| QLever | 85.3 MiB | Incl. memory-mapped index pages |
| Fluree | 5.497GiB | Ephemeral container FS |
| RDFox | 69.3 MiB | Pure heap (RDFox) |



## CPU Usage (average % of one core during query phase)

| Engine | Avg CPU % |
|---|---|
| Nova (louds) | 43.6% |
| Nova (ring) | 51.0% |
| Oxigraph | 81.5% |
| QLever | 60.4% |
| Fluree | 88.0% |
| RDFox | 57.6% |



## Latency Results (milliseconds, HTTP round-trip via curl)

One sub-section per query, with each engine as a column and each percentile (p50, p95) as a row. Charts use p50 latency (lower is better). `path_2hop` and `triangle` are charted separately — their latencies are orders of magnitude higher and would crush the scale of the other queries.





### scan

| Metric | Nova (louds) | Nova (ring) | Oxigraph | QLever | Fluree | RDFox |
|---|---|---|---|---|---|---|
| p50 (ms) | 44.34 | 37.08 | 45.38 | 93.02 | 111.07 | 15.74 |
| p95 (ms) | 46.41 | 40.05 | 46.16 | 94.55 | 119.66 | 18.55 |



### 2join

| Metric | Nova (louds) | Nova (ring) | Oxigraph | QLever | Fluree | RDFox |
|---|---|---|---|---|---|---|
| p50 (ms) | 1.47 | 1.32 | 2.41 | 2.70 | 6.34 | 0.56 |
| p95 (ms) | 1.51 | 1.72 | 3.56 | 2.72 | 7.22 | 0.79 |



### feature_lookup

| Metric | Nova (louds) | Nova (ring) | Oxigraph | QLever | Fluree | RDFox |
|---|---|---|---|---|---|---|
| p50 (ms) | 0.67 | 0.72 | 2.45 | 1.20 | 4.09 | 0.36 |
| p95 (ms) | 0.75 | 0.79 | 4.04 | 1.27 | 5.53 | 0.73 |



### star_with_features

| Metric | Nova (louds) | Nova (ring) | Oxigraph | QLever | Fluree | RDFox |
|---|---|---|---|---|---|---|
| p50 (ms) | 13.28 | 14.60 | 16.56 | 37.85 | 44.08 | 4.81 |
| p95 (ms) | 14.58 | 15.72 | 17.40 | 38.75 | 46.16 | 5.25 |



### path_2hop

| Metric | Nova (louds) | Nova (ring) | Oxigraph | QLever | Fluree | RDFox |
|---|---|---|---|---|---|---|
| p50 (ms) | 455.30 | 511.96 | 510.93 | 1261.42 | 1413.05 | 138.04 |
| p95 (ms) | 461.65 | 519.65 | 557.31 | 1267.30 | 1888.28 | 141.21 |



### triangle

| Metric | Nova (louds) | Nova (ring) | Oxigraph | QLever | Fluree | RDFox |
|---|---|---|---|---|---|---|
| p50 (ms) | 222.28 | 237.54 | 339.40 | 421.72 | 630.00 | 72.53 |
| p95 (ms) | 232.61 | 240.60 | 343.74 | 497.54 | 652.38 | 76.98 |



## Raw per-query summary (mean, stddev, n)

One sub-section per query, with each engine as a column and each statistic (n, mean, stddev, min, max) as a row.

### scan

| Metric | Nova (louds) | Nova (ring) | Oxigraph | QLever | Fluree | RDFox |
|---|---|---|---|---|---|---|
| n | 10 | 10 | 10 | 10 | 10 | 10 |
| mean (ms) | 44.62 | 37.27 | 45.24 | 93.07 | 112.12 | 15.98 |
| stddev (ms) | 1.14 | 1.72 | 0.65 | 1.01 | 4.78 | 1.83 |
| min (ms) | 43.02 | 35.42 | 44.38 | 91.76 | 107.34 | 13.44 |
| max (ms) | 46.65 | 41.18 | 46.41 | 94.92 | 123.79 | 19.11 |

### 2join

| Metric | Nova (louds) | Nova (ring) | Oxigraph | QLever | Fluree | RDFox |
|---|---|---|---|---|---|---|
| n | 10 | 10 | 10 | 10 | 10 | 10 |
| mean (ms) | 1.46 | 1.39 | 2.61 | 2.68 | 6.50 | 0.60 |
| stddev (ms) | 0.05 | 0.20 | 0.54 | 0.06 | 0.45 | 0.11 |
| min (ms) | 1.35 | 1.20 | 2.14 | 2.54 | 5.83 | 0.50 |
| max (ms) | 1.53 | 1.88 | 3.87 | 2.73 | 7.51 | 0.82 |

### feature_lookup

| Metric | Nova (louds) | Nova (ring) | Oxigraph | QLever | Fluree | RDFox |
|---|---|---|---|---|---|---|
| n | 10 | 10 | 10 | 10 | 10 | 10 |
| mean (ms) | 0.67 | 0.72 | 2.43 | 1.19 | 4.05 | 0.45 |
| stddev (ms) | 0.05 | 0.05 | 1.16 | 0.07 | 1.28 | 0.16 |
| min (ms) | 0.60 | 0.64 | 1.02 | 1.05 | 2.32 | 0.33 |
| max (ms) | 0.78 | 0.81 | 4.24 | 1.28 | 5.64 | 0.80 |

### star_with_features

| Metric | Nova (louds) | Nova (ring) | Oxigraph | QLever | Fluree | RDFox |
|---|---|---|---|---|---|---|
| n | 10 | 10 | 10 | 10 | 10 | 10 |
| mean (ms) | 13.48 | 14.76 | 16.69 | 38.00 | 44.44 | 4.79 |
| stddev (ms) | 0.65 | 0.60 | 0.44 | 0.53 | 0.98 | 0.31 |
| min (ms) | 12.98 | 13.99 | 16.36 | 37.25 | 43.50 | 4.32 |
| max (ms) | 15.11 | 16.02 | 17.86 | 38.88 | 46.80 | 5.26 |

### path_2hop

| Metric | Nova (louds) | Nova (ring) | Oxigraph | QLever | Fluree | RDFox |
|---|---|---|---|---|---|---|
| n | 10 | 10 | 10 | 10 | 10 | 10 |
| mean (ms) | 455.19 | 512.60 | 517.71 | 1262.23 | 1475.93 | 138.13 |
| stddev (ms) | 4.63 | 5.32 | 26.28 | 2.95 | 247.12 | 2.21 |
| min (ms) | 448.47 | 506.85 | 503.54 | 1259.53 | 1322.10 | 135.37 |
| max (ms) | 461.77 | 521.64 | 591.77 | 1269.36 | 2148.06 | 142.58 |

### triangle

| Metric | Nova (louds) | Nova (ring) | Oxigraph | QLever | Fluree | RDFox |
|---|---|---|---|---|---|---|
| n | 10 | 10 | 10 | 10 | 10 | 10 |
| mean (ms) | 223.38 | 237.02 | 339.21 | 434.98 | 634.13 | 72.43 |
| stddev (ms) | 5.53 | 2.84 | 3.28 | 40.83 | 11.98 | 2.97 |
| min (ms) | 216.69 | 231.14 | 333.15 | 417.18 | 619.80 | 68.95 |
| max (ms) | 236.05 | 241.69 | 346.11 | 550.56 | 656.80 | 77.39 |

