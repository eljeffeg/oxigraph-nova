#!/usr/bin/env bash
# Comparative benchmark harness: Nova vs Oxigraph vs QLever vs Fluree [+ RDFox]
#
# Modes:
#   (default) in-memory  — Nova (louds) | Nova (ring) | Oxigraph | QLever | Fluree
#                          + RDFox when a valid license is present (optional)
#   --disk               — Nova (louds, --location) | Oxigraph (--location) |
#                          QLever | Fluree (--storage-path)
#                          Ring is skipped in disk mode (no WAL yet).
#                          RDFox is mem-only in this harness (skipped on --disk).
#
# Methodology (see README.md):
#   Mem:
#     - Nova (louds/ring): pure in-process heap; independent fresh processes.
#     - Oxigraph: serve WITHOUT --location (pure in-memory).
#     - QLever: mmap index + warm-up (only mode).
#     - Fluree: fluree/server Docker, no host volume (ephemeral container FS).
#     - RDFox: sandbox/daemon endpoint, in-memory (requires RDFox.lic).
#   Disk:
#     - Nova (louds): --location WAL/snapshot path.
#     - Oxigraph: serve --location (RocksDB).
#     - QLever: same mmap index.
#     - Fluree: --storage-path with host volume mount.
#
# Usage:
#   ./run_comparison.sh [OPTIONS] [ENTITIES] [ITERS] [WARMUP] [RESULT_FILE]
#
# Options:
#   --disk                  Disk-backed / persistent comparison
#   --backends=both|louds|ring   Nova backends (mem only; default both).
#                           Ignored with --disk (always louds).
#   --no-oxigraph           Skip Oxigraph
#   --no-qlever             Skip QLever
#   --no-fluree             Skip Fluree (default: include via Docker)
#   --no-rdfox              Skip RDFox even if license is present
#   --charts                Write/embed SVG charts (default: off)
#   -h, --help              Show this help
#
# Env:
#   NOVA_BACKENDS=both|louds|ring   Same as --backends (mem default: both)
#   QUERY_TIMEOUT_S=60
#   QLEVER_BIN_DIR=/path/to/qlever/build
#   RDFOX_BIN=path/to/RDFox         (default: research/rdfox/RDFox)
#   RDFOX_LICENSE=path/to/RDFox.lic (default: research/rdfox/RDFox.lic or ~/.RDFox/RDFox.lic)
#   RDFOX_ROLE / RDFOX_PASSWORD     (default guest/guest for sandbox)



#
# Defaults:
#   mem:  ENTITIES=50000  ITERS=10 WARMUP=3  → RESULTS_MEM.md
#   disk: ENTITIES=500000 ITERS=10 WARMUP=3  → RESULTS_DISK.md
set -euo pipefail

MODE="mem"   # mem | disk
BACKENDS_FLAG=""
ENABLE_OXIGRAPH=1
ENABLE_QLEVER=1
ENABLE_FLUREE=1
ENABLE_RDFOX=1   # on by default when license/binary present; use --no-rdfox to skip
ENABLE_CHARTS=0
# Positional args collected without empty-array indexing under bash 3.2 + set -u.
P1=""
P2=""
P3=""
P4=""
_pos_n=0

usage() {
  sed -n '2,40p' "$0" | sed 's/^# \?//'
  exit 0
}

_push_pos() {
  _pos_n=$((_pos_n + 1))
  case "$_pos_n" in
    1) P1="$1" ;;
    2) P2="$1" ;;
    3) P3="$1" ;;
    4) P4="$1" ;;
  esac
}

while [ $# -gt 0 ]; do
  case "$1" in
    --disk) MODE="disk"; shift ;;
    --backends=*) BACKENDS_FLAG="${1#--backends=}"; shift ;;
    --backends)
      BACKENDS_FLAG="${2:-}"
      shift 2
      ;;
    --no-oxigraph) ENABLE_OXIGRAPH=0; shift ;;
    --no-qlever) ENABLE_QLEVER=0; shift ;;
    --no-fluree) ENABLE_FLUREE=0; shift ;;
    --no-rdfox) ENABLE_RDFOX=0; shift ;;
    --charts) ENABLE_CHARTS=1; shift ;;
    -h|--help) usage ;;
    --) shift; break ;;
    -*)
      echo "Unknown option: $1 (try --help)" >&2
      exit 1
      ;;
    *) _push_pos "$1"; shift ;;
  esac
done

while [ $# -gt 0 ]; do _push_pos "$1"; shift; done

if [ "$MODE" = "disk" ]; then
  ENTITIES="${P1:-500000}"
  ITERS="${P2:-10}"
  WARMUP="${P3:-3}"
  RESULT_FILE="${P4:-RESULTS_DISK.md}"
else
  ENTITIES="${P1:-50000}"
  ITERS="${P2:-10}"
  WARMUP="${P3:-3}"
  RESULT_FILE="${P4:-RESULTS_MEM.md}"
fi


QUERY_TIMEOUT_S="${QUERY_TIMEOUT_S:-60}"

# Nova backends: disk is always louds-only.
if [ "$MODE" = "disk" ]; then
  NOVA_BACKENDS="louds"
else
  NOVA_BACKENDS="${BACKENDS_FLAG:-${NOVA_BACKENDS:-both}}"
fi
case "$NOVA_BACKENDS" in
  both|louds|ring) ;;
  *)
    echo "NOVA_BACKENDS/--backends must be both|louds|ring (got: $NOVA_BACKENDS)" >&2
    exit 1
    ;;
esac
if [ "$MODE" = "disk" ] && [ "$NOVA_BACKENDS" != "louds" ]; then
  echo "Note: --disk forces Nova (louds) only (Ring has no WAL/--location yet)." >&2
  NOVA_BACKENDS="louds"
