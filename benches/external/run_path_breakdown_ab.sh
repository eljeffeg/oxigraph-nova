#!/usr/bin/env bash
# LOUDS vs Ring HTTP path-breakdown A/B (architecture validation).
#
# Same HTTP endpoint, same SPARQL text, same dataset. Breaks wall time into:
#   Parse / Execution / Decode / Serialize / Total
# using nova_serve /metrics nova_path_* counters.
#
# Interprets:
#   A — Phase L works (Execution drops on Ring vs LOUDS)
#   B — prepared operator not used (Execution same)
#   C — decode/materialize dominates (Execution small/same; Decode/Serialize large)
#
# Usage:
#   ./benches/external/run_path_breakdown_ab.sh [ENTITIES] [ITERS] [WARMUP]
# Defaults: 50000 10 3

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
ENTITIES="${1:-50000}"
ITERS="${2:-10}"
WARMUP="${3:-3}"
PORT="${PORT:-3030}"
BENCH_DIR="${BENCH_DIR:-/tmp/oxigraph-nova-bench}"
DATA="$BENCH_DIR/dataset_${ENTITIES}.nt"
QUERIES="$BENCH_DIR/dataset_${ENTITIES}.queries.json"
OUT_DIR="${OUT_DIR:-$BENCH_DIR/path_breakdown}"
mkdir -p "$OUT_DIR"

LOUDS_BIN="$ROOT/target/release/nova_serve_louds"
RING_BIN="$ROOT/target/release/nova_serve_ring"

if [[ ! -x "$LOUDS_BIN" || ! -x "$RING_BIN" ]]; then
  echo "ERROR: need both binaries:"
  echo "  $LOUDS_BIN"
  echo "  $RING_BIN"
  echo "Build with:"
  echo "  cargo build --release -p oxigraph-nova-server --bin nova_serve && cp target/release/nova_serve target/release/nova_serve_louds"
  echo "  cargo build --release -p oxigraph-nova-server --features ring-backend --bin nova_serve && cp target/release/nova_serve target/release/nova_serve_ring"
  exit 1
fi

if [[ ! -f "$DATA" || ! -f "$QUERIES" ]]; then
  echo "ERROR: missing dataset at $DATA / $QUERIES"
  echo "Generate with: target/release/gen_dataset --entities $ENTITIES --out $DATA"
  exit 1
fi

# Extract SPARQL for named queries
path_2hop_q=$(python3 -c "import json; q=json.load(open('$QUERIES')); print(next(x['sparql'] for x in q if x['name']=='path_2hop'))")
triangle_q=$(python3 -c "import json; q=json.load(open('$QUERIES')); print(next(x['sparql'] for x in q if x['name']=='triangle'))")

kill_port() {
  local pids
  pids=$(lsof -ti tcp:"$PORT" 2>/dev/null || true)
  if [[ -n "${pids:-}" ]]; then
    echo "Killing PIDs on :$PORT → $pids"
    # shellcheck disable=SC2086
    kill $pids 2>/dev/null || true
    sleep 1
    pids=$(lsof -ti tcp:"$PORT" 2>/dev/null || true)
    if [[ -n "${pids:-}" ]]; then
      # shellcheck disable=SC2086
      kill -9 $pids 2>/dev/null || true
      sleep 0.5
    fi
  fi
}

wait_ready() {
  # Match run_comparison_mem.sh: SPARQL endpoint returns 200 or 400 when up.
  # Do NOT use /metrics alone — bind happens only after full dataset load.
  local url="http://127.0.0.1:$PORT/sparql"
  local tries=180
  local code
  for _ in $(seq 1 "$tries"); do
    code=$(curl -s -o /dev/null -m 2 -w "%{http_code}" "$url" 2>/dev/null || echo 000)
    if [[ "$code" =~ ^(200|400)$ ]]; then
      return 0
    fi
    sleep 1
  done
  echo "ERROR: server not ready at $url (last code=$code)"
  return 1
}

