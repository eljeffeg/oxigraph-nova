# Comparative Benchmark (Disk-Backed): Nova vs Oxigraph vs QLever

Dataset: 50,000 synthetic BSBM-style entities (1,250,000 triples), identical N-Triples file loaded into all three engines.

## Methodology & Storage Model

This is the **disk-backed/persistent-storage** sibling of `RESULTS.md` (the pure in-memory comparison). All three engines were benchmarked over the SPARQL 1.1 HTTP Protocol using **byte-identical SPARQL query text** against a **byte-identical dataset**. Each query was run with a warm-up pass (discarded) before N timed iterations.

**Storage model per engine** (this matters — see below):

| Engine | Storage model | Notes |
|---|---|---|
| **Nova** | `RingStore::open(dir)` — WAL-backed | Every `insert()` is durably logged (fsync-per-write) to a write-ahead log before being applied in memory; periodic `compact()` merges the delta into an ε-serde snapshot on disk. |
| **Oxigraph** | `serve --location <dir>` — RocksDB-backed | Oxigraph's own default/production persistent storage mode (`oxrocksdb-sys`). |
| **QLever** | Memory-mapped disk index (mmap) | Unchanged from the in-memory comparison — QLever has no other mode. A warm-up pass ensures the OS page cache holds the working set resident before timed measurements. |

