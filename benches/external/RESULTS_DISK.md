# Comparative Benchmark (Disk-Backed): Nova vs Oxigraph vs QLever

Dataset: 250,000 synthetic BSBM-style entities (6,250,000 triples), identical N-Triples file loaded into all three engines.

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
| Nova (--location) | 8.26 s |
| Oxigraph (--location) | 62.18 s |
| QLever (mmap, warmed) | 15.74 s |

## Memory Usage (Physical Footprint)

| Engine | Memory | Storage model |
|---|---|---|
| Nova (--location) | 356.1 MiB | WAL-backed heap (recovered/compacted state resident) |
| Oxigraph (--location) | 2.898GiB | RocksDB-backed (block cache + heap) |
| QLever (mmap, warmed) | 315.0 MiB | Incl. memory-mapped index pages |

## On-Disk Footprint

`du -sk` on each engine's data directory after the query phase (WAL + snapshot for Nova, full RocksDB dir for Oxigraph, all index/permutation files for QLever).

| Engine | On-disk size |
|---|---|
| Nova (--location) | 103.8 MiB |
| Oxigraph (--location) | 2064.4 MiB |
| QLever (mmap, warmed) | 24.7 MiB |

## CPU Usage (average % of one core during query phase)

| Engine | Avg CPU % |
|---|---|
| Nova (--location) | 9.1% |
| Oxigraph (--location) | 66.9% |
| QLever (mmap, warmed) | 19.5% |

## Latency Results (milliseconds, HTTP round-trip via curl)

One sub-section per query, with each engine as a column and each percentile (p50, p95) as a row.

### scan

| Metric | Nova (--location, WAL-backed) | Oxigraph (--location, RocksDB-backed) | QLever (mmap, warmed) |
|---|---|---|---|
| p50 (ms) | 296.75 | 1087.52 | 485.00 |
| p95 (ms) | 309.50 | 1136.39 | 512.00 |

### 2join

| Metric | Nova (--location, WAL-backed) | Oxigraph (--location, RocksDB-backed) | QLever (mmap, warmed) |
|---|---|---|---|
| p50 (ms) | 8.40 | 45.22 | 12.01 |
| p95 (ms) | 9.29 | 46.73 | 12.58 |

### feature_lookup

| Metric | Nova (--location, WAL-backed) | Oxigraph (--location, RocksDB-backed) | QLever (mmap, warmed) |
|---|---|---|---|
| p50 (ms) | 3.14 | 18.11 | 5.50 |
| p95 (ms) | 4.15 | 24.61 | 5.95 |

### star_with_features

| Metric | Nova (--location, WAL-backed) | Oxigraph (--location, RocksDB-backed) | QLever (mmap, warmed) |
|---|---|---|---|
| p50 (ms) | 72.06 | 276.89 | 190.60 |
| p95 (ms) | 76.26 | 291.73 | 193.71 |

### path_2hop

| Metric | Nova (--location, WAL-backed) | Oxigraph (--location, RocksDB-backed) | QLever (mmap, warmed) |
|---|---|---|---|
| p50 (ms) | 2602.04 | 13610.11 | 6308.82 |
| p95 (ms) | 2943.89 | 14212.44 | 6458.60 |

### triangle

| Metric | Nova (--location, WAL-backed) | Oxigraph (--location, RocksDB-backed) | QLever (mmap, warmed) |
|---|---|---|---|
| p50 (ms) | 1461.27 | 13347.38 | 2087.93 |
| p95 (ms) | 1479.45 | 15320.67 | 2149.41 |

## Raw per-query summary (mean, stddev, n)

One sub-section per query, with each engine as a column and each statistic (n, mean, stddev, min, max) as a row.

### scan

| Metric | Nova (--location, WAL-backed) | Oxigraph (--location, RocksDB-backed) | QLever (mmap, warmed) |
|---|---|---|---|
| n | 10 | 10 | 10 |
| mean (ms) | 298.14 | 1087.05 | 490.03 |
| stddev (ms) | 8.47 | 33.23 | 14.12 |
| min (ms) | 286.08 | 1047.54 | 476.03 |
| max (ms) | 310.46 | 1156.32 | 521.87 |

### 2join

| Metric | Nova (--location, WAL-backed) | Oxigraph (--location, RocksDB-backed) | QLever (mmap, warmed) |
|---|---|---|---|
| n | 10 | 10 | 10 |
| mean (ms) | 8.49 | 45.21 | 12.03 |
| stddev (ms) | 0.52 | 1.04 | 0.35 |
| min (ms) | 7.74 | 43.39 | 11.56 |
| max (ms) | 9.31 | 47.05 | 12.76 |

### feature_lookup

| Metric | Nova (--location, WAL-backed) | Oxigraph (--location, RocksDB-backed) | QLever (mmap, warmed) |
|---|---|---|---|
| n | 10 | 10 | 10 |
| mean (ms) | 3.21 | 18.61 | 5.47 |
| stddev (ms) | 0.62 | 3.90 | 0.41 |
| min (ms) | 2.32 | 14.25 | 4.59 |
| max (ms) | 4.54 | 26.50 | 6.11 |

### star_with_features

| Metric | Nova (--location, WAL-backed) | Oxigraph (--location, RocksDB-backed) | QLever (mmap, warmed) |
|---|---|---|---|
| n | 10 | 10 | 10 |
| mean (ms) | 72.45 | 274.46 | 191.01 |
| stddev (ms) | 2.64 | 12.42 | 1.71 |
| min (ms) | 69.70 | 259.09 | 189.08 |
| max (ms) | 77.41 | 297.32 | 195.37 |

### path_2hop

| Metric | Nova (--location, WAL-backed) | Oxigraph (--location, RocksDB-backed) | QLever (mmap, warmed) |
|---|---|---|---|
| n | 10 | 10 | 10 |
| mean (ms) | 2657.42 | 13681.23 | 6330.23 |
| stddev (ms) | 179.64 | 349.97 | 79.85 |
| min (ms) | 2529.99 | 13287.31 | 6247.89 |
| max (ms) | 3136.56 | 14217.11 | 6496.68 |

### triangle

| Metric | Nova (--location, WAL-backed) | Oxigraph (--location, RocksDB-backed) | QLever (mmap, warmed) |
|---|---|---|---|
| n | 10 | 10 | 10 |
| mean (ms) | 1448.34 | 13560.36 | 2103.42 |
| stddev (ms) | 31.58 | 1068.44 | 28.20 |
| min (ms) | 1399.09 | 12669.36 | 2081.55 |
| max (ms) | 1481.34 | 16341.61 | 2152.20 |