scrape_metrics() {
  local out="$1"
  # never fail the harness on scrape
  curl -sf -m 10 "http://127.0.0.1:$PORT/metrics" > "$out" 2>/dev/null || {
    echo "# scrape failed" > "$out"
    return 1
  }
  return 0
}

run_sparql() {
  local sparql="$1" out="$2"
  curl -sf -m 600 -o "$out" \
    -H "Accept: application/sparql-results+json" \
    --data-urlencode "query=$sparql" \
    "http://127.0.0.1:$PORT/sparql"
}

analyze_query() {
  local pre_path="$1" post_path="$2" times_path="$3" backend="$4" qname="$5" nrows="$6"
  python3 - "$pre_path" "$post_path" "$times_path" "$backend" "$qname" "$nrows" <<'PY'
import sys, re, statistics
pre_path, post_path, times_path, backend, qname, nrows = sys.argv[1:7]

def parse_metrics(path):
    d = {}
    try:
        with open(path) as f:
            for line in f:
                line=line.strip()
                if not line or line.startswith('#'): continue
                m = re.match(r'^([a-zA-Z0-9_:]+)\s+([0-9.eE+-]+)$', line)
                if m:
                    d[m.group(1)] = float(m.group(2))
    except FileNotFoundError:
        pass
    return d

pre, post = parse_metrics(pre_path), parse_metrics(post_path)
delta = {}
for k in set(list(pre) + list(post)):
    if k.startswith('nova_'):
        delta[k] = post.get(k, 0.0) - pre.get(k, 0.0)

def mean_ms(ns_key, n_key):
    ns = delta.get(ns_key, 0.0)
    n  = delta.get(n_key, 0.0)
    if n <= 0: return 0.0
    return (ns / n) / 1e6

parse_ms = mean_ms('nova_path_parse_ns_total','nova_path_parse_samples_total')
exec_ms  = mean_ms('nova_path_execution_ns_total','nova_path_execution_samples_total')
dec_ms   = mean_ms('nova_path_decode_ns_total','nova_path_decode_samples_total')
ser_ms   = mean_ms('nova_path_serialize_ns_total','nova_path_serialize_samples_total')
bucket_total = parse_ms + exec_ms + dec_ms + ser_ms

walls = [float(x) for x in open(times_path) if x.strip()] if __import__('os').path.exists(times_path) else []
wall_mean = statistics.mean(walls) if walls else 0.0
wall_med  = statistics.median(walls) if walls else 0.0

rec = {k: int(delta.get(k,0)) for k in delta if any(s in k for s in
  ('two_hop','triangle','d1_selected','lftj_','fallback','wedge','collapse','shape'))}

print(f"RESULT backend={backend} query={qname}")
print(f"  wall_mean_ms={wall_mean:.2f}  wall_median_ms={wall_med:.2f}  n={len(walls)}  rows={nrows}")
print(f"  parse_ms={parse_ms:.3f}")
print(f"  execution_ms={exec_ms:.3f}")
print(f"  decode_ms={dec_ms:.3f}")
print(f"  serialize_ms={ser_ms:.3f}")
print(f"  bucket_sum_ms={bucket_total:.3f}")
print(f"  samples parse/exec/dec/ser = "
      f"{int(delta.get('nova_path_parse_samples_total',0))}/"
      f"{int(delta.get('nova_path_execution_samples_total',0))}/"
      f"{int(delta.get('nova_path_decode_samples_total',0))}/"
      f"{int(delta.get('nova_path_serialize_samples_total',0))}")
if rec:
    print("  counter_deltas:")
    for k in sorted(rec):
        if rec[k]:
            print(f"    {k}={rec[k]}")
print(f"CSV,{backend},{qname},{wall_mean:.3f},{parse_ms:.3f},{exec_ms:.3f},{dec_ms:.3f},{ser_ms:.3f},{bucket_total:.3f},{nrows}")
PY
}

