# Comparative Benchmark (Disk-Backed): Nova vs Oxigraph vs QLever vs Fluree

Dataset: 50,000 synthetic BSBM-style entities (1,250,000 triples), identical N-Triples file loaded into all 5 engines. RDFox is mem-only in this harness and is not included on disk.

## Methodology & Storage Model

This is the **disk-backed/persistent-storage** sibling of `RESULTS_MEM.md` (the pure in-memory comparison). All engines were benchmarked over the SPARQL 1.1 HTTP Protocol using **byte-identical SPARQL query text** against a **byte-identical dataset**. Each query was run with a warm-up pass (discarded) before N timed iterations.

**Storage model per engine** (this matters — see below):

| Engine | Storage model | Notes |
|---|---|---|
| **Nova (louds)** | `LoudsStore::open(dir)` — WAL-backed | Every `insert()` is durably logged (fsync-per-write) to a write-ahead log before being applied in memory; periodic `compact()` merges the delta into an on-disk snapshot. CSV id: `nova-louds`. |
| **Nova (ring)** | `RingStore::open(dir)` — WAL-backed | Same product surface as LOUDS: `--location` WAL + snapshot via `nova_serve --backend ring`. CSV id: `nova-ring`. |
| **Oxigraph** | `serve --location <dir>` — RocksDB-backed | Oxigraph's own default/production persistent storage mode (`oxrocksdb-sys`). |
| **QLever** | Memory-mapped disk index (mmap) | Unchanged from the in-memory comparison — QLever has no other mode. A warm-up pass ensures the OS page cache holds the working set resident before timed measurements. |
| **Fluree** | `fluree/server --storage-path` (host volume) | File-backed persistent ledger. Memory footprint is **dynamic (not measured)**: LeafletCache/import budgets are host-relative and not comparable to pure-heap engines. SPARQL is connection-scoped; the harness injects `FROM <ledger>` into each query. |
| **RDFox** | N/A (In-memory only) | RDFox is not disk-backed and thus excluded from this benchmark. |

