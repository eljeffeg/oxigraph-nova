#!/usr/bin/env bash
# Comparative benchmark harness: Nova (Ring+LFTJ) vs Oxigraph vs QLever.
#
# Methodology (see README.md in this directory for full rationale):
#   - Nova:      LoudsStore, always pure in-process heap memory (no disk option exists).
#   - Oxigraph:  `serve` run WITHOUT --location -> pure in-memory storage,
#                matching Nova's memory model exactly.
#   - QLever:    index is inherently memory-mapped disk files (no in-memory-only
#                mode exists). A warm-up pass is run before timed measurements
#                so steady-state queries are served from the OS page cache
#                (effectively RAM-speed), consistent with QLever's own
#                published benchmark methodology.
#
# All three engines are loaded with a byte-identical N-Triples dataset and
# queried with byte-identical SPARQL text (see gen_dataset's <out>.queries.json).
#
# CPU usage is sampled in the background (~every 0.3s) throughout the query
# phase for each engine, via `ps -o %cpu` (Nova/QLever) and
# `docker stats --format '{{.CPUPerc}}'` (Oxigraph), then averaged.
#
# MEMORY MEASUREMENT (macOS): native processes (Nova, QLever) are measured via
# `vmmap -summary <pid>`'s "Physical footprint" line, NOT `ps -o rss`. This
# matters: on macOS, `ps` RSS includes allocator-retained-but-freed memory
# (large freed regions `libmalloc` keeps mapped for fast reuse rather than
# `munmap`-ing back to the OS immediately) and can vary wildly (30-300+ MB)
# run-to-run for the identical process/workload with zero code changes.
# `vmmap -summary`'s "Physical footprint" is the same figure Activity Monitor
# and the kernel's own memory accounting report, and is stable/reliable
# across repeated runs of the same workload. On non-macOS platforms (no
# `vmmap`), this script falls back to `ps -o rss` automatically. Oxigraph
# runs in a Docker container, so its memory is still measured via
# `docker stats` (the container boundary makes `vmmap` inapplicable there).
#
# Usage:
#   ./run_comparison.sh [ENTITIES] [ITERS] [WARMUP] [RESULT_FILE]
#
# Defaults: ENTITIES=50000 ITERS=10 WARMUP=3
set -euo pipefail

ENTITIES="${1:-50000}"
ITERS="${2:-10}"
WARMUP="${3:-3}"
RESULT_FILE="${4:-RESULTS_MEM.md}"

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BENCH_DIR="/tmp/oxigraph-nova-bench"
QLEVER_DIR="/tmp/qlever_bench"
QLEVER_BIN="${QLEVER_BIN_DIR:-/Users/jgentes/Documents/Workspace/qlever/build}"
DATA="$BENCH_DIR/dataset_${ENTITIES}.nt"
QUERIES="$BENCH_DIR/dataset_${ENTITIES}.queries.json"
OUT_DIR="$ROOT/benches/external"
CSV="$OUT_DIR/raw_results.csv"
RESULTS_MD="$OUT_DIR/$RESULT_FILE"

NOVA_PORT=3030
OXIGRAPH_PORT=7878
QLEVER_PORT=7979
QLEVER_READY_URL="http://localhost:$QLEVER_PORT/?query=SELECT%20%3Fs%20WHERE%20%7B%3Fs%20%3Fp%20%3Fo%7D%20LIMIT%201"

NOVA_CPU_LOG="/tmp/nova_cpu_samples.txt"
QLEVER_CPU_LOG="/tmp/qlever_cpu_samples.txt"
OXIGRAPH_CPU_LOG="/tmp/oxigraph_cpu_samples.txt"

mkdir -p "$BENCH_DIR" "$QLEVER_DIR" "$OUT_DIR"

# --- Convert a vmmap "Physical footprint" value (e.g. "253.1M", "1.2G", "900K")
# into an integer number of KB, matching the units `ps -o rss` reports in. ---
_footprint_str_to_kb() {
  local s="$1"
  python3 -c "
s = '''$s'''.strip()
mult = {'K': 1.0, 'M': 1024.0, 'G': 1024.0 * 1024.0, 'B': 1.0 / 1024.0}
unit = s[-1] if s else ''
try:
    if unit in mult:
        val = float(s[:-1]) * mult[unit]
    else:
        val = float(s) / 1024.0
except ValueError:
    val = 0.0
print(int(val))
"
}