fi

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BENCH_DIR="/tmp/oxigraph-nova-bench"
OUT_DIR="$ROOT/benches/external"
QLEVER_BIN="${QLEVER_BIN_DIR:-/Users/jgentes/Documents/Workspace/qlever/build}"
DATA="$BENCH_DIR/dataset_${ENTITIES}.nt"
QUERIES="$BENCH_DIR/dataset_${ENTITIES}.queries.json"
RESULTS_MD="$OUT_DIR/$RESULT_FILE"

if [ "$MODE" = "disk" ]; then
  QLEVER_DIR="/tmp/qlever_bench_disk"
  CSV="$OUT_DIR/raw_results_disk.csv"
  NOVA_LOCATION="$BENCH_DIR/nova_disk_data"
  OXIGRAPH_LOCATION_HOST="$BENCH_DIR/oxigraph_disk_data"
  FLUREE_LOCATION_HOST="$BENCH_DIR/fluree_disk_data"
  NOVA_PORT=3031
  OXIGRAPH_PORT=7880
  QLEVER_PORT=7981
  FLUREE_PORT=8091
  RDFOX_PORT=12111
  DOCKER_NAME="oxigraph_bench_disk"
  OXIGRAPH_STATS_NAME="oxigraph_bench_disk"
  FLUREE_DOCKER_NAME="fluree_bench_disk"
  FLUREE_STATS_NAME="fluree_bench_disk"
else
  QLEVER_DIR="/tmp/qlever_bench"
  CSV="$OUT_DIR/raw_results.csv"
  NOVA_LOCATION=""
  OXIGRAPH_LOCATION_HOST=""
  FLUREE_LOCATION_HOST=""
  NOVA_PORT=3030
  OXIGRAPH_PORT=7878
  QLEVER_PORT=7979
  FLUREE_PORT=8090
  RDFOX_PORT=12110
  DOCKER_NAME="oxigraph_bench"
  OXIGRAPH_STATS_NAME="oxigraph_bench"
  FLUREE_DOCKER_NAME="fluree_bench"
  FLUREE_STATS_NAME="fluree_bench"
fi

FLUREE_LEDGER="bench:main"
FLUREE_CPU_LOG="/tmp/fluree_${MODE}_cpu_samples.txt"
RDFOX_CPU_LOG="/tmp/rdfox_${MODE}_cpu_samples.txt"
RDFOX_BIN="${RDFOX_BIN:-$ROOT/research/rdfox/RDFox}"
# Prefer explicit env, then in-tree key next to binary, then ~/.RDFox/RDFox.lic
if [ -z "${RDFOX_LICENSE:-}" ]; then
  if [ -f "$ROOT/research/rdfox/RDFox.lic" ]; then
    RDFOX_LICENSE="$ROOT/research/rdfox/RDFox.lic"
  else
    RDFOX_LICENSE="$HOME/.RDFox/RDFox.lic"
  fi
fi
RDFOX_ROLE="${RDFOX_ROLE:-guest}"
RDFOX_PASSWORD="${RDFOX_PASSWORD:-guest}"
RDFOX_DS="bench"
RDFOX_LOG="/tmp/rdfox_${MODE}_bench.log"
RDFOX_WORKDIR="/tmp/rdfox_${MODE}_workdir"
RDFOX_PID=""
RUN_OXIGRAPH=0
RUN_QLEVER=0
RUN_FLUREE=0
RUN_RDFOX=0


QLEVER_READY_URL="http://localhost:$QLEVER_PORT/?query=SELECT%20%3Fs%20WHERE%20%7B%3Fs%20%3Fp%20%3Fo%7D%20LIMIT%201"

NOVA_LOUDS_BIN="$BENCH_DIR/nova_serve_louds"
NOVA_RING_BIN="$BENCH_DIR/nova_serve_ring"

NOVA_LOUDS_CPU_LOG="/tmp/nova_${MODE}_louds_cpu_samples.txt"
NOVA_RING_CPU_LOG="/tmp/nova_${MODE}_ring_cpu_samples.txt"
QLEVER_CPU_LOG="/tmp/qlever_${MODE}_cpu_samples.txt"
OXIGRAPH_CPU_LOG="/tmp/oxigraph_${MODE}_cpu_samples.txt"

# Resolve engines up-front so logs are honest before long builds.
if [ "$ENABLE_OXIGRAPH" = "1" ]; then
  if command -v docker >/dev/null 2>&1; then
    RUN_OXIGRAPH=1
  else
    echo "Note: Docker not found; skipping Oxigraph." >&2
  fi
fi

if [ "$ENABLE_QLEVER" = "1" ]; then
  if [ -x "$QLEVER_BIN/qlever-index" ] && [ -x "$QLEVER_BIN/qlever-server" ]; then
    RUN_QLEVER=1
  else
    echo "Note: QLever binaries not found under $QLEVER_BIN; skipping QLever." >&2
    echo "      Set QLEVER_BIN_DIR to the directory containing qlever-index/qlever-server." >&2
  fi
fi

if [ "$ENABLE_FLUREE" = "1" ]; then
  if command -v docker >/dev/null 2>&1; then
    RUN_FLUREE=1
  else
    echo "Note: Docker not found; skipping Fluree." >&2
  fi
fi

if [ "$MODE" = "disk" ]; then
  # RDFox path in this harness is in-memory sandbox only.
  if [ "$ENABLE_RDFOX" = "1" ]; then
    echo "Note: RDFox is skipped in --disk mode (mem-only in this harness)." >&2
  fi
  RUN_RDFOX=0