**Memory usage** is reported as *physical footprint* for Nova/QLever (macOS `vmmap -summary <pid>`'s `Physical footprint:` line — falls back to `ps -o rss` on platforms without `vmmap`) and container memory for Oxigraph/Fluree (`docker stats`). See `README.md` for the full rationale behind this choice over raw `ps -o rss`.

**On-disk footprint** is measured via `du -sk` on each engine's data directory after the query phase completes (includes WAL + snapshot files for Nova (louds), WAL + snapshot files for Nova (ring), the full RocksDB directory for Oxigraph, all QLever index/permutation files, and Fluree storage-path contents).

**CPU usage** is sampled every ~0.3s throughout each engine's query phase and averaged. Values are percent of one CPU core.

**Process isolation (Nova backends).** Nova (louds) and Nova (ring) are launched as **independent fresh processes** and measured in **separate phases** (start → load → warm-up → timed queries → resource sample → kill), not selected by flipping a backend flag inside one long-running process.


## Dataset Load Time

Wall-clock time to load the identical N-Triples dataset and become ready to serve queries. For Nova (louds) this includes WAL-logging every triple (fsync-per-write) plus a `compact()` pass — necessarily slower than the in-memory `bulk_load()` path measured in `RESULTS_MEM.md`. For Nova (ring) this is parse + `bulk_load()` into a WAL-backed `--location` store (same crash-safe snapshot commit path as LOUDS). For Oxigraph this is the HTTP bulk-load POST into the RocksDB-backed store. For QLever this is the same `qlever-index` build step as the in-memory comparison (QLever's index is always disk-based). For Fluree this is create-ledger + N-Triples insert into `--storage-path`.

| Engine | Load time |
|---|---|
| Nova (louds) | 2.17 s |
| Nova (ring) | 4.22 s |
| Oxigraph | 11.17 s |
| QLever | 3.10 s |
| Fluree | 6.18 s |

![Dataset load time by engine (lower is better)](charts/disk/load_time.svg)

## Memory Usage (Physical Footprint)

| Engine | Memory | Storage model |
|---|---|---|
| Nova (louds) | 75.5 MiB | WAL-backed heap (recovered/compacted state resident) |
| Nova (ring) | 70.7 MiB | WAL-backed heap (recovered/compacted state resident) |
| Oxigraph | 598.9MiB | RocksDB-backed (block cache + heap) |
| QLever | 88.7 MiB | Incl. memory-mapped index pages |
| Fluree | dynamic (not measured) | File-backed ledger; cache/import budgets host-relative |

![Memory usage by engine (lower is better)](charts/disk/memory.svg)

## On-Disk Footprint

`du -sk` on each engine's data directory after the query phase (WAL + snapshot for both Nova backends, full RocksDB dir for Oxigraph, all index/permutation files for QLever, Fluree storage-path for Fluree).

| Engine | On-disk size |
|---|---|
| Nova (louds) | 18.9 MiB |
| Nova (ring) | 7.3 MiB |
| Oxigraph | 416.5 MiB |
| QLever | 4.2 MiB |
| Fluree | 10.8 MiB |

![On-disk footprint by engine (lower is better)](charts/disk/disk.svg)

## CPU Usage (average % of one core during query phase)

| Engine | Avg CPU % |
|---|---|
| Nova (louds) | 38.9% |
| Nova (ring) | 43.9% |
| Oxigraph | 97.3% |
| QLever | 60.8% |
| Fluree | 94.3% |

![CPU usage by engine (lower is better)](charts/disk/cpu.svg)

## Latency Results (milliseconds, HTTP round-trip via curl)

One sub-section per query, with each engine as a column and each percentile (p50, p95) as a row. Charts use p50 latency (lower is better). `path_2hop` and `triangle` are charted separately — their latencies are orders of magnitude higher and would crush the scale of the other queries.

![p50 latency by query and engine — light queries (lower is better)](charts/disk/latency_p50_overview.svg)

![p50 latency for path_2hop and triangle (lower is better)](charts/disk/latency_p50_heavy.svg)

### scan

| Metric | Nova (louds) | Nova (ring) | Oxigraph | QLever | Fluree |
|---|---|---|---|---|---|
| p50 (ms) | 43.93 | 38.20 | 163.65 | 96.14 | 111.71 |
| p95 (ms) | 45.41 | 43.00 | 175.54 | 97.95 | 131.67 |

![scan p50 latency (lower is better)](charts/disk/latency_p50_scan.svg)

### 2join

| Metric | Nova (louds) | Nova (ring) | Oxigraph | QLever | Fluree |
|---|---|---|---|---|---|
| p50 (ms) | 1.73 | 1.32 | 8.00 | 2.84 | 7.58 |
| p95 (ms) | 1.80 | 1.52 | 12.79 | 3.12 | 10.23 |

![2join p50 latency (lower is better)](charts/disk/latency_p50_2join.svg)

### feature_lookup

| Metric | Nova (louds) | Nova (ring) | Oxigraph | QLever | Fluree |
|---|---|---|---|---|---|
| p50 (ms) | 0.65 | 0.92 | 3.15 | 1.13 | 2.77 |
| p95 (ms) | 0.68 | 1.07 | 3.28 | 1.27 | 3.30 |

![feature_lookup p50 latency (lower is better)](charts/disk/latency_p50_feature_lookup.svg)

### star_with_features

| Metric | Nova (louds) | Nova (ring) | Oxigraph | QLever | Fluree |
|---|---|---|---|---|---|
| p50 (ms) | 13.17 | 14.34 | 43.53 | 38.70 | 45.84 |
| p95 (ms) | 13.55 | 15.07 | 48.49 | 40.58 | 47.21 |

![star_with_features p50 latency (lower is better)](charts/disk/latency_p50_star_with_features.svg)

### path_2hop

| Metric | Nova (louds) | Nova (ring) | Oxigraph | QLever | Fluree |
|---|---|---|---|---|---|
| p50 (ms) | 436.52 | 455.72 | 1974.20 | 1284.07 | 1345.46 |
| p95 (ms) | 440.28 | 466.69 | 2039.21 | 1292.37 | 1456.20 |

![path_2hop p50 latency (lower is better)](charts/disk/latency_p50_path_2hop.svg)

### triangle

| Metric | Nova (louds) | Nova (ring) | Oxigraph | QLever | Fluree |
|---|---|---|---|---|---|
| p50 (ms) | 213.91 | 212.47 | 1740.46 | 432.21 | 638.65 |
| p95 (ms) | 218.01 | 217.64 | 1789.18 | 502.77 | 758.29 |

![triangle p50 latency (lower is better)](charts/disk/latency_p50_triangle.svg)

## Raw per-query summary (mean, stddev, n)

One sub-section per query, with each engine as a column and each statistic (n, mean, stddev, min, max) as a row.

### scan

| Metric | Nova (louds) | Nova (ring) | Oxigraph | QLever | Fluree |
|---|---|---|---|---|---|
| n | 10 | 10 | 10 | 10 | 10 |
| mean (ms) | 44.08 | 39.66 | 162.59 | 96.26 | 114.87 |
| stddev (ms) | 0.92 | 2.39 | 9.12 | 1.12 | 9.83 |
| min (ms) | 42.95 | 37.36 | 151.17 | 95.02 | 107.03 |
| max (ms) | 45.54 | 43.04 | 179.43 | 98.13 | 141.12 |

### 2join

| Metric | Nova (louds) | Nova (ring) | Oxigraph | QLever | Fluree |
|---|---|---|---|---|---|
| n | 10 | 10 | 10 | 10 | 10 |
| mean (ms) | 1.62 | 1.36 | 8.72 | 2.86 | 8.00 |
| stddev (ms) | 0.18 | 0.12 | 2.58 | 0.20 | 1.19 |
| min (ms) | 1.34 | 1.22 | 6.94 | 2.62 | 7.16 |
| max (ms) | 1.83 | 1.53 | 15.89 | 3.12 | 10.66 |

### feature_lookup

| Metric | Nova (louds) | Nova (ring) | Oxigraph | QLever | Fluree |
|---|---|---|---|---|---|
| n | 10 | 10 | 10 | 10 | 10 |
| mean (ms) | 0.65 | 0.93 | 3.15 | 1.16 | 2.83 |
| stddev (ms) | 0.03 | 0.09 | 0.11 | 0.08 | 0.37 |
| min (ms) | 0.59 | 0.81 | 2.94 | 1.07 | 2.27 |
| max (ms) | 0.69 | 1.08 | 3.29 | 1.31 | 3.35 |

### star_with_features

| Metric | Nova (louds) | Nova (ring) | Oxigraph | QLever | Fluree |
|---|---|---|---|---|---|
| n | 10 | 10 | 10 | 10 | 10 |
| mean (ms) | 13.21 | 14.34 | 44.64 | 39.07 | 45.98 |
| stddev (ms) | 0.28 | 0.49 | 2.57 | 0.84 | 0.80 |
| min (ms) | 12.74 | 13.70 | 41.00 | 38.30 | 44.58 |
| max (ms) | 13.61 | 15.36 | 49.12 | 40.93 | 47.43 |

### path_2hop

| Metric | Nova (louds) | Nova (ring) | Oxigraph | QLever | Fluree |
|---|---|---|---|---|---|
| n | 10 | 10 | 10 | 10 | 10 |
| mean (ms) | 435.82 | 457.22 | 1969.08 | 1282.63 | 1364.06 |
| stddev (ms) | 4.14 | 6.63 | 59.15 | 7.50 | 52.06 |
| min (ms) | 426.61 | 450.06 | 1877.71 | 1271.02 | 1326.31 |
| max (ms) | 440.96 | 467.65 | 2043.65 | 1294.34 | 1495.77 |

### triangle

| Metric | Nova (louds) | Nova (ring) | Oxigraph | QLever | Fluree |
|---|---|---|---|---|---|
| n | 10 | 10 | 10 | 10 | 10 |
| mean (ms) | 213.72 | 213.53 | 1741.17 | 445.03 | 660.31 |
| stddev (ms) | 3.01 | 2.64 | 36.80 | 38.88 | 64.30 |
| min (ms) | 209.05 | 210.88 | 1674.90 | 430.13 | 630.77 |
| max (ms) | 218.68 | 218.46 | 1803.90 | 555.44 | 842.00 |

