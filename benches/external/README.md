
# External Comparative Benchmark: Nova vs Oxigraph vs QLever

This directory contains a harness that benchmarks Nova's `RingStore`
(Ring + LFTJ) against two external, independently-developed SPARQL engines
— [Oxigraph](https://github.com/oxigraph/oxigraph) and
[QLever](https://github.com/ad-freiburg/qlever) — on an identical synthetic
BSBM-style dataset with identical SPARQL queries.

See [`RESULTS.md`](./RESULTS.md) for the latest generated in-memory report,
and [`RESULTS_DISK.md`](./RESULTS_DISK.md) for the disk-backed (persistent
storage) variant.

## Why this exists


Nova's internal Criterion benchmarks (`benches/bsbm_large.rs`,
`benches/wikidata_slice.rs`) only compare Nova's own `RingStore` against its
own `MemoryStore` baseline — there's no external system in the loop. This
harness closes that gap by running the *same* dataset and *same* query text
against real, independently maintained engines over the standard SPARQL 1.1
HTTP Protocol.

## Storage-model fairness

This is the single most important methodological detail, so it's called out
here and again at the top of every generated `RESULTS.md`:

| Engine   | Storage model in this benchmark |
|----------|----------------------------------|
| Nova     | Pure in-process heap memory (no disk option exists at all) |
| Oxigraph | `serve` run **without** `--location` → pure in-memory storage (not its default RocksDB-backed mode) |
| QLever   | Memory-mapped disk index — QLever has no in-memory-only mode. A mandatory warm-up pass is run before every timed measurement so the OS page cache holds the working set resident, giving steady-state RAM-speed reads, consistent with how QLever's own published benchmarks are run. |

Memory is reported as *physical footprint* for Nova/QLever — macOS
`vmmap -summary <pid>`'s `Physical footprint:` line, not raw `ps -o rss`.
This distinction matters: on macOS, `ps` RSS includes allocator-retained-but
-freed memory (`libmalloc` keeps large freed regions mapped for fast reuse
rather than immediately `munmap`-ing them back to the OS) and was observed
to vary 10x+ (30-300+ MB) run-to-run for the *identical* process/workload
with zero code changes — see `CLAUDE.md`'s "RSS investigation: the
'anomalous' 142 MiB reading, explained" section for the full writeup.
`vmmap`'s physical footprint is the same figure macOS's Activity Monitor
and the kernel's own memory accounting report, and is stable/reproducible
run-to-run, unlike `ps` RSS. On non-macOS platforms (no `vmmap` available),
the harness falls back to `ps -o rss` automatically. Oxigraph is measured
via `docker stats` (container memory), since the container boundary makes
`vmmap` inapplicable there. QLever's figure necessarily includes resident
memory-mapped index pages — this is noted explicitly in the results rather
than left implicit.


## Prerequisites

- Rust toolchain (workspace default, see `rust-toolchain.toml`)
- Docker (for the Oxigraph `oxigraph/oxigraph:latest` image — native build
  was attempted first but failed on a missing nested `oxrocksdb-sys`
  submodule; Docker is the supported fallback)
- QLever native binaries (`qlever-index`, `qlever-server`) built somewhere
  on disk — point `QLEVER_BIN_DIR` at the directory containing them
- `jq`, `curl`, `python3` (stdlib only, no extra pip packages needed)

## Running

```bash
QLEVER_BIN_DIR=/path/to/qlever/build \
  ./benches/external/run_comparison.sh [ENTITIES] [ITERS] [WARMUP]

# Defaults: ENTITIES=50000 ITERS=30 WARMUP=5
```

This will:
1. Build `gen_dataset` and `nova_serve` in release mode.
2. Generate a synthetic BSBM-style N-Triples dataset (`/tmp/oxigraph-nova-bench/dataset.nt`) plus a fixed SPARQL query set (`dataset.queries.json`).
3. Build a QLever index from that dataset.
4. Start all three engines (Nova in-process, Oxigraph via Docker, QLever natively), loading the identical dataset into each.
5. Run each query with a warm-up pass, a correctness check (expected vs. actual result-row count), then N timed iterations via `curl`.
6. Measure RSS/container memory for each engine.
7. Generate `RESULTS.md` via `generate_report.py`.

Raw per-request timings are written to `raw_results.csv` for further
analysis if needed.

## Scaling up

The default `N=50,000` entities (1.25M triples) matches Nova's existing
internal `bsbm_large.rs` benchmark scale. QLever's own published benchmarks
go up to 500M (DBLP) and 8B (Wikidata Truth) triples — re-running this
harness with a larger `ENTITIES` value (and enough RAM/disk) is a natural
next step once the small-scale numbers are validated.

---

## Disk-backed variant (`run_comparison_disk.sh`)

Alongside the in-memory harness above, `run_comparison_disk.sh` exercises
each engine's **persistent, disk-backed** storage mode instead — this is
the more realistic configuration for production deployments, since restart
durability matters. Results are written to
[`RESULTS_DISK.md`](./RESULTS_DISK.md) / `raw_results_disk.csv`.

### Storage-model fairness (disk-backed variant)

| Engine   | Storage model in this benchmark |
|----------|----------------------------------|
| Nova     | `nova_serve --location <dir>` — persistent `RingStore`, WAL + generation-numbered snapshot on disk (see `crates/nova-storage-common/src/wal.rs`) |
| Oxigraph | `serve --location <dir>` — its default/production RocksDB-backed mode |
| QLever   | Memory-mapped disk index, unchanged from the in-memory comparison (QLever has no other mode) |

Each engine is now running in its own natural persistent configuration, so
this variant is arguably the fairer real-world comparison of the two — but
see the critical caveat below before drawing conclusions from load time.

### ✅ Resolved: Nova's `--location --data` bulk-load path (formerly fsync-per-write)

**Update:** the fsync-per-write bulk-load bottleneck described below has
been fixed. `nova_serve --location <dir> --data <dataset.nt>` now calls
`RingStore::bulk_load()`, which bypasses the WAL entirely for the initial
load — it builds the Ring index directly in memory and commits it via a
single atomic snapshot + MANIFEST swap (see `RingStore::commit_compaction`).
There is no per-triple (or even per-batch) `fsync` on this path at all; the
only disk I/O is one sequential snapshot write plus one small MANIFEST
write, both at the end.

Measured on this machine: **1,250,000 triples loaded (parsed, interned,
indexed, and durably committed to disk) in 1.31s** — compare to the
previous ~210.97s for just 50,000 triples (a >4,000x improvement in
triples/second, and no longer scaling linearly with fsync count at all).
At this rate, a 12.5M-triple load is expected to take on the order of
seconds, not the ~14.6 hours the old fsync-per-write path would have taken.

Ongoing writes after the initial load (`INSERT DATA`/`extend()` with
multiple triples, and single-quad `insert()`) still go through the WAL for
durability, but:
- Multi-triple `extend()` calls (e.g. SPARQL `INSERT DATA` with several
  triples) now acquire the store's lock **once** for the whole batch and
  issue a **single `fsync`** for all of that batch's WAL records (see
  `WalWriter::append_batch` in `crates/nova-storage-common/src/wal.rs`),
  instead of one `fsync` per triple.
