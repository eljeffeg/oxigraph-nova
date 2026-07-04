# Comparative Benchmark (Disk-Backed): Nova vs Oxigraph vs QLever

Dataset: 2,000 synthetic BSBM-style entities (50,000 triples), identical N-Triples file loaded into all three engines.

## Methodology & Storage Model

This is the **disk-backed/persistent-storage** sibling of `RESULTS.md` (the pure in-memory comparison). All three engines were benchmarked over the SPARQL 1.1 HTTP Protocol using **byte-identical SPARQL query text** against a **byte-identical dataset**. Each query was run with a warm-up pass (discarded) before N timed iterations.

**Storage model per engine** (this matters — see below):

| Engine | Storage model | Notes |
|---|---|---|
| **Nova** | `RingStore::open(dir)` — WAL-backed | Every `insert()` is durably logged (fsync-per-write) to a write-ahead log before being applied in memory; periodic `compact()` merges the delta into an ε-serde snapshot on disk. See `CLAUDE.md` item 1 for the full on-disk design. |
| **Oxigraph** | `serve --location <dir>` — RocksDB-backed | Oxigraph's own default/production persistent storage mode (`oxrocksdb-sys`). |
| **QLever** | Memory-mapped disk index (mmap) | Unchanged from the in-memory comparison — QLever has no other mode. A warm-up pass ensures the OS page cache holds the working set resident before timed measurements. |