elif [ "$ENABLE_RDFOX" = "1" ]; then
  if [ -x "$RDFOX_BIN" ] && [ -f "$RDFOX_LICENSE" ]; then
    # evaluation_license.txt is NOT a valid key; require a real .lic file.
    if grep -qi 'END USER LICENCE AGREEMENT\|EVALUATION LICENCE' "$RDFOX_LICENSE" 2>/dev/null; then
      echo "Note: RDFox license at $RDFOX_LICENSE is EULA text, not a key; skipping RDFox." >&2
      RUN_RDFOX=0
    else
      RUN_RDFOX=1
    fi
  else
    if [ ! -x "$RDFOX_BIN" ]; then
      echo "Note: RDFox binary not found at $RDFOX_BIN; skipping RDFox." >&2
    elif [ ! -f "$RDFOX_LICENSE" ]; then
      echo "Note: RDFox license not found at $RDFOX_LICENSE; skipping RDFox." >&2
      echo "      Place a valid RDFox.lic at research/rdfox/RDFox.lic or ~/.RDFox/RDFox.lic." >&2
    fi
    RUN_RDFOX=0
  fi
fi

echo "Engines: Oxigraph=$RUN_OXIGRAPH QLever=$RUN_QLEVER Fluree=$RUN_FLUREE RDFox=$RUN_RDFOX Nova=$NOVA_BACKENDS (mode=$MODE)"

mkdir -p "$BENCH_DIR" "$OUT_DIR"
if [ "$RUN_QLEVER" = "1" ] || [ "$ENABLE_QLEVER" = "1" ]; then
  mkdir -p "$QLEVER_DIR"
fi
if [ "$MODE" = "disk" ]; then
  rm -rf "$NOVA_LOCATION" "$OXIGRAPH_LOCATION_HOST" "$FLUREE_LOCATION_HOST"
  mkdir -p "$NOVA_LOCATION"
  [ "$ENABLE_OXIGRAPH" = "1" ] && mkdir -p "$OXIGRAPH_LOCATION_HOST"
  [ "$ENABLE_FLUREE" = "1" ] && mkdir -p "$FLUREE_LOCATION_HOST"
fi



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

avg_cpu() {
  local log="$1"
  if [ -s "$log" ]; then
    awk '{s+=$1; n++} END { if (n>0) printf "%.2f", s/n; else print "0" }' "$log"
  else
    echo "0"
  fi
}

echo "=== Mode: $MODE | NOVA_BACKENDS=$NOVA_BACKENDS | N=$ENTITIES iters=$ITERS warmup=$WARMUP ==="

echo "=== [1/6] Building Nova binaries (release) ==="
cd "$ROOT"
cargo build --release -p oxigraph-nova-bench --bin gen_dataset

NEED_LOUDS=0
NEED_RING=0
case "$NOVA_BACKENDS" in
  both) NEED_LOUDS=1; NEED_RING=1 ;;
  louds) NEED_LOUDS=1 ;;
  ring) NEED_RING=1 ;;
esac
if [ "$NEED_RING" = "1" ]; then
  # Product default: Huffman C_p via ring-backend. Set NOVA_RING_HUFFMAN=0 for
  # plain QWT256 A/B (ring-backend-qwt).
  if [ "${NOVA_RING_HUFFMAN:-1}" = "0" ]; then
    echo "  Ring binary: features=ring-backend-qwt (plain QWT256 C_p A/B)"
    cargo build --release -p oxigraph-nova-server --features ring-backend-qwt --bin nova_serve
  else
    echo "  Ring binary: features=ring-backend (Huffman C_p product default)"
    cargo build --release -p oxigraph-nova-server --features ring-backend --bin nova_serve
  fi
  cp -f "$ROOT/target/release/nova_serve" "$NOVA_RING_BIN"
fi

if [ "$NEED_LOUDS" = "1" ]; then
  cargo build --release -p oxigraph-nova-server --bin nova_serve
  cp -f "$ROOT/target/release/nova_serve" "$NOVA_LOUDS_BIN"
fi

echo "=== [2/6] Dataset (N=$ENTITIES entities) ==="
if [ "$MODE" = "disk" ] && [ -f "$DATA" ] && [ -f "$QUERIES" ]; then
  echo "  Reusing existing dataset at $DATA"
else
  "$ROOT/target/release/gen_dataset" --entities "$ENTITIES" --out "$DATA"
fi

QLEVER_LOAD_S=""
if [ "$RUN_QLEVER" = "1" ]; then
  echo "=== [3/6] Building QLever index ==="
  rm -f "$QLEVER_DIR"/bench.* 2>/dev/null || true
  QLEVER_LOAD_START=$(date +%s.%N)
  ( cd "$QLEVER_DIR" && "$QLEVER_BIN/qlever-index" -i bench -f "$DATA" -F nt )
  QLEVER_LOAD_END=$(date +%s.%N)
  QLEVER_LOAD_S=$(awk -v a="$QLEVER_LOAD_START" -v b="$QLEVER_LOAD_END" 'BEGIN{printf "%.2f", b-a}')
else
  echo "=== [3/6] Skipping QLever index (disabled) ==="
fi

NOVA_PID=""
QLEVER_PID=""
RDFOX_PID=""
CPU_SAMPLER_PID=""

OXIGRAPH_LOAD_S=""
OXIGRAPH_MEM=""
OXIGRAPH_CPU_PCT=""
OXIGRAPH_DISK_KB=""
QLEVER_RSS_KB=""
QLEVER_CPU_PCT=""
QLEVER_DISK_KB=""
FLUREE_LOAD_S=""
FLUREE_MEM=""
FLUREE_CPU_PCT=""
FLUREE_DISK_KB=""
RDFOX_LOAD_S=""
RDFOX_RSS_KB=""
RDFOX_CPU_PCT=""

cleanup() {
  echo "=== Cleaning up servers ==="
  [ -n "${CPU_SAMPLER_PID:-}" ] && kill "$CPU_SAMPLER_PID" 2>/dev/null || true
  [ -n "${NOVA_PID:-}" ] && kill "$NOVA_PID" 2>/dev/null || true
  [ -n "${QLEVER_PID:-}" ] && kill "$QLEVER_PID" 2>/dev/null || true
  if [ -n "${RDFOX_PID:-}" ]; then
    kill "$RDFOX_PID" 2>/dev/null || true
    wait "$RDFOX_PID" 2>/dev/null || true
    RDFOX_PID=""
  fi
  docker rm -f "$DOCKER_NAME" >/dev/null 2>&1 || true
  docker rm -f "$FLUREE_DOCKER_NAME" >/dev/null 2>&1 || true
}
trap cleanup EXIT


