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

**Memory usage** is reported as *physical footprint* for Nova/QLever (macOS `vmmap -summary <pid>`'s `Physical footprint:` line — falls back to `ps -o rss` on platforms without `vmmap`, e.g. Linux) and container memory for Oxigraph (`docker stats`). `vmmap`'s physical footprint is used instead of raw `ps -o rss` because on macOS, `ps` RSS includes allocator-retained-but-freed memory (`libmalloc` keeps large freed regions mapped for fast reuse rather than returning them to the OS immediately) and was observed to vary by 10x+ (30-300+ MB) run-to-run for the *identical* process and workload with zero code changes -- see `CLAUDE.md`'s "RSS investigation" section for the full writeup. `vmmap`'s physical footprint is the same figure macOS's Activity Monitor and the kernel's own memory accounting report, and is stable across repeated runs. For QLever, this figure includes memory-mapped index pages resident via the OS page cache — architecturally different from Nova/Oxigraph's pure heap allocations, but it answers the same practical question ("how much RAM does this process hold to serve the workload"), so it is used as the common denominator across all three. This asymmetry is called out explicitly here rather than left implicit.

**CPU usage** is sampled every ~0.3s throughout each engine's query phase (`ps -o %cpu` for Nova/QLever; `docker stats --format '{{.CPUPerc}}'` for Oxigraph) and averaged. Values are percent of one CPU core (e.g. 150% means 1.5 cores kept busy on average) — this is a coarse approximation, not a precise profiler measurement, but useful for relative comparison.


## Dataset Load Time

Wall-clock time to load the identical N-Triples dataset and become ready to serve queries (includes parsing + index construction for all engines; for Nova this is parse + `compact()` into the Ring/LFTJ index, for QLever this is the separate `qlever-index` build step, for Oxigraph this is the HTTP bulk-load POST into the in-memory store).

| Engine | Load time |
|---|---|
| Nova (Ring+LFTJ) | 15.44 s |
| Oxigraph (in-memory) | 22.74 s |
| QLever (mmap, warmed) | 30.63 s |

## Memory Usage (Physical Footprint)

Nova/QLever figures are macOS `vmmap -summary`'s "Physical footprint" (stable, allocator-retention-immune — see Methodology above); falls back to `ps -o rss` on non-macOS platforms.

| Engine | Memory | Storage model |
|---|---|---|
| Nova (Ring+LFTJ) | 2355.2 MiB | Pure heap |
| Oxigraph (in-memory) | 3.303GiB | Pure heap (in-memory mode) |
| QLever (mmap, warmed) | 475.0 MiB | Incl. memory-mapped index pages |

## CPU Usage (average % of one core during query phase)

| Engine | Avg CPU % |
|---|---|
| Nova (Ring+LFTJ) | 24.3% |
| Oxigraph (in-memory) | 26.1% |
| QLever (mmap, warmed) | 44.6% |

## Latency Results (milliseconds, HTTP round-trip via curl)

One sub-section per query, with each engine as a column and each percentile (p50, p95) as a row.

### scan

| Metric | Nova (Ring+LFTJ) | Oxigraph (in-memory) | QLever (mmap, warmed) |
|---|---|---|---|
| p50 (ms) | 533.18 | 533.76 | 951.28 |
| p95 (ms) | 554.99 | 723.43 | 979.62 |

### 2join

| Metric | Nova (Ring+LFTJ) | Oxigraph (in-memory) | QLever (mmap, warmed) |
|---|---|---|---|
| p50 (ms) | 14.63 | 26.05 | 21.12 |
| p95 (ms) | 15.00 | 27.54 | 21.45 |

### feature_lookup

| Metric | Nova (Ring+LFTJ) | Oxigraph (in-memory) | QLever (mmap, warmed) |
|---|---|---|---|
| p50 (ms) | 4.24 | 7.32 | 6.40 |
| p95 (ms) | 4.74 | 7.44 | 6.57 |

### star_with_features

| Metric | Nova (Ring+LFTJ) | Oxigraph (in-memory) | QLever (mmap, warmed) |
|---|---|---|---|
| p50 (ms) | 160.99 | 162.18 | 377.66 |
| p95 (ms) | 169.32 | 166.59 | 389.09 |

### path_2hop

| Metric | Nova (Ring+LFTJ) | Oxigraph (in-memory) | QLever (mmap, warmed) |
|---|---|---|---|
| p50 (ms) | 5683.36 | 5243.08 | 12743.30 |
| p95 (ms) | 6860.48 | 5648.08 | 12798.09 |

### triangle

| Metric | Nova (Ring+LFTJ) | Oxigraph (in-memory) | QLever (mmap, warmed) |
|---|---|---|---|
| p50 (ms) | 3167.29 | 3787.24 | 4222.68 |
| p95 (ms) | 3207.09 | 4107.60 | 4227.74 |

## Raw per-query summary (mean, stddev, n)

One sub-section per query, with each engine as a column and each statistic (n, mean, stddev, min, max) as a row.

### scan

| Metric | Nova (Ring+LFTJ) | Oxigraph (in-memory) | QLever (mmap, warmed) |
|---|---|---|---|
| n | 5 | 5 | 5 |
| mean (ms) | 538.62 | 573.63 | 950.53 |
| stddev (ms) | 11.95 | 108.98 | 22.55 |
| min (ms) | 528.96 | 500.41 | 929.31 |
| max (ms) | 558.71 | 763.53 | 986.57 |

### 2join

| Metric | Nova (Ring+LFTJ) | Oxigraph (in-memory) | QLever (mmap, warmed) |
|---|---|---|---|
| n | 5 | 5 | 5 |
| mean (ms) | 14.69 | 26.37 | 21.11 |
| stddev (ms) | 0.24 | 1.00 | 0.32 |
| min (ms) | 14.43 | 25.47 | 20.66 |
| max (ms) | 15.08 | 27.62 | 21.48 |

### feature_lookup

| Metric | Nova (Ring+LFTJ) | Oxigraph (in-memory) | QLever (mmap, warmed) |
|---|---|---|---|
| n | 5 | 5 | 5 |
| mean (ms) | 4.32 | 7.23 | 6.30 |
| stddev (ms) | 0.33 | 0.25 | 0.34 |
| min (ms) | 3.96 | 6.83 | 5.71 |
| max (ms) | 4.84 | 7.46 | 6.60 |

### star_with_features

| Metric | Nova (Ring+LFTJ) | Oxigraph (in-memory) | QLever (mmap, warmed) |
|---|---|---|---|
| n | 5 | 5 | 5 |
| mean (ms) | 163.05 | 161.55 | 379.09 |
| stddev (ms) | 4.60 | 4.70 | 7.76 |
| min (ms) | 159.01 | 155.07 | 371.67 |
| max (ms) | 170.76 | 167.15 | 390.80 |

### path_2hop

| Metric | Nova (Ring+LFTJ) | Oxigraph (in-memory) | QLever (mmap, warmed) |
|---|---|---|---|
| n | 5 | 5 | 5 |
| mean (ms) | 6012.15 | 5342.15 | 12736.09 |
| stddev (ms) | 628.94 | 223.39 | 64.78 |
| min (ms) | 5543.63 | 5217.21 | 12637.07 |
| max (ms) | 7012.61 | 5739.22 | 12801.95 |

### triangle

| Metric | Nova (Ring+LFTJ) | Oxigraph (in-memory) | QLever (mmap, warmed) |
|---|---|---|---|
| n | 5 | 5 | 5 |
| mean (ms) | 3159.18 | 3818.02 | 4216.52 |
| stddev (ms) | 47.13 | 216.79 | 13.52 |
| min (ms) | 3086.52 | 3630.15 | 4195.48 |
| max (ms) | 3213.90 | 4178.93 | 4228.34 |

