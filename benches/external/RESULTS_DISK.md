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
| Nova (--location) | 2.12 s |
| Oxigraph (--location) | 11.00 s |
| QLever (mmap, warmed) | 3.13 s |

## Memory Usage (Physical Footprint)

| Engine | Memory | Storage model |
|---|---|---|
| Nova (--location) | 281.5 MiB | WAL-backed heap (recovered/compacted state resident) |
| Oxigraph (--location) | 598.4MiB | RocksDB-backed (block cache + heap) |
| QLever (mmap, warmed) | 88.2 MiB | Incl. memory-mapped index pages |

## On-Disk Footprint

`du -sk` on each engine's data directory after the query phase (WAL + snapshot for Nova, full RocksDB dir for Oxigraph, all index/permutation files for QLever).

| Engine | On-disk size |
|---|---|
| Nova (--location) | 2.3 MiB |
| Oxigraph (--location) | 416.5 MiB |
| QLever (mmap, warmed) | 4.2 MiB |

## CPU Usage (average % of one core during query phase)

| Engine | Avg CPU % |
|---|---|
| Nova (--location) | 8.6% |
| Oxigraph (--location) | 57.5% |
| QLever (mmap, warmed) | 15.5% |

## Latency Results (milliseconds, HTTP round-trip via curl)

One sub-section per query, with each engine as a column and each percentile (p50, p95) as a row.

### scan

| Metric | Nova (--location, WAL-backed) | Oxigraph (--location, RocksDB-backed) | QLever (mmap, warmed) |
|---|---|---|---|
| p50 (ms) | 53.54 | 191.18 | 95.10 |
| p95 (ms) | 56.17 | 209.99 | 97.99 |

### 2join

| Metric | Nova (--location, WAL-backed) | Oxigraph (--location, RocksDB-backed) | QLever (mmap, warmed) |
|---|---|---|---|
| p50 (ms) | 2.00 | 9.16 | 2.84 |
| p95 (ms) | 2.09 | 9.87 | 2.91 |

### feature_lookup

| Metric | Nova (--location, WAL-backed) | Oxigraph (--location, RocksDB-backed) | QLever (mmap, warmed) |
|---|---|---|---|
| p50 (ms) | 0.79 | 6.60 | 1.32 |
| p95 (ms) | 0.91 | 8.74 | 1.50 |

### star_with_features

| Metric | Nova (--location, WAL-backed) | Oxigraph (--location, RocksDB-backed) | QLever (mmap, warmed) |
|---|---|---|---|
| p50 (ms) | 15.18 | 48.04 | 38.84 |
| p95 (ms) | 15.48 | 50.13 | 39.32 |

### path_2hop

| Metric | Nova (--location, WAL-backed) | Oxigraph (--location, RocksDB-backed) | QLever (mmap, warmed) |
|---|---|---|---|
| p50 (ms) | 578.38 | 2135.09 | 1284.85 |
| p95 (ms) | 590.36 | 2317.15 | 1297.11 |

### triangle

| Metric | Nova (--location, WAL-backed) | Oxigraph (--location, RocksDB-backed) | QLever (mmap, warmed) |
|---|---|---|---|
| p50 (ms) | 347.07 | 1938.63 | 431.42 |
| p95 (ms) | 351.50 | 2081.21 | 443.66 |

## Raw per-query summary (mean, stddev, n)

One sub-section per query, with each engine as a column and each statistic (n, mean, stddev, min, max) as a row.

### scan

| Metric | Nova (--location, WAL-backed) | Oxigraph (--location, RocksDB-backed) | QLever (mmap, warmed) |
|---|---|---|---|
| n | 30 | 30 | 30 |
| mean (ms) | 53.83 | 193.69 | 95.54 |
| stddev (ms) | 0.97 | 7.37 | 1.18 |
| min (ms) | 52.54 | 183.60 | 94.19 |
| max (ms) | 56.57 | 211.06 | 98.20 |

### 2join

| Metric | Nova (--location, WAL-backed) | Oxigraph (--location, RocksDB-backed) | QLever (mmap, warmed) |
|---|---|---|---|
| n | 30 | 30 | 30 |
| mean (ms) | 2.00 | 9.16 | 2.82 |
| stddev (ms) | 0.07 | 0.36 | 0.07 |
| min (ms) | 1.88 | 8.51 | 2.64 |
| max (ms) | 2.12 | 10.00 | 2.97 |

### feature_lookup

| Metric | Nova (--location, WAL-backed) | Oxigraph (--location, RocksDB-backed) | QLever (mmap, warmed) |
|---|---|---|---|
| n | 30 | 30 | 30 |
| mean (ms) | 0.80 | 6.32 | 1.42 |
| stddev (ms) | 0.06 | 1.70 | 0.52 |
| min (ms) | 0.70 | 3.78 | 1.21 |
| max (ms) | 0.93 | 9.18 | 4.18 |

### star_with_features

| Metric | Nova (--location, WAL-backed) | Oxigraph (--location, RocksDB-backed) | QLever (mmap, warmed) |
|---|---|---|---|
| n | 30 | 30 | 30 |
| mean (ms) | 15.22 | 48.43 | 38.89 |
| stddev (ms) | 0.17 | 1.16 | 0.31 |
| min (ms) | 14.89 | 46.77 | 38.46 |
| max (ms) | 15.68 | 51.67 | 39.79 |

### path_2hop

| Metric | Nova (--location, WAL-backed) | Oxigraph (--location, RocksDB-backed) | QLever (mmap, warmed) |
|---|---|---|---|
| n | 30 | 30 | 30 |
| mean (ms) | 580.05 | 2161.25 | 1286.64 |
| stddev (ms) | 6.11 | 71.90 | 9.00 |
| min (ms) | 572.85 | 2088.11 | 1274.19 |
| max (ms) | 603.47 | 2352.69 | 1320.63 |

### triangle

| Metric | Nova (--location, WAL-backed) | Oxigraph (--location, RocksDB-backed) | QLever (mmap, warmed) |
|---|---|---|---|
| n | 30 | 30 | 30 |
| mean (ms) | 347.30 | 1953.11 | 432.61 |
| stddev (ms) | 2.22 | 67.54 | 5.67 |
| min (ms) | 342.58 | 1866.54 | 425.32 |
| max (ms) | 352.38 | 2116.59 | 451.16 |