wait_ready() {
  local url=$1
  local timeout_s="${2:-120}"
  local i
  for (( i=0; i<timeout_s; i++ )); do
    code=$(curl -s -o /dev/null -w "%{http_code}" "$url" || echo "000")
    if [[ "$code" =~ ^(200|400)$ ]]; then return 0; fi
    sleep 1
  done
  echo "Server at $url did not become ready (waited ${timeout_s}s)" >&2
  return 1
}

free_port() {
  local port="$1"
  if command -v lsof >/dev/null 2>&1; then
    local old
    old=$(lsof -tiTCP:"$port" -sTCP:LISTEN 2>/dev/null || true)
    if [ -n "$old" ]; then
      # shellcheck disable=SC2086
      kill $old 2>/dev/null || true
      sleep 0.5
    fi
  fi
}

run_engine_queries() {
  local engine="$1"
  local url="$2"
  local accept_header="$3"
  # Optional 4th arg: SPARQL `FROM <...>` clause to inject before `WHERE`
  # (needed for Fluree's connection-scoped queries).
  local from_clause="${4:-}"
  # Optional 5th arg: extra curl args (e.g. RDFox basic auth) — evaluated carefully.
  local curl_auth="${5:-}"

  while IFS=$'\t' read -r name sparql expected; do
    if [ -n "$from_clause" ]; then
      sparql="${sparql/ WHERE / $from_clause WHERE }"
    fi

    for _ in $(seq 1 "$WARMUP"); do
      # shellcheck disable=SC2086
      curl -s --max-time "$QUERY_TIMEOUT_S" -G --data-urlencode "query=$sparql" \
        -H "$accept_header" $curl_auth "$url" -o /dev/null || true
    done

    if [ "$expected" != "null" ]; then
      # shellcheck disable=SC2086
      actual=$(curl -s --max-time "$QUERY_TIMEOUT_S" -G --data-urlencode "query=$sparql" \
        -H "$accept_header" $curl_auth "$url" \
        | jq -r '.results.bindings | length' 2>/dev/null || echo "ERROR")
      if [ "$actual" != "$expected" ]; then
        echo "  [WARN] [$engine] $name: expected $expected bindings, got $actual" >&2
      fi
    fi

    for i in $(seq 1 "$ITERS"); do
      # shellcheck disable=SC2086
      t=$(curl -s --max-time "$QUERY_TIMEOUT_S" -G --data-urlencode "query=$sparql" \
        -H "$accept_header" $curl_auth "$url" \
        -o /dev/null -w "%{time_total}" || echo "timeout")
      echo "$engine,$name,$i,$t" >> "$CSV"
    done
    echo "  [$engine] $name done"
  done < <(jq -r '.[] | [.name, .sparql, (.expected // "null")] | @tsv' "$QUERIES")
}

# Generic CPU sampler. args after qlever:
#   sample_fluree (0|1), sample_rdfox_pid (pid or empty)
start_cpu_sampler() {
  local native_pid="${1:-}"
  local native_log="${2:-}"
  local sample_oxigraph="${3:-0}"
  local sample_qlever="${4:-0}"
  local sample_fluree="${5:-0}"
  local sample_rdfox_pid="${6:-}"

  [ -n "$native_log" ] && : > "$native_log"
  [ "$sample_oxigraph" = "1" ] && : > "$OXIGRAPH_CPU_LOG"
  [ "$sample_qlever" = "1" ] && : > "$QLEVER_CPU_LOG"
  [ "$sample_fluree" = "1" ] && : > "$FLUREE_CPU_LOG"
  [ -n "$sample_rdfox_pid" ] && : > "$RDFOX_CPU_LOG"

  (
    while true; do
      if [ -n "$native_pid" ] && [ -n "$native_log" ]; then
        ps -o %cpu= -p "$native_pid" 2>/dev/null | tr -d ' ' >> "$native_log" || true
      fi
      if [ "$sample_qlever" = "1" ] && [ -n "${QLEVER_PID:-}" ]; then
        ps -o %cpu= -p "$QLEVER_PID" 2>/dev/null | tr -d ' ' >> "$QLEVER_CPU_LOG" || true
      fi
      if [ "$sample_oxigraph" = "1" ]; then
        docker stats "$OXIGRAPH_STATS_NAME" --no-stream --format '{{.CPUPerc}}' 2>/dev/null \
          | tr -d '%' >> "$OXIGRAPH_CPU_LOG" || true
      fi
      if [ "$sample_fluree" = "1" ]; then
        docker stats "$FLUREE_STATS_NAME" --no-stream --format '{{.CPUPerc}}' 2>/dev/null \
          | tr -d '%' >> "$FLUREE_CPU_LOG" || true
      fi
      if [ -n "$sample_rdfox_pid" ]; then
        ps -o %cpu= -p "$sample_rdfox_pid" 2>/dev/null | tr -d ' ' >> "$RDFOX_CPU_LOG" || true
      fi
      sleep 0.3
    done
  ) &
  CPU_SAMPLER_PID=$!
}


stop_cpu_sampler() {
  if [ -n "${CPU_SAMPLER_PID:-}" ]; then
    kill "$CPU_SAMPLER_PID" 2>/dev/null || true
    wait "$CPU_SAMPLER_PID" 2>/dev/null || true
    CPU_SAMPLER_PID=""
  fi
}

