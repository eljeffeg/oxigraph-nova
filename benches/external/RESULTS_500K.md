# Comparative Benchmark: Nova vs Oxigraph vs QLever

Dataset: 500,000 synthetic BSBM-style entities (12,500,000 triples), identical N-Triples file loaded into all three engines.

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
| Nova (Ring+LFTJ) | 14.40 s |
| Oxigraph (in-memory) | 25.07 s |
| QLever (mmap, warmed) | 31.37 s |

## Memory Usage (Physical Footprint)

Nova/QLever figures are macOS `vmmap -summary`'s "Physical footprint" (stable, allocator-retention-immune — see Methodology above); falls back to `ps -o rss` on non-macOS platforms.

| Engine | Memory | Storage model |
|---|---|---|
| Nova (Ring+LFTJ) | 932.0 MiB | Pure heap |
| Oxigraph (in-memory) | 3.303GiB | Pure heap (in-memory mode) |
| QLever (mmap, warmed) | 579.8 MiB | Incl. memory-mapped index pages |

## CPU Usage (average % of one core during query phase)

| Engine | Avg CPU % |
|---|---|
| Nova (Ring+LFTJ) | 21.6% |
| Oxigraph (in-memory) | 27.6% |
| QLever (mmap, warmed) | 45.6% |

## Latency Results (milliseconds, HTTP round-trip via curl)

One sub-section per query, with each engine as a column and each percentile (p50, p95) as a row.

### scan

| Metric | Nova (Ring+LFTJ) | Oxigraph (in-memory) | QLever (mmap, warmed) |
|---|---|---|---|
| p50 (ms) | 608.83 | 532.38 | 968.08 |
| p95 (ms) | 626.43 | 564.10 | 1004.51 |

### 2join

| Metric | Nova (Ring+LFTJ) | Oxigraph (in-memory) | QLever (mmap, warmed) |
|---|---|---|---|
| p50 (ms) | 13.96 | 27.84 | 21.18 |
| p95 (ms) | 14.96 | 29.00 | 21.64 |

### feature_lookup

| Metric | Nova (Ring+LFTJ) | Oxigraph (in-memory) | QLever (mmap, warmed) |
|---|---|---|---|
| p50 (ms) | 3.75 | 6.92 | 6.28 |
| p95 (ms) | 4.20 | 8.82 | 6.64 |

### star_with_features

| Metric | Nova (Ring+LFTJ) | Oxigraph (in-memory) | QLever (mmap, warmed) |
|---|---|---|---|
| p50 (ms) | 134.78 | 167.32 | 390.05 |
| p95 (ms) | 139.01 | 172.01 | 407.41 |

### path_2hop

| Metric | Nova (Ring+LFTJ) | Oxigraph (in-memory) | QLever (mmap, warmed) |
|---|---|---|---|
| p50 (ms) | 5172.93 | 5316.75 | 13039.83 |
| p95 (ms) | 5314.11 | 5691.84 | 13332.19 |

### triangle

| Metric | Nova (Ring+LFTJ) | Oxigraph (in-memory) | QLever (mmap, warmed) |
|---|---|---|---|
| p50 (ms) | 2934.40 | 4250.76 | 4365.31 |
| p95 (ms) | 2978.36 | 4994.38 | 4433.92 |

## Raw per-query summary (mean, stddev, n)

One sub-section per query, with each engine as a column and each statistic (n, mean, stddev, min, max) as a row.

### scan

| Metric | Nova (Ring+LFTJ) | Oxigraph (in-memory) | QLever (mmap, warmed) |
|---|---|---|---|
| n | 30 | 30 | 30 |
| mean (ms) | 610.69 | 535.43 | 976.24 |
| stddev (ms) | 17.70 | 18.29 | 31.58 |
| min (ms) | 581.57 | 492.08 | 946.88 |
| max (ms) | 683.29 | 581.25 | 1124.29 |

### 2join

| Metric | Nova (Ring+LFTJ) | Oxigraph (in-memory) | QLever (mmap, warmed) |
|---|---|---|---|
| n | 30 | 30 | 30 |
| mean (ms) | 14.02 | 27.99 | 21.19 |
| stddev (ms) | 0.69 | 0.56 | 0.29 |
| min (ms) | 12.95 | 27.13 | 20.58 |
| max (ms) | 16.51 | 29.58 | 21.91 |

### feature_lookup

| Metric | Nova (Ring+LFTJ) | Oxigraph (in-memory) | QLever (mmap, warmed) |
|---|---|---|---|
| n | 30 | 30 | 30 |
| mean (ms) | 3.78 | 7.09 | 6.27 |
| stddev (ms) | 0.26 | 0.91 | 0.26 |
| min (ms) | 3.02 | 5.68 | 5.71 |
| max (ms) | 4.34 | 9.46 | 6.66 |

### star_with_features

| Metric | Nova (Ring+LFTJ) | Oxigraph (in-memory) | QLever (mmap, warmed) |
|---|---|---|---|
| n | 30 | 30 | 30 |
| mean (ms) | 135.15 | 167.21 | 392.17 |
| stddev (ms) | 2.26 | 3.47 | 10.47 |
| min (ms) | 131.43 | 161.13 | 379.11 |
| max (ms) | 140.08 | 173.97 | 434.64 |

### path_2hop

| Metric | Nova (Ring+LFTJ) | Oxigraph (in-memory) | QLever (mmap, warmed) |
|---|---|---|---|
| n | 30 | 30 | 30 |
| mean (ms) | 5157.82 | 5375.69 | 13057.48 |
| stddev (ms) | 103.35 | 169.66 | 194.58 |
| min (ms) | 5000.05 | 5218.65 | 12766.13 |
| max (ms) | 5379.54 | 5965.76 | 13516.50 |

### triangle

| Metric | Nova (Ring+LFTJ) | Oxigraph (in-memory) | QLever (mmap, warmed) |
|---|---|---|---|
| n | 30 | 30 | 30 |
| mean (ms) | 2932.03 | 4313.04 | 4353.02 |
| stddev (ms) | 36.91 | 498.19 | 61.55 |
| min (ms) | 2866.28 | 3694.57 | 4275.26 |
| max (ms) | 3042.27 | 6050.11 | 4511.45 |

