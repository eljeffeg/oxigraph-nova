# Comparative Benchmark: Nova vs Oxigraph vs QLever

Dataset: 50,000 synthetic BSBM-style entities (1,250,000 triples), identical N-Triples file loaded into all three engines.

## Methodology & Storage Model

All three engines were benchmarked over the SPARQL 1.1 HTTP Protocol (`curl` to each engine's `/sparql` or query endpoint) using **byte-identical SPARQL query text** against a **byte-identical dataset**. Each query was run with a warm-up pass (discarded) before N timed iterations, so all reported latencies reflect steady-state (not cold-cache) performance.

**Storage model per engine** (this matters — see below):

| Engine | Storage model | Notes |
|---|---|---|
| **Nova** | Pure in-process heap memory | No disk persistence exists at all; the whole dataset + index must fit in RAM. |
| **Oxigraph** | Pure in-memory (`serve` run **without** `--location`) | Deliberately run in-memory (not its default RocksDB-backed mode) to match Nova's memory model — this is an apples-to-apples memory comparison, not Oxigraph's disk-persistent configuration. |
| **QLever** | Memory-mapped disk index (mmap) | QLever has **no pure in-memory mode** — its index format is inherently a set of memory-mapped files. After the warm-up pass, the OS page cache holds the working set resident in RAM, so steady-state latency is effectively RAM-speed. This is consistent with how QLever is used and benchmarked in practice. |

**Memory usage** is reported as *physical footprint* for Nova/QLever (macOS `vmmap -summary <pid>`'s `Physical footprint:` line — falls back to `ps -o rss` on platforms without `vmmap`, e.g. Linux) and container memory for Oxigraph (`docker stats`). `vmmap`'s physical footprint is used instead of raw `ps -o rss` because on macOS, `ps` RSS includes allocator-retained-but-freed memory (`libmalloc` keeps large freed regions mapped for fast reuse rather than returning them to the OS immediately) and was observed to vary by 10x+ (30-300+ MB) run-to-run for the *identical* process and workload with zero code changes. `vmmap`'s physical footprint is the same figure macOS's Activity Monitor and the kernel's own memory accounting report, and is stable across repeated runs. For QLever, this figure includes memory-mapped index pages resident via the OS page cache — architecturally different from Nova/Oxigraph's pure heap allocations, but it answers the same practical question ("how much RAM does this process hold to serve the workload"), so it is used as the common denominator across all three. This asymmetry is called out explicitly here rather than left implicit.

**CPU usage** is sampled every ~0.3s throughout each engine's query phase (`ps -o %cpu` for Nova/QLever; `docker stats --format '{{.CPUPerc}}'` for Oxigraph) and averaged. Values are percent of one CPU core (e.g. 150% means 1.5 cores kept busy on average) — this is a coarse approximation, not a precise profiler measurement, but useful for relative comparison.


## Dataset Load Time

Wall-clock time to load the identical N-Triples dataset and become ready to serve queries (includes parsing + index construction for all engines; for Nova this is parse + `compact()` into the Ring/LFTJ index, for QLever this is the separate `qlever-index` build step, for Oxigraph this is the HTTP bulk-load POST into the in-memory store).

| Engine | Load time |
|---|---|
| Nova (Ring+LFTJ) | 2.23 s |
| Oxigraph (in-memory) | 4.23 s |
| QLever (mmap, warmed) | 3.59 s |

## Memory Usage (Physical Footprint)

Nova/QLever figures are macOS `vmmap -summary`'s "Physical footprint" (stable, allocator-retention-immune — see Methodology above); falls back to `ps -o rss` on non-macOS platforms.

| Engine | Memory | Storage model |
|---|---|---|
| Nova (Ring+LFTJ) | 104.6 MiB | Pure heap |
| Oxigraph (in-memory) | 352.7MiB | Pure heap (in-memory mode) |
| QLever (mmap, warmed) | 82.6 MiB | Incl. memory-mapped index pages |

## CPU Usage (average % of one core during query phase)

| Engine | Avg CPU % |
|---|---|
| Nova (Ring+LFTJ) | 12.6% |
| Oxigraph (in-memory) | 29.0% |
| QLever (mmap, warmed) | 28.7% |

## Latency Results (milliseconds, HTTP round-trip via curl)

One sub-section per query, with each engine as a column and each percentile (p50, p95) as a row.

### scan

| Metric | Nova (Ring+LFTJ) | Oxigraph (in-memory) | QLever (mmap, warmed) |
|---|---|---|---|
| p50 (ms) | 47.45 | 47.20 | 91.74 |
| p95 (ms) | 50.48 | 53.38 | 93.17 |

### 2join

| Metric | Nova (Ring+LFTJ) | Oxigraph (in-memory) | QLever (mmap, warmed) |
|---|---|---|---|
| p50 (ms) | 1.54 | 2.93 | 2.71 |
| p95 (ms) | 1.98 | 6.08 | 2.78 |

### feature_lookup

| Metric | Nova (Ring+LFTJ) | Oxigraph (in-memory) | QLever (mmap, warmed) |
|---|---|---|---|
| p50 (ms) | 0.68 | 4.05 | 1.31 |
| p95 (ms) | 0.87 | 4.83 | 1.49 |

### star_with_features

| Metric | Nova (Ring+LFTJ) | Oxigraph (in-memory) | QLever (mmap, warmed) |
|---|---|---|---|
| p50 (ms) | 14.07 | 21.66 | 37.20 |
| p95 (ms) | 14.86 | 24.30 | 37.62 |

### path_2hop

| Metric | Nova (Ring+LFTJ) | Oxigraph (in-memory) | QLever (mmap, warmed) |
|---|---|---|---|
| p50 (ms) | 525.24 | 705.46 | 1238.30 |
| p95 (ms) | 533.20 | 774.45 | 1295.46 |

### triangle

| Metric | Nova (Ring+LFTJ) | Oxigraph (in-memory) | QLever (mmap, warmed) |
|---|---|---|---|
| p50 (ms) | 289.30 | 414.93 | 413.86 |
| p95 (ms) | 298.99 | 444.17 | 419.74 |

## Raw per-query summary (mean, stddev, n)

One sub-section per query, with each engine as a column and each statistic (n, mean, stddev, min, max) as a row.

### scan

| Metric | Nova (Ring+LFTJ) | Oxigraph (in-memory) | QLever (mmap, warmed) |
|---|---|---|---|
| n | 30 | 30 | 30 |
| mean (ms) | 47.95 | 48.02 | 91.86 |
| stddev (ms) | 1.37 | 3.21 | 0.63 |
| min (ms) | 46.25 | 42.77 | 90.88 |
| max (ms) | 52.58 | 53.57 | 93.60 |

### 2join

| Metric | Nova (Ring+LFTJ) | Oxigraph (in-memory) | QLever (mmap, warmed) |
|---|---|---|---|
| n | 30 | 30 | 30 |
| mean (ms) | 1.60 | 3.55 | 2.70 |
| stddev (ms) | 0.18 | 1.25 | 0.06 |
| min (ms) | 1.44 | 2.34 | 2.52 |
| max (ms) | 2.01 | 6.23 | 2.80 |

### feature_lookup

| Metric | Nova (Ring+LFTJ) | Oxigraph (in-memory) | QLever (mmap, warmed) |
|---|---|---|---|
| n | 30 | 30 | 30 |
| mean (ms) | 0.70 | 4.20 | 1.34 |
| stddev (ms) | 0.10 | 1.23 | 0.32 |
| min (ms) | 0.55 | 3.19 | 1.04 |
| max (ms) | 0.95 | 10.09 | 2.95 |

### star_with_features

| Metric | Nova (Ring+LFTJ) | Oxigraph (in-memory) | QLever (mmap, warmed) |
|---|---|---|---|
| n | 30 | 30 | 30 |
| mean (ms) | 14.12 | 22.07 | 37.20 |
| stddev (ms) | 0.46 | 1.17 | 0.29 |
| min (ms) | 13.47 | 20.79 | 36.62 |
| max (ms) | 15.73 | 24.67 | 37.71 |

### path_2hop

| Metric | Nova (Ring+LFTJ) | Oxigraph (in-memory) | QLever (mmap, warmed) |
|---|---|---|---|
| n | 30 | 30 | 30 |
| mean (ms) | 523.98 | 725.82 | 1247.26 |
| stddev (ms) | 7.33 | 41.61 | 20.48 |
| min (ms) | 506.85 | 670.48 | 1232.27 |
| max (ms) | 540.00 | 785.45 | 1315.15 |

### triangle

| Metric | Nova (Ring+LFTJ) | Oxigraph (in-memory) | QLever (mmap, warmed) |
|---|---|---|---|
| n | 30 | 30 | 30 |
| mean (ms) | 290.08 | 418.26 | 418.17 |
| stddev (ms) | 3.83 | 14.02 | 23.29 |
| min (ms) | 286.81 | 400.15 | 410.69 |
| max (ms) | 303.27 | 445.37 | 540.80 |