**Memory usage** is reported as *physical footprint* for Nova/QLever (macOS `vmmap -summary <pid>`'s `Physical footprint:` line — falls back to `ps -o rss` on platforms without `vmmap`) and container memory for Oxigraph (`docker stats`). See `README.md`/`CLAUDE.md` for the full rationale behind this choice over raw `ps -o rss`.

**On-disk footprint** is measured via `du -sk` on each engine's data directory after the query phase completes (includes WAL + snapshot files for Nova, the full RocksDB directory for Oxigraph, and all QLever index/permutation files).

**CPU usage** is sampled every ~0.3s throughout each engine's query phase and averaged. Values are percent of one CPU core.


## Dataset Load Time

Wall-clock time to load the identical N-Triples dataset and become ready to serve queries. For Nova this includes WAL-logging every triple (fsync-per-write) plus a `compact()` pass — necessarily slower than the in-memory `bulk_load()` path measured in `RESULTS.md`. For Oxigraph this is the HTTP bulk-load POST into the RocksDB-backed store. For QLever this is the same `qlever-index` build step as the in-memory comparison (QLever's index is always disk-based).

| Engine | Load time |
|---|---|
| Nova (--location) | 1.06 s |
| Oxigraph (--location) | 1.64 s |
| QLever (mmap, warmed) | 0.40 s |

## Memory Usage (Physical Footprint)

| Engine | Memory | Storage model |
|---|---|---|
| Nova (--location) | 18.9 MiB | WAL-backed heap (recovered/compacted state resident) |
| Oxigraph (--location) | 32.71MiB | RocksDB-backed (block cache + heap) |
| QLever (mmap, warmed) | 17.0 MiB | Incl. memory-mapped index pages |

## On-Disk Footprint

`du -sk` on each engine's data directory after the query phase (WAL + snapshot for Nova, full RocksDB dir for Oxigraph, all index/permutation files for QLever).

| Engine | On-disk size |
|---|---|
| Nova (--location) | 0.1 MiB |
| Oxigraph (--location) | 0.2 MiB |
| QLever (mmap, warmed) | 0.4 MiB |

## CPU Usage (average % of one core during query phase)

| Engine | Avg CPU % |
|---|---|
| Nova (--location) | 3.8% |
| Oxigraph (--location) | 7.0% |
| QLever (mmap, warmed) | 5.4% |

## Latency Results (milliseconds, HTTP round-trip via curl)

One sub-section per query, with each engine as a column and each percentile (p50, p95) as a row.

### scan

| Metric | Nova (--location, WAL-backed) | Oxigraph (--location, RocksDB-backed) | QLever (mmap, warmed) |
|---|---|---|---|
| p50 (ms) | 3.08 | 8.69 | 5.05 |
| p95 (ms) | 3.19 | 11.72 | 5.13 |

### 2join

| Metric | Nova (--location, WAL-backed) | Oxigraph (--location, RocksDB-backed) | QLever (mmap, warmed) |
|---|---|---|---|
| p50 (ms) | 0.61 | 2.05 | 1.21 |
| p95 (ms) | 0.66 | 11.33 | 1.30 |

### feature_lookup

| Metric | Nova (--location, WAL-backed) | Oxigraph (--location, RocksDB-backed) | QLever (mmap, warmed) |
|---|---|---|---|
| p50 (ms) | 0.49 | 1.11 | 0.82 |
| p95 (ms) | 0.58 | 1.16 | 0.84 |

### star_with_features

| Metric | Nova (--location, WAL-backed) | Oxigraph (--location, RocksDB-backed) | QLever (mmap, warmed) |
|---|---|---|---|
| p50 (ms) | 1.30 | 3.20 | 2.41 |
| p95 (ms) | 1.38 | 5.92 | 2.66 |

### path_2hop

| Metric | Nova (--location, WAL-backed) | Oxigraph (--location, RocksDB-backed) | QLever (mmap, warmed) |
|---|---|---|---|
| p50 (ms) | 26.54 | 52.59 | 53.04 |
| p95 (ms) | 28.37 | 54.28 | 53.44 |

### triangle

| Metric | Nova (--location, WAL-backed) | Oxigraph (--location, RocksDB-backed) | QLever (mmap, warmed) |
|---|---|---|---|
| p50 (ms) | 14.01 | 43.91 | 20.12 |
| p95 (ms) | 17.31 | 48.22 | 20.26 |

## Raw per-query summary (mean, stddev, n)

One sub-section per query, with each engine as a column and each statistic (n, mean, stddev, min, max) as a row.

### scan

| Metric | Nova (--location, WAL-backed) | Oxigraph (--location, RocksDB-backed) | QLever (mmap, warmed) |
|---|---|---|---|
| n | 5 | 5 | 5 |
| mean (ms) | 3.06 | 8.54 | 5.01 |
| stddev (ms) | 0.12 | 2.57 | 0.15 |
| min (ms) | 2.88 | 6.05 | 4.76 |
| max (ms) | 3.21 | 12.28 | 5.15 |

### 2join

| Metric | Nova (--location, WAL-backed) | Oxigraph (--location, RocksDB-backed) | QLever (mmap, warmed) |
|---|---|---|---|
| n | 5 | 5 | 5 |
| mean (ms) | 0.60 | 4.23 | 1.20 |
| stddev (ms) | 0.06 | 5.26 | 0.10 |
| min (ms) | 0.51 | 1.48 | 1.03 |
| max (ms) | 0.66 | 13.62 | 1.32 |

### feature_lookup

| Metric | Nova (--location, WAL-backed) | Oxigraph (--location, RocksDB-backed) | QLever (mmap, warmed) |
|---|---|---|---|
| n | 5 | 5 | 5 |
| mean (ms) | 0.51 | 1.10 | 0.82 |
| stddev (ms) | 0.05 | 0.05 | 0.02 |
| min (ms) | 0.47 | 1.02 | 0.79 |
| max (ms) | 0.60 | 1.18 | 0.84 |

### star_with_features

| Metric | Nova (--location, WAL-backed) | Oxigraph (--location, RocksDB-backed) | QLever (mmap, warmed) |
|---|---|---|---|
| n | 5 | 5 | 5 |
| mean (ms) | 1.31 | 3.83 | 2.47 |
| stddev (ms) | 0.05 | 1.65 | 0.15 |
| min (ms) | 1.26 | 2.39 | 2.31 |
| max (ms) | 1.40 | 6.17 | 2.69 |

### path_2hop

| Metric | Nova (--location, WAL-backed) | Oxigraph (--location, RocksDB-backed) | QLever (mmap, warmed) |
|---|---|---|---|
| n | 5 | 5 | 5 |
| mean (ms) | 27.09 | 52.17 | 53.09 |
| stddev (ms) | 1.06 | 2.22 | 0.27 |
| min (ms) | 25.98 | 49.53 | 52.78 |
| max (ms) | 28.47 | 54.29 | 53.52 |

### triangle

| Metric | Nova (--location, WAL-backed) | Oxigraph (--location, RocksDB-backed) | QLever (mmap, warmed) |
|---|---|---|---|
| n | 5 | 5 | 5 |
| mean (ms) | 15.06 | 44.91 | 20.11 |
| stddev (ms) | 1.65 | 2.42 | 0.18 |
| min (ms) | 13.95 | 43.28 | 19.82 |
| max (ms) | 17.73 | 49.18 | 20.27 |

