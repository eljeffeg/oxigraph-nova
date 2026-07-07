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
| Nova (--location) | 2.09 s |
| Oxigraph (--location) | 10.00 s |
| QLever (mmap, warmed) | 3.28 s |

## Memory Usage (Physical Footprint)

| Engine | Memory | Storage model |
|---|---|---|
| Nova (--location) | 216.4 MiB | WAL-backed heap (recovered/compacted state resident) |
| Oxigraph (--location) | 599.4MiB | RocksDB-backed (block cache + heap) |
| QLever (mmap, warmed) | 95.8 MiB | Incl. memory-mapped index pages |

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
| Nova (--location) | 5.9% |
| Oxigraph (--location) | 56.6% |
| QLever (mmap, warmed) | 16.8% |

## Latency Results (milliseconds, HTTP round-trip via curl)

One sub-section per query, with each engine as a column and each percentile (p50, p95) as a row.

### scan

| Metric | Nova (--location, WAL-backed) | Oxigraph (--location, RocksDB-backed) | QLever (mmap, warmed) |
|---|---|---|---|
| p50 (ms) | 42.38 | 173.89 | 92.97 |
| p95 (ms) | 44.02 | 212.95 | 94.55 |

### 2join

| Metric | Nova (--location, WAL-backed) | Oxigraph (--location, RocksDB-backed) | QLever (mmap, warmed) |
|---|---|---|---|
| p50 (ms) | 1.66 | 8.75 | 2.84 |
| p95 (ms) | 1.91 | 11.84 | 3.12 |

### feature_lookup

| Metric | Nova (--location, WAL-backed) | Oxigraph (--location, RocksDB-backed) | QLever (mmap, warmed) |
|---|---|---|---|
| p50 (ms) | 0.73 | 6.60 | 1.19 |
| p95 (ms) | 0.98 | 8.61 | 1.30 |

### star_with_features

| Metric | Nova (--location, WAL-backed) | Oxigraph (--location, RocksDB-backed) | QLever (mmap, warmed) |
|---|---|---|---|
| p50 (ms) | 13.18 | 43.32 | 37.78 |
| p95 (ms) | 13.89 | 48.77 | 38.56 |

### path_2hop

| Metric | Nova (--location, WAL-backed) | Oxigraph (--location, RocksDB-backed) | QLever (mmap, warmed) |
|---|---|---|---|
| p50 (ms) | 488.87 | 1936.69 | 1263.60 |
| p95 (ms) | 506.25 | 2045.92 | 1296.20 |

### triangle

| Metric | Nova (--location, WAL-backed) | Oxigraph (--location, RocksDB-backed) | QLever (mmap, warmed) |
|---|---|---|---|
| p50 (ms) | 270.94 | 1726.99 | 421.84 |
| p95 (ms) | 277.91 | 1775.59 | 425.55 |

## Raw per-query summary (mean, stddev, n)

One sub-section per query, with each engine as a column and each statistic (n, mean, stddev, min, max) as a row.

### scan

| Metric | Nova (--location, WAL-backed) | Oxigraph (--location, RocksDB-backed) | QLever (mmap, warmed) |
|---|---|---|---|
| n | 30 | 30 | 30 |
| mean (ms) | 42.38 | 179.77 | 93.11 |
| stddev (ms) | 0.78 | 16.41 | 0.71 |
| min (ms) | 40.96 | 163.08 | 92.06 |
| max (ms) | 44.39 | 222.01 | 94.86 |

### 2join

| Metric | Nova (--location, WAL-backed) | Oxigraph (--location, RocksDB-backed) | QLever (mmap, warmed) |
|---|---|---|---|
| n | 30 | 30 | 30 |
| mean (ms) | 1.69 | 9.05 | 2.86 |
| stddev (ms) | 0.13 | 1.32 | 0.12 |
| min (ms) | 1.48 | 6.29 | 2.69 |
| max (ms) | 1.98 | 12.05 | 3.21 |

### feature_lookup

| Metric | Nova (--location, WAL-backed) | Oxigraph (--location, RocksDB-backed) | QLever (mmap, warmed) |
|---|---|---|---|
| n | 30 | 30 | 30 |
| mean (ms) | 0.75 | 6.12 | 1.19 |
| stddev (ms) | 0.12 | 1.91 | 0.07 |
| min (ms) | 0.53 | 2.58 | 1.06 |
| max (ms) | 1.10 | 9.04 | 1.34 |

### star_with_features

| Metric | Nova (--location, WAL-backed) | Oxigraph (--location, RocksDB-backed) | QLever (mmap, warmed) |
|---|---|---|---|
| n | 30 | 30 | 30 |
| mean (ms) | 13.26 | 43.76 | 37.85 |
| stddev (ms) | 0.40 | 2.23 | 0.40 |
| min (ms) | 12.65 | 41.45 | 37.26 |
| max (ms) | 14.07 | 50.94 | 39.13 |

### path_2hop

| Metric | Nova (--location, WAL-backed) | Oxigraph (--location, RocksDB-backed) | QLever (mmap, warmed) |
|---|---|---|---|
| n | 30 | 30 | 30 |
| mean (ms) | 490.50 | 1947.84 | 1267.19 |
| stddev (ms) | 6.84 | 44.74 | 13.45 |
| min (ms) | 484.02 | 1889.19 | 1249.95 |
| max (ms) | 513.79 | 2063.11 | 1308.55 |

### triangle

| Metric | Nova (--location, WAL-backed) | Oxigraph (--location, RocksDB-backed) | QLever (mmap, warmed) |
|---|---|---|---|
| n | 30 | 30 | 30 |
| mean (ms) | 271.60 | 1723.71 | 426.10 |
| stddev (ms) | 3.35 | 40.94 | 22.68 |
| min (ms) | 267.45 | 1648.74 | 418.67 |
| max (ms) | 282.25 | 1853.14 | 545.78 |