# Start one Nova process. tag=louds|ring. disk_mode uses --location for louds only.
start_nova() {
  local tag="$1"
  local bin="$2"
  local log="$3"
  free_port "$NOVA_PORT"
  if [ -n "${NOVA_PID:-}" ]; then
    kill "$NOVA_PID" 2>/dev/null || true
    wait "$NOVA_PID" 2>/dev/null || true
    NOVA_PID=""
  fi

  if [ "$MODE" = "disk" ]; then
    # Disk: louds only, with --location
    "$bin" --location "$NOVA_LOCATION" --file "$DATA" \
      --bind "127.0.0.1:$NOVA_PORT" > "$log" 2>&1 &
  elif [ "$tag" = "ring" ]; then
    "$bin" --backend ring --file "$DATA" \
      --bind "127.0.0.1:$NOVA_PORT" > "$log" 2>&1 &
  else
    "$bin" --file "$DATA" \
      --bind "127.0.0.1:$NOVA_PORT" > "$log" 2>&1 &
  fi
  NOVA_PID=$!
}

run_nova_backend() {
  local tag="$1"
  local bin="$2"
  local engine_id="$3"
  local cpu_log="/tmp/nova_${MODE}_${tag}_cpu_samples.txt"
  local log="/tmp/nova_serve_${MODE}_${tag}_bench.log"
  local ready_timeout=600
  if [ "$MODE" = "disk" ]; then
    ready_timeout=3600
  fi

  echo "--- Nova ($tag) [$MODE]: fresh process ($engine_id) ---"
  local load_start load_end load_s
  load_start=$(date +%s.%N)
  start_nova "$tag" "$bin" "$log"
  if ! wait_ready "http://127.0.0.1:$NOVA_PORT/sparql" "$ready_timeout"; then
    echo "Nova ($tag) failed to become ready; log tail:" >&2
    tail -n 40 "$log" >&2 || true
    return 1
  fi
  load_end=$(date +%s.%N)
  load_s=$(awk -v a="$load_start" -v b="$load_end" 'BEGIN{printf "%.2f", b-a}')

  start_cpu_sampler "$NOVA_PID" "$cpu_log" 0 0
  run_engine_queries "$engine_id" "http://127.0.0.1:$NOVA_PORT/sparql" \
    "Accept: application/sparql-results+json"
  local rss_kb cpu_pct
  rss_kb=$(measure_footprint_kb "$NOVA_PID")
  stop_cpu_sampler
  cpu_pct=$(avg_cpu "$cpu_log")

  # Keep Nova alive in disk mode until after disk du; mem tears down each phase.
  if [ "$MODE" = "mem" ]; then
    kill "$NOVA_PID" 2>/dev/null || true
    wait "$NOVA_PID" 2>/dev/null || true
    NOVA_PID=""
  fi

  if [ "$tag" = "louds" ]; then
    NOVA_LOUDS_LOAD_S="$load_s"
    NOVA_LOUDS_RSS_KB="$rss_kb"
    NOVA_LOUDS_CPU_PCT="$cpu_pct"
  else
    NOVA_RING_LOAD_S="$load_s"
    NOVA_RING_RSS_KB="$rss_kb"
    NOVA_RING_CPU_PCT="$cpu_pct"
  fi
  echo "  Nova ($tag) load=${load_s}s mem=${rss_kb}KB cpu=${cpu_pct}%"
}

NOVA_LOUDS_LOAD_S=""
NOVA_LOUDS_RSS_KB=""
NOVA_LOUDS_CPU_PCT=""
NOVA_RING_LOAD_S=""
NOVA_RING_RSS_KB=""
NOVA_RING_CPU_PCT=""

echo "=== [4/6] Starting external engines ==="

# --- Oxigraph ---
if [ "$RUN_OXIGRAPH" = "1" ]; then
  OXIGRAPH_LOAD_START=$(date +%s.%N)
  docker rm -f "$DOCKER_NAME" >/dev/null 2>&1 || true
  if [ "$MODE" = "disk" ]; then
    docker run -d --name "$DOCKER_NAME" -p "$OXIGRAPH_PORT:7878" \
      -v "$OXIGRAPH_LOCATION_HOST:/data" oxigraph/oxigraph:latest \
      serve --location /data --bind 0.0.0.0:7878 >/dev/null
  else
    docker run -d --name "$DOCKER_NAME" -p "$OXIGRAPH_PORT:7878" oxigraph/oxigraph:latest \
      serve --bind 0.0.0.0:7878 >/dev/null
  fi
  wait_ready "http://localhost:$OXIGRAPH_PORT/sparql"
  if [ "$MODE" = "disk" ]; then
    echo "  Bulk-loading dataset into Oxigraph (RocksDB-backed) via HTTP POST..."
  else
    echo "  Bulk-loading dataset into Oxigraph (in-memory) via HTTP POST..."
  fi
  curl -s -X POST -T "$DATA" -H "Content-Type: application/n-triples" \
    "http://localhost:$OXIGRAPH_PORT/store?default" -o /dev/null
  OXIGRAPH_LOAD_END=$(date +%s.%N)
  OXIGRAPH_LOAD_S=$(awk -v a="$OXIGRAPH_LOAD_START" -v b="$OXIGRAPH_LOAD_END" 'BEGIN{printf "%.2f", b-a}')
fi

