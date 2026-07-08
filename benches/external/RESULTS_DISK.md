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
| Oxigraph (--location) | 12.23 s |
| QLever (mmap, warmed) | 3.46 s |

## Memory Usage (Physical Footprint)

| Engine | Memory | Storage model |
|---|---|---|
| Nova (--location) | 198.0 MiB | WAL-backed heap (recovered/compacted state resident) |
| Oxigraph (--location) | 598.3MiB | RocksDB-backed (block cache + heap) |
| QLever (mmap, warmed) | 88.9 MiB | Incl. memory-mapped index pages |

## On-Disk Footprint

`du -sk` on each engine's data directory after the query phase (WAL + snapshot for Nova, full RocksDB dir for Oxigraph, all index/permutation files for QLever).

| Engine | On-disk size |
|---|---|
| Nova (--location) | 46.3 MiB |
| Oxigraph (--location) | 416.5 MiB |
| QLever (mmap, warmed) | 4.2 MiB |

## CPU Usage (average % of one core during query phase)

| Engine | Avg CPU % |
|---|---|
| Nova (--location) | 6.3% |
| Oxigraph (--location) | 57.9% |
| QLever (mmap, warmed) | 16.3% |

## Latency Results (milliseconds, HTTP round-trip via curl)

One sub-section per query, with each engine as a column and each percentile (p50, p95) as a row.

### scan

| Metric | Nova (--location, WAL-backed) | Oxigraph (--location, RocksDB-backed) | QLever (mmap, warmed) |
|---|---|---|---|
| p50 (ms) | 43.76 | 201.12 | 94.36 |
| p95 (ms) | 46.25 | 210.36 | 96.07 |

### 2join

| Metric | Nova (--location, WAL-backed) | Oxigraph (--location, RocksDB-backed) | QLever (mmap, warmed) |
|---|---|---|---|
| p50 (ms) | 1.97 | 9.24 | 2.93 |
| p95 (ms) | 2.22 | 9.76 | 3.10 |

### feature_lookup

| Metric | Nova (--location, WAL-backed) | Oxigraph (--location, RocksDB-backed) | QLever (mmap, warmed) |
|---|---|---|---|
| p50 (ms) | 0.94 | 7.00 | 1.36 |
| p95 (ms) | 1.05 | 10.38 | 1.51 |

### star_with_features

| Metric | Nova (--location, WAL-backed) | Oxigraph (--location, RocksDB-backed) | QLever (mmap, warmed) |
|---|---|---|---|
| p50 (ms) | 13.94 | 46.75 | 40.33 |
| p95 (ms) | 14.75 | 48.74 | 44.71 |

### path_2hop

| Metric | Nova (--location, WAL-backed) | Oxigraph (--location, RocksDB-backed) | QLever (mmap, warmed) |
|---|---|---|---|
| p50 (ms) | 496.30 | 2117.72 | 1283.23 |
| p95 (ms) | 520.28 | 2241.23 | 1326.23 |

### triangle

| Metric | Nova (--location, WAL-backed) | Oxigraph (--location, RocksDB-backed) | QLever (mmap, warmed) |
|---|---|---|---|
| p50 (ms) | 276.51 | 1899.82 | 430.44 |
| p95 (ms) | 282.45 | 2035.55 | 438.45 |

## Raw per-query summary (mean, stddev, n)

One sub-section per query, with each engine as a column and each statistic (n, mean, stddev, min, max) as a row.

### scan

| Metric | Nova (--location, WAL-backed) | Oxigraph (--location, RocksDB-backed) | QLever (mmap, warmed) |
|---|---|---|---|
| n | 30 | 30 | 30 |
| mean (ms) | 44.17 | 199.72 | 94.53 |
| stddev (ms) | 1.00 | 7.47 | 0.80 |
| min (ms) | 42.97 | 182.30 | 93.34 |
| max (ms) | 46.66 | 216.15 | 96.23 |

### 2join

| Metric | Nova (--location, WAL-backed) | Oxigraph (--location, RocksDB-backed) | QLever (mmap, warmed) |
|---|---|---|---|
| n | 30 | 30 | 30 |
| mean (ms) | 1.99 | 9.29 | 2.93 |
| stddev (ms) | 0.14 | 0.31 | 0.12 |
| min (ms) | 1.78 | 8.68 | 2.70 |
| max (ms) | 2.45 | 10.16 | 3.12 |

### feature_lookup

| Metric | Nova (--location, WAL-backed) | Oxigraph (--location, RocksDB-backed) | QLever (mmap, warmed) |
|---|---|---|---|
| n | 30 | 30 | 30 |
| mean (ms) | 0.93 | 7.01 | 1.37 |
| stddev (ms) | 0.08 | 2.91 | 0.10 |
| min (ms) | 0.79 | 3.39 | 1.21 |
| max (ms) | 1.11 | 18.55 | 1.63 |

### star_with_features

| Metric | Nova (--location, WAL-backed) | Oxigraph (--location, RocksDB-backed) | QLever (mmap, warmed) |
|---|---|---|---|
| n | 30 | 30 | 30 |
| mean (ms) | 14.04 | 46.76 | 40.83 |
| stddev (ms) | 0.45 | 1.50 | 2.11 |
| min (ms) | 13.26 | 43.84 | 38.34 |
| max (ms) | 14.86 | 50.01 | 47.65 |

### path_2hop

| Metric | Nova (--location, WAL-backed) | Oxigraph (--location, RocksDB-backed) | QLever (mmap, warmed) |
|---|---|---|---|
| n | 30 | 30 | 30 |
| mean (ms) | 498.86 | 2129.23 | 1289.06 |
| stddev (ms) | 8.70 | 84.71 | 14.96 |
| min (ms) | 490.81 | 2017.28 | 1275.08 |
| max (ms) | 523.86 | 2473.57 | 1339.11 |

### triangle

| Metric | Nova (--location, WAL-backed) | Oxigraph (--location, RocksDB-backed) | QLever (mmap, warmed) |
|---|---|---|---|
| n | 30 | 30 | 30 |
| mean (ms) | 277.35 | 1913.59 | 431.22 |
| stddev (ms) | 3.71 | 59.65 | 5.27 |
| min (ms) | 271.38 | 1859.32 | 422.03 |
| max (ms) | 288.60 | 2120.86 | 451.39 |

