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

**Memory usage** is reported as process RSS (`ps -o rss` for Nova/QLever; `docker stats` for Oxigraph's container). For QLever, RSS includes memory-mapped index pages resident via the OS page cache — architecturally different from Nova/Oxigraph's pure heap allocations, but it answers the same practical question ("how much RAM does this process hold to serve the workload"), so it is used as the common denominator across all three. This asymmetry is called out explicitly here rather than left implicit.

**CPU usage** is sampled every ~0.3s throughout each engine's query phase (`ps -o %cpu` for Nova/QLever; `docker stats --format '{{.CPUPerc}}'` for Oxigraph) and averaged. Values are percent of one CPU core (e.g. 150% means 1.5 cores kept busy on average) — this is a coarse approximation, not a precise profiler measurement, but useful for relative comparison.

## Latency Results (milliseconds, HTTP round-trip via curl)

One sub-section per query, with each engine as a column and each percentile (p50, p95) as a row.

### scan

| Metric | Nova (Ring+LFTJ) | Oxigraph (in-memory) | QLever (mmap, warmed) |
|---|---|---|---|
| p50 (ms) | 552.39 | 529.31 | 955.31 |
| p95 (ms) | 553.85 | 549.56 | 956.93 |

### 2join

| Metric | Nova (Ring+LFTJ) | Oxigraph (in-memory) | QLever (mmap, warmed) |
|---|---|---|---|
| p50 (ms) | 15.39 | 26.43 | 20.79 |
| p95 (ms) | 16.01 | 30.28 | 20.91 |

### feature_lookup

| Metric | Nova (Ring+LFTJ) | Oxigraph (in-memory) | QLever (mmap, warmed) |
|---|---|---|---|
| p50 (ms) | 3.80 | 6.51 | 5.97 |
| p95 (ms) | 4.02 | 6.99 | 6.32 |

### star_with_features

| Metric | Nova (Ring+LFTJ) | Oxigraph (in-memory) | QLever (mmap, warmed) |
|---|---|---|---|
| p50 (ms) | 159.65 | 168.19 | 381.21 |
| p95 (ms) | 166.47 | 172.56 | 387.38 |

### path_2hop

| Metric | Nova (Ring+LFTJ) | Oxigraph (in-memory) | QLever (mmap, warmed) |
|---|---|---|---|
| p50 (ms) | 6283.84 | 5698.22 | 13168.32 |
| p95 (ms) | 7368.22 | 6317.30 | 13223.27 |

### triangle

| Metric | Nova (Ring+LFTJ) | Oxigraph (in-memory) | QLever (mmap, warmed) |
|---|---|---|---|
| p50 (ms) | 3243.59 | 3651.91 | 4362.18 |
| p95 (ms) | 3467.61 | 4357.70 | 4413.07 |


## Dataset Load Time

Wall-clock time to load the identical N-Triples dataset and become ready to serve queries (includes parsing + index construction for all engines; for Nova this is parse + `compact()` into the Ring/LFTJ index, for QLever this is the separate `qlever-index` build step, for Oxigraph this is the HTTP bulk-load POST into the in-memory store).

| Engine | Load time |
|---|---|
| Nova (Ring+LFTJ) | 16.54 s |
| Oxigraph (in-memory) | 24.20 s |
| QLever (mmap, warmed) | 30.97 s |

## Memory Usage (Resident Set Size)

| Engine | RSS | Storage model |
|---|---|---|
| Nova (Ring+LFTJ) | 3312.7 MiB | Pure heap |
| Oxigraph (in-memory) | 3.303GiB | Pure heap (in-memory mode) |
| QLever (mmap, warmed) | 420.1 MiB | Process RSS incl. memory-mapped index pages |

## CPU Usage (average % of one core during query phase)

| Engine | Avg CPU % |
|---|---|
| Nova (Ring+LFTJ) | 24.6% |
| Oxigraph (in-memory) | 25.8% |
| QLever (mmap, warmed) | 44.1% |

## Raw per-query summary (mean, stddev, n)

One sub-section per query, with each engine as a column and each statistic (n, mean, stddev, min, max) as a row.

### scan

| Metric | Nova (Ring+LFTJ) | Oxigraph (in-memory) | QLever (mmap, warmed) |
|---|---|---|---|
| n | 5 | 5 | 5 |
| mean (ms) | 548.84 | 531.47 | 955.04 |
| stddev (ms) | 6.71 | 18.05 | 2.16 |
| min (ms) | 537.83 | 505.82 | 951.68 |
| max (ms) | 554.12 | 550.15 | 956.95 |

### 2join

| Metric | Nova (Ring+LFTJ) | Oxigraph (in-memory) | QLever (mmap, warmed) |
|---|---|---|---|
| n | 5 | 5 | 5 |
| mean (ms) | 15.46 | 27.46 | 20.67 |
| stddev (ms) | 0.44 | 2.04 | 0.29 |
| min (ms) | 14.96 | 26.27 | 20.21 |
| max (ms) | 16.11 | 31.06 | 20.92 |

### feature_lookup

| Metric | Nova (Ring+LFTJ) | Oxigraph (in-memory) | QLever (mmap, warmed) |
|---|---|---|---|
| n | 5 | 5 | 5 |
| mean (ms) | 3.84 | 6.50 | 6.03 |
| stddev (ms) | 0.13 | 0.40 | 0.23 |
| min (ms) | 3.72 | 6.01 | 5.80 |
| max (ms) | 4.07 | 7.11 | 6.35 |

### star_with_features

| Metric | Nova (Ring+LFTJ) | Oxigraph (in-memory) | QLever (mmap, warmed) |
|---|---|---|---|
| n | 5 | 5 | 5 |
| mean (ms) | 161.14 | 168.35 | 382.12 |
| stddev (ms) | 4.38 | 3.74 | 4.65 |
| min (ms) | 156.42 | 164.21 | 376.38 |
| max (ms) | 167.03 | 172.89 | 387.78 |

### path_2hop

| Metric | Nova (Ring+LFTJ) | Oxigraph (in-memory) | QLever (mmap, warmed) |
|---|---|---|---|
| n | 5 | 5 | 5 |
| mean (ms) | 6447.26 | 5729.21 | 13172.20 |
| stddev (ms) | 685.38 | 456.21 | 39.89 |
| min (ms) | 5814.10 | 5308.44 | 13126.70 |
| max (ms) | 7600.17 | 6432.40 | 13235.87 |

### triangle

| Metric | Nova (Ring+LFTJ) | Oxigraph (in-memory) | QLever (mmap, warmed) |
|---|---|---|---|
| n | 5 | 5 | 5 |
| mean (ms) | 3292.42 | 3821.52 | 4370.86 |
| stddev (ms) | 127.93 | 398.26 | 30.93 |
| min (ms) | 3203.14 | 3629.07 | 4348.86 |
| max (ms) | 3517.25 | 4533.73 | 4424.40 |