# --- Fluree ---
if [ "$RUN_FLUREE" = "1" ]; then
  FLUREE_LOAD_START=$(date +%s.%N)
  docker rm -f "$FLUREE_DOCKER_NAME" >/dev/null 2>&1 || true
  if [ "$MODE" = "disk" ]; then
    echo "  Starting Fluree (file-backed --storage-path)..."
    docker run -d --name "$FLUREE_DOCKER_NAME" -p "$FLUREE_PORT:8090" \
      -v "$FLUREE_LOCATION_HOST:/var/lib/fluree" fluree/server:latest \
      --storage-path /var/lib/fluree >/dev/null
  else
    echo "  Starting Fluree (ephemeral container, no host volume)..."
    docker run -d --name "$FLUREE_DOCKER_NAME" -p "$FLUREE_PORT:8090" \
      fluree/server:latest >/dev/null
  fi
  wait_ready "http://localhost:$FLUREE_PORT/health" 180
  curl -s -X POST "http://localhost:$FLUREE_PORT/v1/fluree/create" \
    -H "Content-Type: application/json" \
    -d "{\"ledger\": \"$FLUREE_LEDGER\"}" -o /dev/null
  echo "  Bulk-loading dataset into Fluree via HTTP POST..."
  curl -s -X POST "http://localhost:$FLUREE_PORT/v1/fluree/insert?ledger=$FLUREE_LEDGER" \
    -H "Content-Type: application/n-triples" \
    --data-binary @"$DATA" -o /dev/null
  FLUREE_LOAD_END=$(date +%s.%N)
  FLUREE_LOAD_S=$(awk -v a="$FLUREE_LOAD_START" -v b="$FLUREE_LOAD_END" 'BEGIN{printf "%.2f", b-a}')
  echo "  Fluree load=${FLUREE_LOAD_S}s"
fi

# --- RDFox (mem only, optional license) ---
if [ "$RUN_RDFOX" = "1" ]; then
  echo "  Starting RDFox sandbox endpoint on port $RDFOX_PORT..."
  free_port "$RDFOX_PORT"
  RDFOX_LOAD_START=$(date +%s.%N)
  export RDFOX_ROLE RDFOX_PASSWORD
  # RDFox looks for license next to binary and/or in ~/.RDFox/
  mkdir -p "$HOME/.RDFox"
  if [ ! -f "$HOME/.RDFox/RDFox.lic" ] && [ -f "$RDFOX_LICENSE" ]; then
    cp -f "$RDFOX_LICENSE" "$HOME/.RDFox/RDFox.lic" 2>/dev/null || true
  fi
  # Also ensure license sits next to binary (common lookup path).
  if [ -f "$RDFOX_LICENSE" ] && [ ! -f "$(dirname "$RDFOX_BIN")/RDFox.lic" ]; then
    cp -f "$RDFOX_LICENSE" "$(dirname "$RDFOX_BIN")/RDFox.lic" 2>/dev/null || true
  fi

  rm -rf "$RDFOX_WORKDIR"
  mkdir -p "$RDFOX_WORKDIR"
  # Stage dataset into workdir so relative import works under sandbox root.
  cp -f "$DATA" "$RDFOX_WORKDIR/dataset.nt"

  # sandbox mode: process exits when stdin closes, so keep stdin open.
  # v7.6 params: -port (not -endpoint.port); store types: parallel-nn|nw|ww.
  (
    "$RDFOX_BIN" \
      -port "$RDFOX_PORT" \
      sandbox "$RDFOX_WORKDIR" \
      "dstore create $RDFOX_DS type parallel-nn" \
      "active $RDFOX_DS" \
      "import dataset.nt" \
      "endpoint start"
  ) > "$RDFOX_LOG" 2>&1 < <(while true; do sleep 3600; done) &
  RDFOX_PID=$!

  # wait_ready uses unauthenticated curl; RDFox needs basic auth — probe manually.
  rdfox_ready=0
  for _i in $(seq 1 180); do
    code=$(curl -s -o /dev/null -w "%{http_code}" -u "${RDFOX_ROLE}:${RDFOX_PASSWORD}" \
      -G --data-urlencode "query=SELECT ?s WHERE { ?s ?p ?o } LIMIT 1" \
      -H "Accept: application/sparql-results+json" \
      "http://127.0.0.1:$RDFOX_PORT/datastores/$RDFOX_DS/sparql" 2>/dev/null || echo "000")
    if [ "$code" = "200" ] || [ "$code" = "400" ]; then
      rdfox_ready=1
      break
    fi
    if ! kill -0 "$RDFOX_PID" 2>/dev/null; then
      break
    fi
    sleep 1
  done
  if [ "$rdfox_ready" != "1" ]; then
    echo "RDFox failed to become ready; log tail:" >&2
    tail -n 40 "$RDFOX_LOG" >&2 || true
    echo "Note: skipping RDFox for this run." >&2
    kill "$RDFOX_PID" 2>/dev/null || true
    wait "$RDFOX_PID" 2>/dev/null || true
    RDFOX_PID=""
    RUN_RDFOX=0
  else
    RDFOX_LOAD_END=$(date +%s.%N)
    RDFOX_LOAD_S=$(awk -v a="$RDFOX_LOAD_START" -v b="$RDFOX_LOAD_END" 'BEGIN{printf "%.2f", b-a}')
    echo "  RDFox load=${RDFOX_LOAD_S}s (license: $RDFOX_LICENSE)"
  fi
fi


# --- QLever ---
if [ "$RUN_QLEVER" = "1" ]; then
  ( cd "$QLEVER_DIR" && "$QLEVER_BIN/qlever-server" -i bench -p "$QLEVER_PORT" -n > "/tmp/qlever_server_${MODE}_bench.log" 2>&1 & )
  sleep 1
  QLEVER_PID=$(pgrep -f "qlever-server -i bench -p $QLEVER_PORT" | head -1)
  echo "Waiting for QLever..."
  wait_ready "$QLEVER_READY_URL"
fi

echo "=== [5/6] Running benchmark queries (warmup=$WARMUP, iters=$ITERS) ==="
echo "engine,query,iter,time_s" > "$CSV"

if [ "$NEED_LOUDS" = "1" ]; then
  run_nova_backend "louds" "$NOVA_LOUDS_BIN" "nova-louds"
fi
if [ "$NEED_RING" = "1" ]; then
  run_nova_backend "ring" "$NOVA_RING_BIN" "nova-ring"
fi