# --- Measure a native process's real physical memory footprint in KB. ---
# Prefers `vmmap -summary <pid>`'s "Physical footprint:" line (macOS only --
# this is the accurate, allocator-retention-immune metric; see the header
# comment above for why plain `ps -o rss` is unreliable run-to-run on
# macOS). Falls back to `ps -o rss` on any platform/failure where `vmmap`
# isn't available (e.g. Linux CI), so the script still works everywhere,
# just with the less precise metric in that case.
measure_footprint_kb() {
  local pid="$1"
  if command -v vmmap >/dev/null 2>&1; then
    local raw
    raw=$(vmmap -summary "$pid" 2>/dev/null | awk -F: '/^Physical footprint:/ {print $2; exit}' | tr -d ' ')
    if [ -n "$raw" ]; then
      _footprint_str_to_kb "$raw"
      return
    fi
  fi
  ps -o rss= -p "$pid" 2>/dev/null | tr -d ' '
}

# NOVA_BACKEND=louds (default) | ring
# ring = in-memory RingStore pilot; builds nova_serve with --features ring-backend
# and passes --backend ring. Disk harness cannot use ring (no WAL yet).
NOVA_BACKEND="${NOVA_BACKEND:-louds}"

echo "=== [1/6] Building Nova binaries (release) [NOVA_BACKEND=$NOVA_BACKEND] ==="
cd "$ROOT"
if [ "$NOVA_BACKEND" = "ring" ]; then
  cargo build --release -p oxigraph-nova-bench --bin gen_dataset \
    -p oxigraph-nova-server --features ring-backend --bin nova_serve
else
  cargo build --release -p oxigraph-nova-bench --bin gen_dataset \
    -p oxigraph-nova-server --bin nova_serve
fi


echo "=== [2/6] Generating dataset (N=$ENTITIES entities) ==="
"$ROOT/target/release/gen_dataset" --entities "$ENTITIES" --out "$DATA"

echo "=== [3/6] Building QLever index ==="
rm -f "$QLEVER_DIR"/bench.* 2>/dev/null || true
QLEVER_LOAD_START=$(date +%s.%N)
( cd "$QLEVER_DIR" && "$QLEVER_BIN/qlever-index" -i bench -f "$DATA" -F nt )
QLEVER_LOAD_END=$(date +%s.%N)
QLEVER_LOAD_S=$(awk -v a="$QLEVER_LOAD_START" -v b="$QLEVER_LOAD_END" 'BEGIN{printf "%.2f", b-a}')

NOVA_PID=""
QLEVER_PID=""
CPU_SAMPLER_PID=""



cleanup() {
  echo "=== Cleaning up servers ==="
  [ -n "$CPU_SAMPLER_PID" ] && kill "$CPU_SAMPLER_PID" 2>/dev/null || true
  [ -n "$NOVA_PID" ] && kill "$NOVA_PID" 2>/dev/null || true
  [ -n "$QLEVER_PID" ] && kill "$QLEVER_PID" 2>/dev/null || true
  docker rm -f oxigraph_bench >/dev/null 2>&1 || true
}
trap cleanup EXIT

echo "=== [4/6] Starting servers ==="

wait_ready() {
  local url=$1
  for _ in $(seq 1 60); do
    code=$(curl -s -o /dev/null -w "%{http_code}" "$url" || echo "000")
    if [[ "$code" =~ ^(200|400)$ ]]; then return 0; fi
    sleep 1
  done
  echo "Server at $url did not become ready" >&2
  return 1
}

# --- Nova: pure in-memory store (load time = parse + compact into index) ---
# LOUDS LoudsStore by default; RingStore when NOVA_BACKEND=ring.
NOVA_LOAD_START=$(date +%s.%N)
if [ "$NOVA_BACKEND" = "ring" ]; then
  "$ROOT/target/release/nova_serve" --backend ring --file "$DATA" \
    --bind "127.0.0.1:$NOVA_PORT" > /tmp/nova_serve_bench.log 2>&1 &
else
  "$ROOT/target/release/nova_serve" --file "$DATA" \
    --bind "127.0.0.1:$NOVA_PORT" > /tmp/nova_serve_bench.log 2>&1 &