**Memory usage** is reported as *physical footprint* for Nova/QLever (macOS `vmmap -summary <pid>`'s `Physical footprint:` line — falls back to `ps -o rss` on platforms without `vmmap`) and container memory for Oxigraph (`docker stats`). See `README.md` for the full rationale behind this choice over raw `ps -o rss`.

**On-disk footprint** is measured via `du -sk` on each engine's data directory after the query phase completes (includes WAL + snapshot files for Nova, the full RocksDB directory for Oxigraph, and all QLever index/permutation files).

**CPU usage** is sampled every ~0.3s throughout each engine's query phase and averaged. Values are percent of one CPU core.


## Dataset Load Time

Wall-clock time to load the identical N-Triples dataset and become ready to serve queries. For Nova this includes WAL-logging every triple (fsync-per-write) plus a `compact()` pass — necessarily slower than the in-memory `bulk_load()` path measured in `RESULTS.md`. For Oxigraph this is the HTTP bulk-load POST into the RocksDB-backed store. For QLever this is the same `qlever-index` build step as the in-memory comparison (QLever's index is always disk-based).

| Engine | Load time |
|---|---|
| Nova (--location) | 2.08 s |
| Oxigraph (--location) | 10.03 s |
| QLever (mmap, warmed) | 3.15 s |

## Memory Usage (Physical Footprint)

| Engine | Memory | Storage model |
|---|---|---|
| Nova (--location) | 250.3 MiB | WAL-backed heap (recovered/compacted state resident) |
| Oxigraph (--location) | 598.5MiB | RocksDB-backed (block cache + heap) |
| QLever (mmap, warmed) | 87.8 MiB | Incl. memory-mapped index pages |

## On-Disk Footprint

`du -sk` on each engine's data directory after the query phase (WAL + snapshot for Nova, full RocksDB dir for Oxigraph, all index/permutation files for QLever).

| Engine | On-disk size |
|---|---|
| Nova (--location) | 45.4 MiB |
| Oxigraph (--location) | 416.5 MiB |
| QLever (mmap, warmed) | 4.2 MiB |

## CPU Usage (average % of one core during query phase)

| Engine | Avg CPU % |
|---|---|
| Nova (--location) | 7.3% |
| Oxigraph (--location) | 57.3% |
| QLever (mmap, warmed) | 16.5% |

## Latency Results (milliseconds, HTTP round-trip via curl)

One sub-section per query, with each engine as a column and each percentile (p50, p95) as a row.

### scan

| Metric | Nova (--location, WAL-backed) | Oxigraph (--location, RocksDB-backed) | QLever (mmap, warmed) |
|---|---|---|---|
| p50 (ms) | 43.65 | 165.99 | 94.57 |
| p95 (ms) | 44.94 | 173.69 | 95.59 |

### 2join

| Metric | Nova (--location, WAL-backed) | Oxigraph (--location, RocksDB-backed) | QLever (mmap, warmed) |
|---|---|---|---|
| p50 (ms) | 1.74 | 8.87 | 2.77 |
| p95 (ms) | 1.89 | 9.82 | 2.96 |

### feature_lookup

| Metric | Nova (--location, WAL-backed) | Oxigraph (--location, RocksDB-backed) | QLever (mmap, warmed) |
|---|---|---|---|
| p50 (ms) | 0.77 | 6.63 | 1.24 |
| p95 (ms) | 0.86 | 8.96 | 1.38 |

### star_with_features

| Metric | Nova (--location, WAL-backed) | Oxigraph (--location, RocksDB-backed) | QLever (mmap, warmed) |
|---|---|---|---|
| p50 (ms) | 13.30 | 43.49 | 38.77 |
| p95 (ms) | 13.56 | 44.31 | 39.30 |

### path_2hop

| Metric | Nova (--location, WAL-backed) | Oxigraph (--location, RocksDB-backed) | QLever (mmap, warmed) |
|---|---|---|---|
| p50 (ms) | 493.41 | 2023.93 | 1282.23 |
| p95 (ms) | 507.38 | 2193.30 | 1294.99 |

### triangle

| Metric | Nova (--location, WAL-backed) | Oxigraph (--location, RocksDB-backed) | QLever (mmap, warmed) |
|---|---|---|---|
| p50 (ms) | 271.20 | 1836.25 | 428.02 |
| p95 (ms) | 277.06 | 1953.50 | 435.78 |

## Raw per-query summary (mean, stddev, n)

One sub-section per query, with each engine as a column and each statistic (n, mean, stddev, min, max) as a row.

### scan

| Metric | Nova (--location, WAL-backed) | Oxigraph (--location, RocksDB-backed) | QLever (mmap, warmed) |
|---|---|---|---|
| n | 30 | 30 | 30 |
| mean (ms) | 43.80 | 166.07 | 94.48 |
| stddev (ms) | 0.68 | 4.11 | 0.66 |
| min (ms) | 42.53 | 159.53 | 92.94 |
| max (ms) | 45.40 | 174.89 | 96.08 |

### 2join

| Metric | Nova (--location, WAL-backed) | Oxigraph (--location, RocksDB-backed) | QLever (mmap, warmed) |
|---|---|---|---|
| n | 30 | 30 | 30 |
| mean (ms) | 1.75 | 8.79 | 2.78 |
| stddev (ms) | 0.08 | 0.70 | 0.10 |
| min (ms) | 1.60 | 7.73 | 2.63 |
| max (ms) | 1.97 | 10.18 | 3.02 |

### feature_lookup

| Metric | Nova (--location, WAL-backed) | Oxigraph (--location, RocksDB-backed) | QLever (mmap, warmed) |
|---|---|---|---|
| n | 30 | 30 | 30 |
| mean (ms) | 0.76 | 6.28 | 1.25 |
| stddev (ms) | 0.07 | 2.29 | 0.07 |
| min (ms) | 0.63 | 2.99 | 1.11 |
| max (ms) | 0.92 | 13.44 | 1.38 |

### star_with_features

| Metric | Nova (--location, WAL-backed) | Oxigraph (--location, RocksDB-backed) | QLever (mmap, warmed) |
|---|---|---|---|
| n | 30 | 30 | 30 |
| mean (ms) | 13.30 | 43.52 | 38.83 |
| stddev (ms) | 0.20 | 1.04 | 0.35 |
| min (ms) | 12.90 | 41.82 | 38.09 |
| max (ms) | 13.74 | 47.40 | 39.84 |

### path_2hop

| Metric | Nova (--location, WAL-backed) | Oxigraph (--location, RocksDB-backed) | QLever (mmap, warmed) |
|---|---|---|---|
| n | 30 | 30 | 30 |
| mean (ms) | 495.21 | 2029.54 | 1284.59 |
| stddev (ms) | 5.75 | 89.32 | 8.97 |
| min (ms) | 486.77 | 1908.70 | 1273.53 |
| max (ms) | 510.77 | 2249.46 | 1322.22 |

### triangle

| Metric | Nova (--location, WAL-backed) | Oxigraph (--location, RocksDB-backed) | QLever (mmap, warmed) |
|---|---|---|---|
| n | 30 | 30 | 30 |
| mean (ms) | 271.60 | 1857.63 | 432.42 |
| stddev (ms) | 2.90 | 56.08 | 23.65 |
| min (ms) | 267.34 | 1779.50 | 424.60 |
| max (ms) | 278.89 | 2032.29 | 556.82 |

