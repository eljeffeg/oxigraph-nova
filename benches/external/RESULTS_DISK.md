# Comparative Benchmark (Disk-Backed): Nova vs Oxigraph vs QLever

Dataset: 500,000 synthetic BSBM-style entities (12,500,000 triples), identical N-Triples file loaded into all three engines.

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
| Nova (--location) | 15.43 s |
| Oxigraph (--location) | 140.88 s |
| QLever (mmap, warmed) | 30.70 s |

## Memory Usage (Physical Footprint)

| Engine | Memory | Storage model |
|---|---|---|
| Nova (--location) | 949.0 MiB | WAL-backed heap (recovered/compacted state resident) |
| Oxigraph (--location) | 5.796GiB | RocksDB-backed (block cache + heap) |
| QLever (mmap, warmed) | 599.3 MiB | Incl. memory-mapped index pages |

## On-Disk Footprint

`du -sk` on each engine's data directory after the query phase (WAL + snapshot for Nova, full RocksDB dir for Oxigraph, all index/permutation files for QLever).

| Engine | On-disk size |
|---|---|
| Nova (--location) | 221.1 MiB |
| Oxigraph (--location) | 4129.5 MiB |
| QLever (mmap, warmed) | 55.6 MiB |

## CPU Usage (average % of one core during query phase)

| Engine | Avg CPU % |
|---|---|
| Nova (--location) | 8.3% |
| Oxigraph (--location) | 73.0% |
| QLever (mmap, warmed) | 17.0% |

## Latency Results (milliseconds, HTTP round-trip via curl)

One sub-section per query, with each engine as a column and each percentile (p50, p95) as a row.

### scan

| Metric | Nova (--location, WAL-backed) | Oxigraph (--location, RocksDB-backed) | QLever (mmap, warmed) |
|---|---|---|---|
| p50 (ms) | 592.98 | 2222.08 | 937.40 |
| p95 (ms) | 633.01 | 2346.96 | 959.58 |

### 2join

| Metric | Nova (--location, WAL-backed) | Oxigraph (--location, RocksDB-backed) | QLever (mmap, warmed) |
|---|---|---|---|
| p50 (ms) | 13.95 | 90.02 | 21.53 |
| p95 (ms) | 15.18 | 95.33 | 22.61 |

### feature_lookup

| Metric | Nova (--location, WAL-backed) | Oxigraph (--location, RocksDB-backed) | QLever (mmap, warmed) |
|---|---|---|---|
| p50 (ms) | 4.75 | 24.91 | 6.92 |
| p95 (ms) | 5.17 | 26.67 | 9.85 |

### star_with_features

| Metric | Nova (--location, WAL-backed) | Oxigraph (--location, RocksDB-backed) | QLever (mmap, warmed) |
|---|---|---|---|
| p50 (ms) | 142.27 | 569.78 | 387.75 |
| p95 (ms) | 145.98 | 614.66 | 397.11 |

### path_2hop

| Metric | Nova (--location, WAL-backed) | Oxigraph (--location, RocksDB-backed) | QLever (mmap, warmed) |
|---|---|---|---|
| p50 (ms) | 5046.39 | 36218.99 | 12951.67 |
| p95 (ms) | 5208.61 | 45461.82 | 13272.94 |

### triangle

| Metric | Nova (--location, WAL-backed) | Oxigraph (--location, RocksDB-backed) | QLever (mmap, warmed) |
|---|---|---|---|
| p50 (ms) | 2844.02 | 33457.91 | 4238.43 |
| p95 (ms) | 2897.42 | 41500.43 | 4268.89 |

## Raw per-query summary (mean, stddev, n)

One sub-section per query, with each engine as a column and each statistic (n, mean, stddev, min, max) as a row.

### scan

| Metric | Nova (--location, WAL-backed) | Oxigraph (--location, RocksDB-backed) | QLever (mmap, warmed) |
|---|---|---|---|
| n | 10 | 10 | 10 |
| mean (ms) | 595.91 | 2247.99 | 941.23 |
| stddev (ms) | 24.17 | 59.87 | 11.98 |
| min (ms) | 567.04 | 2172.49 | 933.77 |
| max (ms) | 645.15 | 2382.09 | 974.70 |

### 2join

| Metric | Nova (--location, WAL-backed) | Oxigraph (--location, RocksDB-backed) | QLever (mmap, warmed) |
|---|---|---|---|
| n | 10 | 10 | 10 |
| mean (ms) | 14.05 | 91.04 | 21.44 |
| stddev (ms) | 0.73 | 2.62 | 0.81 |
| min (ms) | 12.85 | 87.74 | 20.50 |
| max (ms) | 15.37 | 96.04 | 22.65 |

### feature_lookup

| Metric | Nova (--location, WAL-backed) | Oxigraph (--location, RocksDB-backed) | QLever (mmap, warmed) |
|---|---|---|---|
| n | 10 | 10 | 10 |
| mean (ms) | 4.67 | 24.95 | 7.50 |
| stddev (ms) | 0.39 | 1.18 | 1.48 |
| min (ms) | 4.08 | 23.52 | 6.76 |
| max (ms) | 5.30 | 27.11 | 11.61 |

### star_with_features

| Metric | Nova (--location, WAL-backed) | Oxigraph (--location, RocksDB-backed) | QLever (mmap, warmed) |
|---|---|---|---|
| n | 10 | 10 | 10 |
| mean (ms) | 141.14 | 572.76 | 386.96 |
| stddev (ms) | 3.95 | 26.91 | 7.68 |
| min (ms) | 133.92 | 540.98 | 372.26 |
| max (ms) | 146.25 | 627.62 | 401.41 |

### path_2hop

| Metric | Nova (--location, WAL-backed) | Oxigraph (--location, RocksDB-backed) | QLever (mmap, warmed) |
|---|---|---|---|
| n | 10 | 10 | 10 |
| mean (ms) | 5066.61 | 37425.91 | 12985.34 |
| stddev (ms) | 79.97 | 5618.27 | 185.37 |
| min (ms) | 5003.34 | 30026.08 | 12725.59 |
| max (ms) | 5269.82 | 45712.07 | 13286.68 |

### triangle

| Metric | Nova (--location, WAL-backed) | Oxigraph (--location, RocksDB-backed) | QLever (mmap, warmed) |
|---|---|---|---|
| n | 10 | 10 | 10 |
| mean (ms) | 2843.72 | 34562.85 | 4244.44 |
| stddev (ms) | 44.18 | 4351.47 | 15.77 |
| min (ms) | 2776.94 | 30896.43 | 4228.06 |
| max (ms) | 2909.10 | 45651.03 | 4275.55 |

