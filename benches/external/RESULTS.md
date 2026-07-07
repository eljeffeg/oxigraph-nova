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
| Nova (Ring+LFTJ) | 2.10 s |
| Oxigraph (in-memory) | 2.36 s |
| QLever (mmap, warmed) | 3.14 s |

## Memory Usage (Physical Footprint)

Nova/QLever figures are macOS `vmmap -summary`'s "Physical footprint" (stable, allocator-retention-immune — see Methodology above); falls back to `ps -o rss` on non-macOS platforms.

| Engine | Memory | Storage model |
|---|---|---|
| Nova (Ring+LFTJ) | 199.8 MiB | Pure heap |
| Oxigraph (in-memory) | 338.2MiB | Pure heap (in-memory mode) |
| QLever (mmap, warmed) | 87.6 MiB | Incl. memory-mapped index pages |

## CPU Usage (average % of one core during query phase)

| Engine | Avg CPU % |
|---|---|
| Nova (Ring+LFTJ) | 12.6% |
| Oxigraph (in-memory) | 23.8% |
| QLever (mmap, warmed) | 29.1% |

## Latency Results (milliseconds, HTTP round-trip via curl)

One sub-section per query, with each engine as a column and each percentile (p50, p95) as a row.

### scan

| Metric | Nova (Ring+LFTJ) | Oxigraph (in-memory) | QLever (mmap, warmed) |
|---|---|---|---|
| p50 (ms) | 44.00 | 48.61 | 94.88 |
| p95 (ms) | 45.18 | 59.86 | 96.72 |

### 2join

| Metric | Nova (Ring+LFTJ) | Oxigraph (in-memory) | QLever (mmap, warmed) |
|---|---|---|---|
| p50 (ms) | 1.72 | 7.12 | 2.87 |
| p95 (ms) | 1.96 | 9.94 | 3.09 |

### feature_lookup

| Metric | Nova (Ring+LFTJ) | Oxigraph (in-memory) | QLever (mmap, warmed) |
|---|---|---|---|
| p50 (ms) | 0.73 | 3.14 | 1.34 |
| p95 (ms) | 0.87 | 5.55 | 1.40 |

### star_with_features

| Metric | Nova (Ring+LFTJ) | Oxigraph (in-memory) | QLever (mmap, warmed) |
|---|---|---|---|
| p50 (ms) | 13.44 | 17.53 | 39.17 |
| p95 (ms) | 14.15 | 17.96 | 40.12 |

### path_2hop

| Metric | Nova (Ring+LFTJ) | Oxigraph (in-memory) | QLever (mmap, warmed) |
|---|---|---|---|
| p50 (ms) | 497.68 | 529.50 | 1318.26 |
| p95 (ms) | 503.36 | 539.72 | 1363.25 |

### triangle

| Metric | Nova (Ring+LFTJ) | Oxigraph (in-memory) | QLever (mmap, warmed) |
|---|---|---|---|
| p50 (ms) | 284.64 | 355.80 | 440.03 |
| p95 (ms) | 300.86 | 384.27 | 454.41 |

## Raw per-query summary (mean, stddev, n)

One sub-section per query, with each engine as a column and each statistic (n, mean, stddev, min, max) as a row.

### scan

| Metric | Nova (Ring+LFTJ) | Oxigraph (in-memory) | QLever (mmap, warmed) |
|---|---|---|---|
| n | 30 | 30 | 30 |
| mean (ms) | 44.01 | 50.30 | 95.10 |
| stddev (ms) | 0.78 | 4.73 | 0.83 |
| min (ms) | 42.98 | 45.46 | 94.02 |
| max (ms) | 45.44 | 67.58 | 96.94 |

### 2join

| Metric | Nova (Ring+LFTJ) | Oxigraph (in-memory) | QLever (mmap, warmed) |
|---|---|---|---|
| n | 30 | 30 | 30 |
| mean (ms) | 1.75 | 6.24 | 2.89 |
| stddev (ms) | 0.11 | 2.50 | 0.12 |
| min (ms) | 1.60 | 2.79 | 2.67 |
| max (ms) | 1.96 | 10.05 | 3.19 |

### feature_lookup

| Metric | Nova (Ring+LFTJ) | Oxigraph (in-memory) | QLever (mmap, warmed) |
|---|---|---|---|
| n | 30 | 30 | 30 |
| mean (ms) | 0.72 | 3.29 | 1.33 |
| stddev (ms) | 0.10 | 1.37 | 0.08 |
| min (ms) | 0.57 | 1.51 | 1.20 |
| max (ms) | 0.90 | 7.50 | 1.59 |

### star_with_features

| Metric | Nova (Ring+LFTJ) | Oxigraph (in-memory) | QLever (mmap, warmed) |
|---|---|---|---|
| n | 30 | 30 | 30 |
| mean (ms) | 13.52 | 17.46 | 39.45 |
| stddev (ms) | 0.40 | 0.42 | 1.27 |
| min (ms) | 12.97 | 16.03 | 38.57 |
| max (ms) | 14.99 | 18.04 | 45.81 |

### path_2hop

| Metric | Nova (Ring+LFTJ) | Oxigraph (in-memory) | QLever (mmap, warmed) |
|---|---|---|---|
| n | 30 | 30 | 30 |
| mean (ms) | 498.57 | 529.57 | 1317.67 |
| stddev (ms) | 2.70 | 6.98 | 22.96 |
| min (ms) | 495.00 | 516.79 | 1288.64 |
| max (ms) | 503.92 | 540.62 | 1385.54 |

### triangle

| Metric | Nova (Ring+LFTJ) | Oxigraph (in-memory) | QLever (mmap, warmed) |
|---|---|---|---|
| n | 30 | 30 | 30 |
| mean (ms) | 288.27 | 360.22 | 440.66 |
| stddev (ms) | 7.49 | 10.89 | 7.78 |
| min (ms) | 278.43 | 350.19 | 426.80 |
| max (ms) | 302.15 | 387.53 | 459.00 |