# Oxigraph query phase
if [ "$RUN_OXIGRAPH" = "1" ]; then
  start_cpu_sampler "" "" 1 0 0 ""
  run_engine_queries "oxigraph" "http://localhost:$OXIGRAPH_PORT/sparql" \
    "Accept: application/sparql-results+json"
  OXIGRAPH_MEM=$(docker stats "$OXIGRAPH_STATS_NAME" --no-stream --format '{{.MemUsage}}' | awk -F'/' '{print $1}' | tr -d ' ')
  stop_cpu_sampler
  OXIGRAPH_CPU_PCT=$(avg_cpu "$OXIGRAPH_CPU_LOG")
fi

# Fluree query phase
if [ "$RUN_FLUREE" = "1" ]; then
  start_cpu_sampler "" "" 0 0 1 ""
  run_engine_queries "fluree" \
    "http://localhost:$FLUREE_PORT/v1/fluree/query" \
    "Accept: application/sparql-results+json" \
    "FROM <$FLUREE_LEDGER>"
  FLUREE_MEM=$(docker stats "$FLUREE_STATS_NAME" --no-stream --format '{{.MemUsage}}' | awk -F'/' '{print $1}' | tr -d ' ')
  stop_cpu_sampler
  FLUREE_CPU_PCT=$(avg_cpu "$FLUREE_CPU_LOG")
  echo "  Fluree mem=${FLUREE_MEM} cpu=${FLUREE_CPU_PCT}%"
fi

# RDFox query phase
if [ "$RUN_RDFOX" = "1" ] && [ -n "${RDFOX_PID:-}" ]; then
  start_cpu_sampler "$RDFOX_PID" "$RDFOX_CPU_LOG" 0 0 0 "$RDFOX_PID"
  # RDFox SPARQL endpoint: /datastores/<name>/sparql with basic auth
  run_engine_queries "rdfox" \
    "http://127.0.0.1:$RDFOX_PORT/datastores/$RDFOX_DS/sparql" \
    "Accept: application/sparql-results+json" \
    "" \
    "-u ${RDFOX_ROLE}:${RDFOX_PASSWORD}"
  RDFOX_RSS_KB=$(measure_footprint_kb "$RDFOX_PID")
  stop_cpu_sampler
  RDFOX_CPU_PCT=$(avg_cpu "$RDFOX_CPU_LOG")
  echo "  RDFox mem=${RDFOX_RSS_KB}KB cpu=${RDFOX_CPU_PCT}%"
fi

# QLever query phase
if [ "$RUN_QLEVER" = "1" ]; then
  start_cpu_sampler "" "" 0 1 0 ""
  run_engine_queries "qlever" "http://localhost:$QLEVER_PORT/" \
    "Accept: application/sparql-results+json"
  QLEVER_RSS_KB=$(measure_footprint_kb "$QLEVER_PID")
  stop_cpu_sampler
  QLEVER_CPU_PCT=$(avg_cpu "$QLEVER_CPU_LOG")
fi

echo "=== [6/6] Generating report ==="

if [ "$MODE" = "disk" ]; then
  NOVA_DISK_KB=$(du -sk "$NOVA_LOCATION" 2>/dev/null | awk '{print $1}')
  if [ "$RUN_OXIGRAPH" = "1" ]; then
    OXIGRAPH_DISK_KB=$(du -sk "$OXIGRAPH_LOCATION_HOST" 2>/dev/null | awk '{print $1}')
  fi
  if [ "$RUN_QLEVER" = "1" ]; then
    QLEVER_DISK_KB=$(du -sk "$QLEVER_DIR" 2>/dev/null | awk '{print $1}')
  fi
  if [ "$RUN_FLUREE" = "1" ] && [ -n "$FLUREE_LOCATION_HOST" ]; then
    FLUREE_DISK_KB=$(du -sk "$FLUREE_LOCATION_HOST" 2>/dev/null | awk '{print $1}')
  fi
  # Prefer louds metrics (only Nova on disk)
  NOVA_RSS_KB="${NOVA_LOUDS_RSS_KB:-0}"
  NOVA_CPU_PCT="${NOVA_LOUDS_CPU_PCT:-0}"
  NOVA_LOAD_S="${NOVA_LOUDS_LOAD_S:-0}"

  echo "Nova (louds) mem: ${NOVA_RSS_KB} KB | CPU ${NOVA_CPU_PCT}% | disk ${NOVA_DISK_KB} KB | load ${NOVA_LOAD_S}s"
  if [ "$RUN_OXIGRAPH" = "1" ]; then
    echo "Oxigraph MEM:     ${OXIGRAPH_MEM} | CPU ${OXIGRAPH_CPU_PCT}% | disk ${OXIGRAPH_DISK_KB} KB"
  fi
  if [ "$RUN_QLEVER" = "1" ]; then
    echo "QLever mem:       ${QLEVER_RSS_KB} KB | CPU ${QLEVER_CPU_PCT}% | disk ${QLEVER_DISK_KB} KB"
  fi
  if [ "$RUN_FLUREE" = "1" ]; then
    echo "Fluree MEM:       ${FLUREE_MEM} | CPU ${FLUREE_CPU_PCT}% | disk ${FLUREE_DISK_KB:-0} KB | load ${FLUREE_LOAD_S}s"
  fi

  set -- \
    --csv "$CSV" \
    --queries "$QUERIES" \
    --nova-rss-kb "$NOVA_RSS_KB" \
    --nova-cpu-pct "$NOVA_CPU_PCT" \
    --nova-load-s "$NOVA_LOAD_S" \
    --nova-disk-kb "${NOVA_DISK_KB:-0}" \
    --entities "$ENTITIES" \
    --triples "$(( ENTITIES * 25 ))" \
    --out "$RESULTS_MD"
  if [ "$RUN_OXIGRAPH" = "1" ]; then
    set -- "$@" \
      --oxigraph-mem "$OXIGRAPH_MEM" \
      --oxigraph-cpu-pct "$OXIGRAPH_CPU_PCT" \
      --oxigraph-load-s "$OXIGRAPH_LOAD_S" \
      --oxigraph-disk-kb "${OXIGRAPH_DISK_KB:-0}"
  fi
  if [ "$RUN_QLEVER" = "1" ]; then
    set -- "$@" \
      --qlever-rss-kb "$QLEVER_RSS_KB" \
      --qlever-cpu-pct "$QLEVER_CPU_PCT" \
      --qlever-load-s "$QLEVER_LOAD_S" \
      --qlever-disk-kb "${QLEVER_DISK_KB:-0}"
  fi
  if [ "$RUN_FLUREE" = "1" ]; then
    set -- "$@" \
      --fluree-mem "$FLUREE_MEM" \
      --fluree-cpu-pct "$FLUREE_CPU_PCT" \
      --fluree-load-s "$FLUREE_LOAD_S" \
      --fluree-disk-kb "${FLUREE_DISK_KB:-0}"
  fi
  if [ "$ENABLE_CHARTS" = "1" ]; then
    set -- "$@" --charts
  fi
  python3 "$OUT_DIR/generate_report_disk.py" "$@"
