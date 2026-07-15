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
| Nova (Ring+LFTJ) | 0.04 s |
| Oxigraph (in-memory) | 4.90 s |
| QLever (mmap, warmed) | 3.21 s |

## Memory Usage (Physical Footprint)

Nova/QLever figures are macOS `vmmap -summary`'s "Physical footprint" (stable, allocator-retention-immune — see Methodology above); falls back to `ps -o rss` on non-macOS platforms.

| Engine | Memory | Storage model |
|---|---|---|
| Nova (Ring+LFTJ) | 116.9 MiB | Pure heap |
| Oxigraph (in-memory) | 338.7MiB | Pure heap (in-memory mode) |
| QLever (mmap, warmed) | 92.4 MiB | Incl. memory-mapped index pages |

## CPU Usage (average % of one core during query phase)

| Engine | Avg CPU % |
|---|---|
| Nova (Ring+LFTJ) | 15.5% |
| Oxigraph (in-memory) | 23.8% |
| QLever (mmap, warmed) | 24.9% |

## Latency Results (milliseconds, HTTP round-trip via curl)

One sub-section per query, with each engine as a column and each percentile (p50, p95) as a row.

### scan

| Metric | Nova (Ring+LFTJ) | Oxigraph (in-memory) | QLever (mmap, warmed) |
|---|---|---|---|
| p50 (ms) | 50.23 | 47.44 | 95.49 |
| p95 (ms) | 52.36 | 49.22 | 99.90 |

### 2join

| Metric | Nova (Ring+LFTJ) | Oxigraph (in-memory) | QLever (mmap, warmed) |
|---|---|---|---|
| p50 (ms) | 2.32 | 3.61 | 3.35 |
| p95 (ms) | 2.59 | 4.10 | 3.92 |

### feature_lookup

| Metric | Nova (Ring+LFTJ) | Oxigraph (in-memory) | QLever (mmap, warmed) |
|---|---|---|---|
| p50 (ms) | 1.47 | 5.38 | 1.92 |
| p95 (ms) | 1.69 | 8.26 | 2.33 |

### star_with_features

| Metric | Nova (Ring+LFTJ) | Oxigraph (in-memory) | QLever (mmap, warmed) |
|---|---|---|---|
| p50 (ms) | 15.57 | 17.84 | 38.71 |
| p95 (ms) | 16.22 | 19.01 | 39.78 |

### path_2hop

| Metric | Nova (Ring+LFTJ) | Oxigraph (in-memory) | QLever (mmap, warmed) |
|---|---|---|---|
| p50 (ms) | 551.21 | 509.55 | 1252.92 |
| p95 (ms) | 564.22 | 521.47 | 1291.71 |

### triangle

| Metric | Nova (Ring+LFTJ) | Oxigraph (in-memory) | QLever (mmap, warmed) |
|---|---|---|---|
| p50 (ms) | 305.07 | 337.84 | 425.02 |
| p95 (ms) | 315.80 | 350.28 | 435.89 |

## Raw per-query summary (mean, stddev, n)

One sub-section per query, with each engine as a column and each statistic (n, mean, stddev, min, max) as a row.

### scan

| Metric | Nova (Ring+LFTJ) | Oxigraph (in-memory) | QLever (mmap, warmed) |
|---|---|---|---|
| n | 10 | 10 | 10 |
| mean (ms) | 50.59 | 47.40 | 96.06 |
| stddev (ms) | 1.06 | 1.52 | 2.50 |
| min (ms) | 49.28 | 44.49 | 92.91 |
| max (ms) | 52.59 | 49.40 | 100.85 |

### 2join

| Metric | Nova (Ring+LFTJ) | Oxigraph (in-memory) | QLever (mmap, warmed) |
|---|---|---|---|
| n | 10 | 10 | 10 |
| mean (ms) | 2.26 | 3.60 | 3.46 |
| stddev (ms) | 0.27 | 0.39 | 0.31 |
| min (ms) | 1.90 | 3.06 | 3.09 |
| max (ms) | 2.63 | 4.10 | 4.08 |

### feature_lookup

| Metric | Nova (Ring+LFTJ) | Oxigraph (in-memory) | QLever (mmap, warmed) |
|---|---|---|---|
| n | 10 | 10 | 10 |
| mean (ms) | 1.44 | 5.92 | 2.02 |
| stddev (ms) | 0.20 | 1.32 | 0.23 |
| min (ms) | 1.08 | 4.67 | 1.75 |
| max (ms) | 1.71 | 8.26 | 2.35 |

### star_with_features

| Metric | Nova (Ring+LFTJ) | Oxigraph (in-memory) | QLever (mmap, warmed) |
|---|---|---|---|
| n | 10 | 10 | 10 |
| mean (ms) | 15.62 | 17.97 | 38.85 |
| stddev (ms) | 0.40 | 0.68 | 0.63 |
| min (ms) | 14.85 | 17.32 | 37.96 |
| max (ms) | 16.24 | 19.46 | 40.09 |

### path_2hop

| Metric | Nova (Ring+LFTJ) | Oxigraph (in-memory) | QLever (mmap, warmed) |
|---|---|---|---|
| n | 10 | 10 | 10 |
| mean (ms) | 551.98 | 508.44 | 1259.53 |
| stddev (ms) | 8.74 | 11.92 | 17.46 |
| min (ms) | 538.45 | 480.89 | 1247.67 |
| max (ms) | 566.56 | 521.68 | 1301.70 |

### triangle

| Metric | Nova (Ring+LFTJ) | Oxigraph (in-memory) | QLever (mmap, warmed) |
|---|---|---|---|
| n | 10 | 10 | 10 |
| mean (ms) | 305.37 | 339.92 | 427.08 |
| stddev (ms) | 7.60 | 6.29 | 5.84 |
| min (ms) | 294.96 | 332.17 | 421.64 |
| max (ms) | 316.71 | 350.44 | 438.07 |