- A configurable `SyncPolicy` (see `oxigraph_nova_storage_ring::SyncPolicy`,
  and `nova_serve --sync-interval-ms <n>`) lets a deployment trade the
  default `Always` (fsync every write, zero data loss on crash) for
  `Interval(n)` — a background thread fsyncs the WAL every `n`
  milliseconds ("group commit"), removing fsync latency from the write path
  entirely at the cost of a bounded durability window (writes acknowledged
  since the last flush can be lost on a crash, though never corrupted —
  `wal::replay`'s existing torn-tail handling covers this exactly the same
  way it covers any other incomplete write).

Because the disk-backed report may still use a different dataset scale than
the in-memory report (depending on when each was last regenerated), the two
reports are not necessarily directly comparable in absolute latency terms —
only the relative engine-to-engine comparisons *within* each report are
guaranteed meaningful.


### Running (disk-backed)

```bash
QLEVER_BIN_DIR=/path/to/qlever/build \
  ./benches/external/run_comparison_disk.sh [ENTITIES] [ITERS] [WARMUP]

# Defaults: ENTITIES=50000 ITERS=30 WARMUP=5
# Recommended for a quick run given the caveat above: ENTITIES=2000
```

This follows the same steps as `run_comparison.sh`, with two additions:
each engine's data directory is wiped fresh before loading (to ensure a
clean/comparable on-disk footprint), and after the query phase each
engine's on-disk footprint is measured via `du -sk` and included in the
report alongside load time, memory, CPU, and query latency.

