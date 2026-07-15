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
| Nova (Ring+LFTJ) | 0.16 s |
| Oxigraph (in-memory) | 10.85 s |
| QLever (mmap, warmed) | 3.33 s |

## Memory Usage (Physical Footprint)

Nova/QLever figures are macOS `vmmap -summary`'s "Physical footprint" (stable, allocator-retention-immune — see Methodology above); falls back to `ps -o rss` on non-macOS platforms.

| Engine | Memory | Storage model |
|---|---|---|
| Nova (Ring+LFTJ) | 115.3 MiB | Pure heap |
| Oxigraph (in-memory) | 338.9MiB | Pure heap (in-memory mode) |
| QLever (mmap, warmed) | 84.6 MiB | Incl. memory-mapped index pages |

## CPU Usage (average % of one core during query phase)

| Engine | Avg CPU % |
|---|---|
| Nova (Ring+LFTJ) | 14.4% |
| Oxigraph (in-memory) | 22.9% |
| QLever (mmap, warmed) | 28.1% |

## Latency Results (milliseconds, HTTP round-trip via curl)

One sub-section per query, with each engine as a column and each percentile (p50, p95) as a row.

### scan

| Metric | Nova (Ring+LFTJ) | Oxigraph (in-memory) | QLever (mmap, warmed) |
|---|---|---|---|
| p50 (ms) | 49.77 | 48.13 | 95.85 |
| p95 (ms) | 52.19 | 54.59 | 100.66 |

### 2join

| Metric | Nova (Ring+LFTJ) | Oxigraph (in-memory) | QLever (mmap, warmed) |
|---|---|---|---|
| p50 (ms) | 1.79 | 4.13 | 3.79 |
| p95 (ms) | 2.09 | 8.22 | 4.26 |

### feature_lookup

| Metric | Nova (Ring+LFTJ) | Oxigraph (in-memory) | QLever (mmap, warmed) |
|---|---|---|---|
| p50 (ms) | 0.88 | 4.22 | 1.99 |
| p95 (ms) | 0.97 | 5.08 | 2.43 |

### star_with_features

| Metric | Nova (Ring+LFTJ) | Oxigraph (in-memory) | QLever (mmap, warmed) |
|---|---|---|---|
| p50 (ms) | 14.59 | 17.69 | 39.61 |
| p95 (ms) | 15.16 | 19.47 | 41.02 |

### path_2hop

| Metric | Nova (Ring+LFTJ) | Oxigraph (in-memory) | QLever (mmap, warmed) |
|---|---|---|---|
| p50 (ms) | 547.52 | 514.33 | 1271.66 |
| p95 (ms) | 561.05 | 523.22 | 1288.84 |

### triangle

| Metric | Nova (Ring+LFTJ) | Oxigraph (in-memory) | QLever (mmap, warmed) |
|---|---|---|---|
| p50 (ms) | 298.51 | 344.50 | 420.99 |
| p95 (ms) | 304.86 | 352.25 | 426.51 |

## Raw per-query summary (mean, stddev, n)

One sub-section per query, with each engine as a column and each statistic (n, mean, stddev, min, max) as a row.

### scan

| Metric | Nova (Ring+LFTJ) | Oxigraph (in-memory) | QLever (mmap, warmed) |
|---|---|---|---|
| n | 30 | 30 | 30 |
| mean (ms) | 50.10 | 48.76 | 96.87 |
| stddev (ms) | 1.84 | 3.24 | 2.16 |
| min (ms) | 48.13 | 44.28 | 94.21 |
| max (ms) | 58.35 | 58.82 | 101.11 |

### 2join

| Metric | Nova (Ring+LFTJ) | Oxigraph (in-memory) | QLever (mmap, warmed) |
|---|---|---|---|
| n | 30 | 30 | 30 |
| mean (ms) | 1.82 | 5.00 | 3.80 |
| stddev (ms) | 0.13 | 1.71 | 0.25 |
| min (ms) | 1.70 | 3.45 | 3.40 |
| max (ms) | 2.29 | 8.34 | 4.28 |

### feature_lookup

| Metric | Nova (Ring+LFTJ) | Oxigraph (in-memory) | QLever (mmap, warmed) |
|---|---|---|---|
| n | 30 | 30 | 30 |
| mean (ms) | 0.88 | 4.00 | 2.03 |
| stddev (ms) | 0.05 | 0.85 | 0.31 |
| min (ms) | 0.82 | 1.71 | 1.58 |
| max (ms) | 1.03 | 5.10 | 3.01 |

### star_with_features

| Metric | Nova (Ring+LFTJ) | Oxigraph (in-memory) | QLever (mmap, warmed) |
|---|---|---|---|
| n | 30 | 30 | 30 |
| mean (ms) | 14.71 | 17.83 | 39.72 |
| stddev (ms) | 0.32 | 1.16 | 0.66 |
| min (ms) | 14.25 | 16.20 | 38.69 |
| max (ms) | 15.73 | 22.84 | 41.17 |

### path_2hop

| Metric | Nova (Ring+LFTJ) | Oxigraph (in-memory) | QLever (mmap, warmed) |
|---|---|---|---|
| n | 30 | 30 | 30 |
| mean (ms) | 548.18 | 512.74 | 1271.87 |
| stddev (ms) | 7.85 | 8.28 | 10.17 |
| min (ms) | 531.11 | 494.56 | 1256.81 |
| max (ms) | 568.82 | 524.16 | 1297.57 |

### triangle

| Metric | Nova (Ring+LFTJ) | Oxigraph (in-memory) | QLever (mmap, warmed) |
|---|---|---|---|
| n | 30 | 30 | 30 |
| mean (ms) | 298.60 | 346.98 | 421.54 |
| stddev (ms) | 4.68 | 15.71 | 3.19 |
| min (ms) | 289.15 | 336.74 | 415.93 |
| max (ms) | 310.87 | 426.65 | 430.08 |