# One server start per backend; both queries with metrics deltas.
run_backend() {
  local backend="$1"
  local bin
  local -a cmd
  local log="$OUT_DIR/${backend}.log"

  if [[ "$backend" == "ring" ]]; then
    bin="$RING_BIN"
    cmd=("$bin" --backend ring --file "$DATA" --bind "127.0.0.1:$PORT")
  else
    bin="$LOUDS_BIN"
    cmd=("$bin" --file "$DATA" --bind "127.0.0.1:$PORT")
  fi

  echo ""
  echo "════════════════════════════════════════════════════════"
  echo " START BACKEND=$backend"
  echo "════════════════════════════════════════════════════════"
  kill_port
  echo "Starting ${cmd[*]}"
  "${cmd[@]}" >"$log" 2>&1 &
  local spid=$!
  echo "PID=$spid  log=$log"

  if ! wait_ready; then
    echo "---- server log ----"
    tail -80 "$log" || true
    kill_port
    return 1
  fi
  echo "Server ready."
  sleep 0.5

  local qname sparql tag times_out pre_m post_m i t0 t1 ms nrows
  for qname in path_2hop triangle; do
    if [[ "$qname" == "path_2hop" ]]; then
      sparql="$path_2hop_q"
    else
      sparql="$triangle_q"
    fi
    tag="${backend}_${qname}"
    times_out="$OUT_DIR/${tag}.wall.txt"
    pre_m="$OUT_DIR/${tag}.metrics_pre.txt"
    post_m="$OUT_DIR/${tag}.metrics.txt"
    : > "$times_out"

    echo ""
    echo "── $backend / $qname  warm=$WARMUP iters=$ITERS ──"

    echo "Warmup x$WARMUP..."
    for _ in $(seq 1 "$WARMUP"); do
      if ! run_sparql "$sparql" /dev/null; then
        echo "ERROR: warmup failed"; tail -30 "$log" || true; return 1
      fi
    done

    scrape_metrics "$pre_m" || true

    echo "Timed iters x$ITERS..."
    for i in $(seq 1 "$ITERS"); do
      t0=$(python3 -c 'import time; print(time.perf_counter_ns())')
      if ! run_sparql "$sparql" "/tmp/nova_path_ab_${tag}_${i}.json"; then
        echo "ERROR: timed iter $i failed"; tail -30 "$log" || true; return 1
      fi
      t1=$(python3 -c 'import time; print(time.perf_counter_ns())')
      ms=$(python3 -c "print( ($t1-$t0)/1e6 )")
      echo "$ms" >> "$times_out"
      printf "  iter %2d: %8.1f ms\n" "$i" "$ms"
    done

    # let serialize worker finish path-timing sample
    sleep 1
    scrape_metrics "$post_m" || true

    nrows=$(python3 -c "
import json
try:
  d=json.load(open('/tmp/nova_path_ab_${tag}_${ITERS}.json'))
  print(len(d.get('results',{}).get('bindings',[])))
except Exception:
  print(0)
")
    echo "result_rows_last=$nrows"
    analyze_query "$pre_m" "$post_m" "$times_out" "$backend" "$qname" "$nrows"
  done

  kill_port
  wait "$spid" 2>/dev/null || true
}

SUMMARY="$OUT_DIR/summary.txt"
: > "$SUMMARY"
echo "path_breakdown ENTITIES=$ENTITIES ITERS=$ITERS WARMUP=$WARMUP $(date -u +%Y-%m-%dT%H:%M:%SZ)" | tee -a "$SUMMARY"
echo "CSV,backend,query,wall_mean_ms,parse_ms,execution_ms,decode_ms,serialize_ms,bucket_sum_ms,rows" | tee -a "$SUMMARY"

for backend in louds ring; do
  run_backend "$backend" | tee -a "$SUMMARY"
done

echo ""
echo "════════════════════════════════════════════════════════"
echo " SCENARIO INTERPRETATION"
echo "════════════════════════════════════════════════════════"
python3 - "$SUMMARY" <<'PY'
import sys, re
from collections import defaultdict
rows = []
for line in open(sys.argv[1]):
    if line.startswith('CSV,') and not line.startswith('CSV,backend'):
        parts = line.strip().split(',')
        # CSV,backend,query,wall,parse,exec,dec,ser,bucket,rows
        if len(parts) >= 9:
            rows.append({
                'backend': parts[1], 'query': parts[2],
                'wall': float(parts[3]), 'parse': float(parts[4]),
                'exec': float(parts[5]), 'dec': float(parts[6]),
                'ser': float(parts[7]), 'bucket': float(parts[8]),
                'nrows': parts[9] if len(parts)>9 else '?',
            })

if not rows:
    print("No CSV rows found.")
    sys.exit(0)

# table
hdr = f"{'query':12} {'backend':7} {'wall':>9} {'parse':>8} {'exec':>8} {'decode':>9} {'serial':>9} {'bucket':>9} {'rows':>8}"
print(hdr)
print('-'*len(hdr))
by_q = defaultdict(dict)
for r in rows:
    by_q[r['query']][r['backend']] = r
    print(f"{r['query']:12} {r['backend']:7} {r['wall']:9.1f} {r['parse']:8.2f} {r['exec']:8.2f} {r['dec']:9.2f} {r['ser']:9.2f} {r['bucket']:9.2f} {r['nrows']:>8}")

print()
for q, m in by_q.items():
    if 'louds' not in m or 'ring' not in m:
        print(f"{q}: incomplete (need both backends)")
        continue
    L, R = m['louds'], m['ring']
    de = R['exec'] - L['exec']
    dd = R['dec'] - L['dec']
    ds = R['ser'] - L['ser']
    dw = R['wall'] - L['wall']
    print(f"── {q} ──")
    print(f"  Δ wall (ring-louds) = {dw:+.1f} ms")
    print(f"  Δ exec              = {de:+.1f} ms   (Phase L signal: large negative)")
    print(f"  Δ decode            = {dd:+.1f} ms")
    print(f"  Δ serialize         = {ds:+.1f} ms")
    # Scenario classification
    exec_drop = L['exec'] > 0 and (R['exec'] / L['exec']) < 0.7  # ≥30% drop
    exec_similar = abs(de) < max(5.0, 0.15 * max(L['exec'], 1.0))
    non_exec = L['dec'] + L['ser']
    exec_small = L['exec'] < 0.25 * L['wall'] if L['wall'] > 0 else False
    dominate = non_exec > 0.5 * L['wall'] if L['wall'] > 0 else False

    if exec_drop:
        scenario = "A — Phase L works: Execution drops on Ring vs LOUDS"
    elif exec_similar and dominate:
        scenario = "C — decode/serialize dominate; Execution same/small on both"
    elif exec_similar:
        scenario = "B — prepared operator not differentiating Execution (same on both)"
    else:
        scenario = "mixed / inconclusive — inspect numbers"

    # refine C if exec is small fraction even when slightly different
    if dominate and exec_small and not exec_drop:
        scenario = "C — decode/serialize dominate; Execution not the wall bottleneck"

    print(f"  SCENARIO: {scenario}")
    print(f"  LOUDS composition: exec={100*L['exec']/max(L['wall'],1e-9):.0f}%  "
          f"dec={100*L['dec']/max(L['wall'],1e-9):.0f}%  ser={100*L['ser']/max(L['wall'],1e-9):.0f}%  "
          f"parse={100*L['parse']/max(L['wall'],1e-9):.0f}%")
    print(f"  RING  composition: exec={100*R['exec']/max(R['wall'],1e-9):.0f}%  "
          f"dec={100*R['dec']/max(R['wall'],1e-9):.0f}%  ser={100*R['ser']/max(R['wall'],1e-9):.0f}%  "
          f"parse={100*R['parse']/max(R['wall'],1e-9):.0f}%")
    print()
PY

echo "Full log: $SUMMARY"
echo "Per-run artifacts: $OUT_DIR/"
