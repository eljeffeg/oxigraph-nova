# External Comparative Benchmark

This directory contains a harness that benchmarks Nova's two in-memory
backends — **LoudsStore** (default production) and **RingStore** (pilot) —
against external SPARQL engines on an identical synthetic BSBM-style dataset
with identical SPARQL queries:

- [Oxigraph](https://github.com/oxigraph/oxigraph)
- [QLever](https://github.com/ad-freiburg/qlever)
- [Fluree](https://github.com/fluree/server) (Docker `fluree/server`)
- [RDFox](https://www.oxfordsemantic.tech/product) (optional; requires license)

| Report | Command | Engines |
|--------|---------|---------|
| [`RESULTS_MEM.md`](./RESULTS_MEM.md) | `./run_comparison.sh` | Nova (louds), Nova (ring, Huffman C_p default), Oxigraph, QLever, Fluree [+ RDFox if licensed] |
| [`RESULTS_DISK.md`](./RESULTS_DISK.md) | `./run_comparison.sh --disk` | Nova (louds), Oxigraph, QLever, Fluree |

Nova (ring) is mem-only (no WAL / `--location` yet), so `--disk` is louds-only.
RDFox is mem-only in this harness (skipped with `--disk`).

Legacy wrappers still work:

- `run_comparison_mem.sh` → `run_comparison.sh`
- `run_comparison_disk.sh` → `run_comparison.sh --disk`

### Ring C_p (Huffman)

Plain `./run_comparison.sh` builds Nova (ring) with **Huffman C_p** (`--features ring-backend`). You do **not** need `NOVA_RING_HUFFMAN=1`.

```bash
./benches/external/run_comparison.sh                 # louds + ring (Huffman) + externals
./benches/external/run_comparison.sh --backends=ring # ring only, still Huffman
NOVA_RING_HUFFMAN=0 ./benches/external/run_comparison.sh --backends=ring  # Qwt A/B only
```

## Why this exists

Nova's internal Criterion benchmarks (`benches/bsbm_large.rs`,
`benches/wikidata_slice.rs`) only compare Nova's own stores against each
other — there's no external system in the loop. This harness closes that
gap by running the *same* dataset and *same* query text against real,
independently maintained engines over the standard SPARQL 1.1 HTTP Protocol.

It also makes the louds-vs-ring tradeoff visible side-by-side with external
baselines, instead of burying backend choice in a single "Nova" column.

## Storage-model fairness (in-memory)

This is the single most important methodological detail, so it's called out
here and again at the top of every generated `RESULTS_MEM.md`:

| Engine | Storage model in this benchmark |
|--------|----------------------------------|
| **Nova (louds)** | Pure in-process heap (`LoudsStore` — LOUDS + LFTJ). Default production in-memory backend. |
| **Nova (ring)** | Pure in-process heap (`RingStore` pilot — cyclic QWT + **Huffman C_p** product default). Built with `--features ring-backend`, started with `--backend ring`. Mem-only. Plain QWT A/B: `NOVA_RING_HUFFMAN=0`. |
| **Oxigraph** | `serve` run **without** `--location` → pure in-memory storage (not its default RocksDB-backed mode) |
| **QLever** | Memory-mapped disk index — QLever has no in-memory-only mode. A mandatory warm-up pass is run before every timed measurement so the OS page cache holds the working set resident, giving steady-state RAM-speed reads, consistent with how QLever's own published benchmarks are run. |
| **Fluree** | Ephemeral container FS (`fluree/server`, no host volume). Default file storage lives inside the container and is destroyed with it — functionally in-memory for this bench. SPARQL is connection-scoped; the harness injects `FROM <ledger>` into each query. |
| **RDFox** | In-memory datastore via sandbox/endpoint (`par-complex-nn`). **Optional:** requires a valid `RDFox.lic` (not the evaluation EULA text shipped under `research/rdfox/evaluation_license.txt`). |

CSV engine IDs: `nova-louds`, `nova-ring`, `oxigraph`, `qlever`, `fluree`, `rdfox`
(legacy `nova` rows are still accepted by the report generator as louds).

Nova backends are run **sequentially** (not concurrently) so RSS/CPU samples
are not distorted by two Nova processes competing for RAM/CPU. Each backend is
an **independent fresh process** measured in its own phase (start → load →
warm-up → timed queries → resource sample → kill) — not a backend-flag flip
inside one long-running `nova_serve`. Oxigraph, QLever, Fluree (and RDFox when
present) stay up for the whole query phase.

**Latency reporting:** headline comparisons use **medians (p50)** (p95 for
tails). Within-process stddev can be large on some shapes (e.g. Ring
`path_2hop` ~66 ms stddev vs LOUDS ~23 ms); future optimization runs should keep
medians, enough timed rounds after warm-up, and may add process-level
repetitions on top of within-process iterations.

Memory is reported as *physical footprint* for Nova/QLever/RDFox — macOS
`vmmap -summary <pid>`'s `Physical footprint:` line, not raw `ps -o rss`.
This distinction matters: on macOS, `ps` RSS includes allocator-retained-but
-freed memory (`libmalloc` keeps large freed regions mapped for fast reuse
rather than immediately `munmap`-ing them back to the OS) and was observed
to vary 10x+ (30-300+ MB) run-to-run for the *identical* process/workload
with zero code changes.
`vmmap`'s physical footprint is the same figure macOS's Activity Monitor
and the kernel's own memory accounting report, and is stable/reproducible
run-to-run, unlike `ps` RSS. On non-macOS platforms (no `vmmap` available),
the harness falls back to `ps -o rss` automatically. Oxigraph and Fluree are
measured via `docker stats` (container memory), since the container boundary
makes `vmmap` inapplicable there. QLever's figure necessarily includes resident
memory-mapped index pages — this is noted explicitly in the results rather
than left implicit.

## Prerequisites

- Rust toolchain (workspace default, see `rust-toolchain.toml`)
- Docker (for Oxigraph `oxigraph/oxigraph:latest` and Fluree `fluree/server:latest`)
- QLever native binaries (`qlever-index`, `qlever-server`) — point
  `QLEVER_BIN_DIR` at the directory containing them
- `jq`, `curl`, `python3` (stdlib only)
- **RDFox (optional):** binary at `research/rdfox/RDFox` (or `RDFOX_BIN`) and a
  valid license key at `research/rdfox/RDFox.lic` or `~/.RDFox/RDFox.lic`
  (or `RDFOX_LICENSE`). The bundled `evaluation_license.txt` is an EULA, **not**
  a key — without a real `.lic` the harness skips RDFox automatically (unless
  `--with-rdfox`, which fails).

## Running

```bash
# In-memory (default): Nova louds + ring + Oxigraph + QLever + Fluree [+ RDFox]
./benches/external/run_comparison.sh [ENTITIES] [ITERS] [WARMUP]
# Defaults: ENTITIES=50000 ITERS=10 WARMUP=3 → RESULTS_MEM.md

# Disk-backed: Nova (louds --location) + Oxigraph + QLever + Fluree
./benches/external/run_comparison.sh --disk [ENTITIES] [ITERS] [WARMUP]
# Defaults: ENTITIES=500000 ITERS=10 WARMUP=3 → RESULTS_DISK.md

# Optional flags:
#   --backends=both|louds|ring   (mem only; default both)
#   --no-fluree                  skip Fluree
#   --no-rdfox                   skip RDFox even if license present
#   --with-rdfox                 require RDFox (fail if no license/binary)
#   NOVA_BACKENDS=...            same as --backends
#   QUERY_TIMEOUT_S=60
#   QLEVER_BIN_DIR=/path/to/qlever/build
#   RDFOX_BIN=path/to/RDFox
#   RDFOX_LICENSE=path/to/RDFox.lic
#   RDFOX_ROLE / RDFOX_PASSWORD  (default guest/guest)
```

### Quick examples

```bash
# Full mem comparison
./benches/external/run_comparison.sh

# Faster mem smoke (skip Fluree)
./benches/external/run_comparison.sh --no-fluree 2000 5 2

# Mem, louds only
./benches/external/run_comparison.sh --backends=louds 2000 5 2

# Disk smoke
./benches/external/run_comparison.sh --disk 2000 5 2

# Old names still work
./benches/external/run_comparison_mem.sh 2000 5 2
./benches/external/run_comparison_disk.sh 2000 5 2
```

This will:
1. Build `gen_dataset` and the needed `nova_serve` binaries in release mode.
2. Generate (or reuse, on disk) a synthetic BSBM-style N-Triples dataset.
3. Build a QLever index.
4. Start Oxigraph + QLever (+ Fluree/RDFox when enabled); run Nova backend(s)
   as independent fresh processes.
5. Warm-up, correctness check, then N timed iterations via `curl`.
6. Measure physical footprint / container memory / CPU (and on-disk size with `--disk`).
7. Generate `RESULTS_MEM.md` or `RESULTS_DISK.md` (SVG charts off by default; pass `--charts`).

Raw timings: `raw_results.csv` (mem) or `raw_results_disk.csv` (disk).

### Charts (optional)

SVG charts are **off by default**. Pass `--charts` to write them under `charts/`
and embed them in the Markdown report:

```bash
./benches/external/run_comparison.sh --charts
# or when regenerating from an existing CSV:
python3 benches/external/generate_report.py ... --charts
```

| Report | Chart directory |
|--------|-----------------|
| `RESULTS_MEM.md` | `charts/mem/` |
| `RESULTS_DISK.md` | `charts/disk/` |

Palette: Nova (louds) blue, Nova (ring) purple, Oxigraph red, QLever green,
Fluree orange, RDFox cyan. All charts are labeled **lower is better**.

## Disk-backed mode (`--disk`)

| Engine | Storage model |
|--------|---------------|
| **Nova (louds)** | `nova_serve --location <dir>` — WAL + snapshot. CSV id: `nova-louds` |
| **Oxigraph** | `serve --location <dir>` — RocksDB |
| **QLever** | mmap disk index (unchanged) |
| **Fluree** | `fluree/server --storage-path` with host volume mount |

Ring and RDFox are not included on disk in this harness.

## Fluree notes

- Endpoint: `http://localhost:<port>/v1/fluree/query`
- Every SPARQL query is rewritten to inject `FROM <bench:main>` before `WHERE`
  (Fluree has no default dataset on the connection).
- Mem: container with no host volume (ephemeral).
- Disk: `-v <host>:/var/lib/fluree --storage-path /var/lib/fluree`.

## RDFox notes

- Binary default: `research/rdfox/RDFox` (v7.6 in-tree).
- License default: `research/rdfox/RDFox.lic`, else `~/.RDFox/RDFox.lic`.
  Auto-skipped when missing or when the file looks like EULA text rather than a key.
- Started in `sandbox` mode with `-port 12110` (mem); data store `parallel-nn`;
  SPARQL at `/datastores/bench/sparql` with basic auth (`guest`/`guest` by default).
  Stdin is kept open so the sandbox process does not exit after `endpoint start`.
- Use `--with-rdfox` to fail hard if license/binary is unavailable; use
  `--no-rdfox` to skip even when present.