fi
NOVA_PID=$!
wait_ready "http://127.0.0.1:$NOVA_PORT/sparql"
NOVA_LOAD_END=$(date +%s.%N)
NOVA_LOAD_S=$(awk -v a="$NOVA_LOAD_START" -v b="$NOVA_LOAD_END" 'BEGIN{printf "%.2f", b-a}')


# --- Oxigraph: serve WITHOUT --location => pure in-memory storage ---
# (load time = container start + HTTP bulk-load POST of the dataset)
OXIGRAPH_LOAD_START=$(date +%s.%N)
docker rm -f oxigraph_bench >/dev/null 2>&1 || true
docker run -d --name oxigraph_bench -p "$OXIGRAPH_PORT:7878" oxigraph/oxigraph:latest \
  serve --bind 0.0.0.0:7878 >/dev/null
wait_ready "http://localhost:$OXIGRAPH_PORT/sparql"
echo "  Bulk-loading dataset into Oxigraph (in-memory) via HTTP POST..."
curl -s -X POST -T "$DATA" -H "Content-Type: application/n-triples" \
  "http://localhost:$OXIGRAPH_PORT/store?default" -o /dev/null
OXIGRAPH_LOAD_END=$(date +%s.%N)
OXIGRAPH_LOAD_S=$(awk -v a="$OXIGRAPH_LOAD_START" -v b="$OXIGRAPH_LOAD_END" 'BEGIN{printf "%.2f", b-a}')

# --- QLever: memory-mapped disk index (only supported mode) ---
# Index was already built in step [3/6] (QLEVER_LOAD_S); starting the server
# against a pre-built index is fast (just opens the mmap files).
( cd "$QLEVER_DIR" && "$QLEVER_BIN/qlever-server" -i bench -p "$QLEVER_PORT" -n > /tmp/qlever_server_bench.log 2>&1 & )
sleep 1
QLEVER_PID=$(pgrep -f "qlever-server -i bench -p $QLEVER_PORT" | head -1)

echo "Waiting for all servers to be ready..."
wait_ready "$QLEVER_READY_URL"


# --- Start background CPU sampler (runs for the whole query phase) ---
: > "$NOVA_CPU_LOG"
: > "$QLEVER_CPU_LOG"
: > "$OXIGRAPH_CPU_LOG"
(
  while true; do
    ps -o %cpu= -p "$NOVA_PID" 2>/dev/null | tr -d ' ' >> "$NOVA_CPU_LOG" || true
    ps -o %cpu= -p "$QLEVER_PID" 2>/dev/null | tr -d ' ' >> "$QLEVER_CPU_LOG" || true
    docker stats oxigraph_bench --no-stream --format '{{.CPUPerc}}' 2>/dev/null | tr -d '%' >> "$OXIGRAPH_CPU_LOG" || true
    sleep 0.3
  done
) &
CPU_SAMPLER_PID=$!

echo "=== [5/6] Running benchmark queries (warmup=$WARMUP, iters=$ITERS) ==="
echo "engine,query,iter,time_s" > "$CSV"

run_engine_queries() {
  local engine="$1"
  local url="$2"
  local accept_header="$3"

  while IFS=$'\t' read -r name sparql expected; do
    # ── Warm-up pass (critical for QLever's OS page cache; harmless for others) ──
    for _ in $(seq 1 "$WARMUP"); do
      curl -s -G --data-urlencode "query=$sparql" -H "$accept_header" "$url" -o /dev/null
    done

    # ── Correctness check (skip if expected == null, i.e. informational-only queries) ──
    if [ "$expected" != "null" ]; then
      actual=$(curl -s -G --data-urlencode "query=$sparql" -H "$accept_header" "$url" \
        | jq -r '.results.bindings | length' 2>/dev/null || echo "ERROR")
      if [ "$actual" != "$expected" ]; then
        echo "  [WARN] [$engine] $name: expected $expected bindings, got $actual" >&2
      fi
    fi

    # ── Timed runs ──
    for i in $(seq 1 "$ITERS"); do
      t=$(curl -s -G --data-urlencode "query=$sparql" -H "$accept_header" "$url" \
        -o /dev/null -w "%{time_total}")
      echo "$engine,$name,$i,$t" >> "$CSV"
    done
    echo "  [$engine] $name done"
  done < <(jq -r '.[] | [.name, .sparql, (.expected // "null")] | @tsv' "$QUERIES")
}

