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
| Oxigraph (--location) | 8.30 s |
| QLever (mmap, warmed) | 3.04 s |

## Memory Usage (Physical Footprint)

| Engine | Memory | Storage model |
|---|---|---|
| Nova (--location) | 81.0 MiB | WAL-backed heap (recovered/compacted state resident) |
| Oxigraph (--location) | 599.6MiB | RocksDB-backed (block cache + heap) |
| QLever (mmap, warmed) | 87.4 MiB | Incl. memory-mapped index pages |

## On-Disk Footprint

`du -sk` on each engine's data directory after the query phase (WAL + snapshot for Nova, full RocksDB dir for Oxigraph, all index/permutation files for QLever).

| Engine | On-disk size |
|---|---|
| Nova (--location) | 46.3 MiB |
| Oxigraph (--location) | 416.8 MiB |
| QLever (mmap, warmed) | 4.2 MiB |

## CPU Usage (average % of one core during query phase)

| Engine | Avg CPU % |
|---|---|
| Nova (--location) | 7.2% |
| Oxigraph (--location) | 55.5% |
| QLever (mmap, warmed) | 16.8% |

## Latency Results (milliseconds, HTTP round-trip via curl)

One sub-section per query, with each engine as a column and each percentile (p50, p95) as a row.

### scan

| Metric | Nova (--location, WAL-backed) | Oxigraph (--location, RocksDB-backed) | QLever (mmap, warmed) |
|---|---|---|---|
| p50 (ms) | 45.64 | 163.58 | 92.59 |
| p95 (ms) | 50.67 | 173.46 | 96.43 |

### 2join

| Metric | Nova (--location, WAL-backed) | Oxigraph (--location, RocksDB-backed) | QLever (mmap, warmed) |
|---|---|---|---|
| p50 (ms) | 1.77 | 9.35 | 2.73 |
| p95 (ms) | 2.07 | 11.70 | 2.93 |

### feature_lookup

| Metric | Nova (--location, WAL-backed) | Oxigraph (--location, RocksDB-backed) | QLever (mmap, warmed) |
|---|---|---|---|
| p50 (ms) | 0.76 | 4.22 | 1.20 |
| p95 (ms) | 0.84 | 7.40 | 1.29 |

### star_with_features

| Metric | Nova (--location, WAL-backed) | Oxigraph (--location, RocksDB-backed) | QLever (mmap, warmed) |
|---|---|---|---|
| p50 (ms) | 14.50 | 44.83 | 37.58 |
| p95 (ms) | 15.66 | 46.72 | 38.44 |

### path_2hop

| Metric | Nova (--location, WAL-backed) | Oxigraph (--location, RocksDB-backed) | QLever (mmap, warmed) |
|---|---|---|---|
| p50 (ms) | 523.99 | 1913.33 | 1266.50 |
| p95 (ms) | 547.09 | 2024.72 | 1297.89 |

### triangle

| Metric | Nova (--location, WAL-backed) | Oxigraph (--location, RocksDB-backed) | QLever (mmap, warmed) |
|---|---|---|---|
| p50 (ms) | 280.98 | 1633.79 | 421.94 |
| p95 (ms) | 285.53 | 1706.96 | 430.86 |

## Raw per-query summary (mean, stddev, n)

One sub-section per query, with each engine as a column and each statistic (n, mean, stddev, min, max) as a row.

### scan

| Metric | Nova (--location, WAL-backed) | Oxigraph (--location, RocksDB-backed) | QLever (mmap, warmed) |
|---|---|---|---|
| n | 30 | 30 | 30 |
| mean (ms) | 46.77 | 164.55 | 93.10 |
| stddev (ms) | 2.14 | 4.75 | 1.90 |
| min (ms) | 44.45 | 156.53 | 91.62 |
| max (ms) | 52.69 | 178.03 | 100.84 |

### 2join

| Metric | Nova (--location, WAL-backed) | Oxigraph (--location, RocksDB-backed) | QLever (mmap, warmed) |
|---|---|---|---|
| n | 30 | 30 | 30 |
| mean (ms) | 1.81 | 9.71 | 2.79 |
| stddev (ms) | 0.16 | 1.36 | 0.33 |
| min (ms) | 1.52 | 7.68 | 2.54 |
| max (ms) | 2.12 | 11.75 | 4.48 |

### feature_lookup

| Metric | Nova (--location, WAL-backed) | Oxigraph (--location, RocksDB-backed) | QLever (mmap, warmed) |
|---|---|---|---|
| n | 30 | 30 | 30 |
| mean (ms) | 0.77 | 4.55 | 1.21 |
| stddev (ms) | 0.05 | 1.72 | 0.05 |
| min (ms) | 0.70 | 2.64 | 1.09 |
| max (ms) | 0.91 | 7.95 | 1.35 |

### star_with_features

| Metric | Nova (--location, WAL-backed) | Oxigraph (--location, RocksDB-backed) | QLever (mmap, warmed) |
|---|---|---|---|
| n | 30 | 30 | 30 |
| mean (ms) | 14.60 | 44.40 | 37.67 |
| stddev (ms) | 0.53 | 1.71 | 0.40 |
| min (ms) | 13.86 | 41.49 | 36.90 |
| max (ms) | 16.36 | 47.32 | 38.53 |

### path_2hop

| Metric | Nova (--location, WAL-backed) | Oxigraph (--location, RocksDB-backed) | QLever (mmap, warmed) |
|---|---|---|---|
| n | 30 | 30 | 30 |
| mean (ms) | 523.91 | 1914.19 | 1270.25 |
| stddev (ms) | 14.02 | 57.98 | 11.79 |
| min (ms) | 496.18 | 1830.04 | 1255.69 |
| max (ms) | 558.64 | 2044.04 | 1303.78 |

### triangle

| Metric | Nova (--location, WAL-backed) | Oxigraph (--location, RocksDB-backed) | QLever (mmap, warmed) |
|---|---|---|---|
| n | 30 | 30 | 30 |
| mean (ms) | 281.73 | 1644.37 | 427.02 |
| stddev (ms) | 2.31 | 47.54 | 25.18 |
| min (ms) | 278.43 | 1597.38 | 418.61 |
| max (ms) | 286.75 | 1845.51 | 559.10 |