else
  if [ "$RUN_OXIGRAPH" = "1" ]; then
    echo "Oxigraph MEM:  ${OXIGRAPH_MEM} | avg CPU: ${OXIGRAPH_CPU_PCT}%"
  fi
  if [ "$RUN_QLEVER" = "1" ]; then
    echo "QLever mem:    ${QLEVER_RSS_KB} KB | avg CPU: ${QLEVER_CPU_PCT}% | index: ${QLEVER_LOAD_S}s"
  fi
  if [ -n "$NOVA_LOUDS_RSS_KB" ]; then
    echo "Nova (louds):  ${NOVA_LOUDS_RSS_KB} KB | CPU ${NOVA_LOUDS_CPU_PCT}% | load ${NOVA_LOUDS_LOAD_S}s"
  fi
  if [ -n "$NOVA_RING_RSS_KB" ]; then
    echo "Nova (ring):   ${NOVA_RING_RSS_KB} KB | CPU ${NOVA_RING_CPU_PCT}% | load ${NOVA_RING_LOAD_S}s"
  fi
  if [ "$RUN_FLUREE" = "1" ]; then
    echo "Fluree MEM:    ${FLUREE_MEM} | avg CPU: ${FLUREE_CPU_PCT}% | load ${FLUREE_LOAD_S}s"
  fi
  if [ "$RUN_RDFOX" = "1" ] && [ -n "$RDFOX_RSS_KB" ]; then
    echo "RDFox mem:     ${RDFOX_RSS_KB} KB | avg CPU: ${RDFOX_CPU_PCT}% | load ${RDFOX_LOAD_S}s"
  fi

  # Build report args without empty-array pitfalls under bash 3.2 + set -u
  set -- \
    --csv "$CSV" \
    --queries "$QUERIES" \
    --entities "$ENTITIES" \
    --triples "$(( ENTITIES * 25 ))" \
    --out "$RESULTS_MD"
  if [ "$RUN_OXIGRAPH" = "1" ]; then
    set -- "$@" \
      --oxigraph-mem "$OXIGRAPH_MEM" \
      --oxigraph-cpu-pct "$OXIGRAPH_CPU_PCT" \
      --oxigraph-load-s "$OXIGRAPH_LOAD_S"
  fi
  if [ "$RUN_QLEVER" = "1" ]; then
    set -- "$@" \
      --qlever-rss-kb "$QLEVER_RSS_KB" \
      --qlever-cpu-pct "$QLEVER_CPU_PCT" \
      --qlever-load-s "$QLEVER_LOAD_S"
  fi
  if [ -n "$NOVA_LOUDS_RSS_KB" ]; then
    set -- "$@" \
      --nova-louds-rss-kb "$NOVA_LOUDS_RSS_KB" \
      --nova-louds-cpu-pct "$NOVA_LOUDS_CPU_PCT" \
      --nova-louds-load-s "$NOVA_LOUDS_LOAD_S"
  fi
  if [ -n "$NOVA_RING_RSS_KB" ]; then
    set -- "$@" \
      --nova-ring-rss-kb "$NOVA_RING_RSS_KB" \
      --nova-ring-cpu-pct "$NOVA_RING_CPU_PCT" \
      --nova-ring-load-s "$NOVA_RING_LOAD_S"
  fi
  if [ "$RUN_FLUREE" = "1" ]; then
    set -- "$@" \
      --fluree-mem "$FLUREE_MEM" \
      --fluree-cpu-pct "$FLUREE_CPU_PCT" \
      --fluree-load-s "$FLUREE_LOAD_S"
  fi
  if [ "$RUN_RDFOX" = "1" ] && [ -n "$RDFOX_RSS_KB" ]; then
    set -- "$@" \
      --rdfox-rss-kb "$RDFOX_RSS_KB" \
      --rdfox-cpu-pct "$RDFOX_CPU_PCT" \
      --rdfox-load-s "$RDFOX_LOAD_S"
  fi
  if [ "$ENABLE_CHARTS" = "1" ]; then
    set -- "$@" --charts
  fi
  python3 "$OUT_DIR/generate_report.py" "$@"
fi

echo ""
echo "Done. Results written to $RESULTS_MD (raw data: $CSV)"
ids=""
[ "$NEED_LOUDS" = "1" ] && ids="${ids:+$ids, }nova-louds"
[ "$NEED_RING" = "1" ] && ids="${ids:+$ids, }nova-ring"
[ "$RUN_OXIGRAPH" = "1" ] && ids="${ids:+$ids, }oxigraph"
[ "$RUN_QLEVER" = "1" ] && ids="${ids:+$ids, }qlever"
[ "$RUN_FLUREE" = "1" ] && ids="${ids:+$ids, }fluree"
[ "$RUN_RDFOX" = "1" ] && [ -n "${RDFOX_RSS_KB:-}" ] && ids="${ids:+$ids, }rdfox"
echo "CSV engine IDs: ${ids:-none}"