# ── NOTE ON MEMORY MEASUREMENT TIMING (important, see README) ──────────────
# Memory is captured IMMEDIATELY after each engine's own query phase, rather
# than all together at the very end. On macOS, the kernel's memory compressor
# aggressively compresses/swaps the heap of a process that has gone idle
# (e.g. Nova finishing its phase and then sitting idle for many minutes while
# Oxigraph and QLever run their own phases). Measuring at the very end after
# such an idle period would report a massively understated (compressed)
# figure that does not reflect actual working-set memory during querying.
# Measuring right after each engine's own phase avoids this artifact and is
# a fair apples-to-apples comparison.
#
# NOTE ON METRIC (see the top-of-file comment for full rationale): native
# processes (Nova, QLever) are measured via `measure_footprint_kb`, which
# uses `vmmap -summary`'s "Physical footprint" on macOS (stable,
# allocator-retention-immune) rather than `ps -o rss` (which can swing wildly
# run-to-run due to `libmalloc` retaining freed-but-unreturned memory).

run_engine_queries "nova"     "http://127.0.0.1:$NOVA_PORT/sparql"    "Accept: application/sparql-results+json"
NOVA_RSS_KB=$(measure_footprint_kb "$NOVA_PID")

run_engine_queries "oxigraph" "http://localhost:$OXIGRAPH_PORT/sparql" "Accept: application/sparql-results+json"
OXIGRAPH_MEM=$(docker stats oxigraph_bench --no-stream --format '{{.MemUsage}}' | awk -F'/' '{print $1}' | tr -d ' ')

run_engine_queries "qlever"   "http://localhost:$QLEVER_PORT/"         "Accept: application/sparql-results+json"
QLEVER_RSS_KB=$(measure_footprint_kb "$QLEVER_PID")

# --- Stop CPU sampler ---
kill "$CPU_SAMPLER_PID" 2>/dev/null || true
CPU_SAMPLER_PID=""

avg_cpu() {
  local log="$1"
  if [ -s "$log" ]; then
    awk '{s+=$1; n++} END { if (n>0) printf "%.2f", s/n; else print "0" }' "$log"
  else
    echo "0"
  fi
}
NOVA_CPU_PCT=$(avg_cpu "$NOVA_CPU_LOG")
QLEVER_CPU_PCT=$(avg_cpu "$QLEVER_CPU_LOG")
OXIGRAPH_CPU_PCT=$(avg_cpu "$OXIGRAPH_CPU_LOG")

echo "=== [6/6] Generating report ==="

echo "Nova mem:      ${NOVA_RSS_KB} KB (vmmap physical footprint, or ps RSS fallback)   | avg CPU: ${NOVA_CPU_PCT}%"
echo "QLever mem:    ${QLEVER_RSS_KB} KB (vmmap physical footprint incl. mmap'd index pages, or ps RSS fallback) | avg CPU: ${QLEVER_CPU_PCT}%"
echo "Oxigraph MEM:  ${OXIGRAPH_MEM} (docker stats, in-memory mode) | avg CPU: ${OXIGRAPH_CPU_PCT}%"

echo "Nova load:     ${NOVA_LOAD_S}s | Oxigraph load: ${OXIGRAPH_LOAD_S}s | QLever index build: ${QLEVER_LOAD_S}s"

python3 "$OUT_DIR/generate_report.py" \
  --csv "$CSV" \
  --queries "$QUERIES" \
  --nova-rss-kb "$NOVA_RSS_KB" \
  --qlever-rss-kb "$QLEVER_RSS_KB" \
  --oxigraph-mem "$OXIGRAPH_MEM" \
  --nova-cpu-pct "$NOVA_CPU_PCT" \
  --qlever-cpu-pct "$QLEVER_CPU_PCT" \
  --oxigraph-cpu-pct "$OXIGRAPH_CPU_PCT" \
  --nova-load-s "$NOVA_LOAD_S" \
  --qlever-load-s "$QLEVER_LOAD_S" \
  --oxigraph-load-s "$OXIGRAPH_LOAD_S" \
  --entities "$ENTITIES" \
  --triples "$(( ENTITIES * 25 ))" \
  --out "$RESULTS_MD"


echo ""
echo "Done. Results written to $RESULTS_MD (raw data: $CSV)"
