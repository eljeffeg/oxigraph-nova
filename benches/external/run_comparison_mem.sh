#!/usr/bin/env bash
# Compatibility wrapper → run_comparison.sh (in-memory mode).
# Prefer: ./run_comparison.sh [ENTITIES] [ITERS] [WARMUP]
set -euo pipefail
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
exec "$DIR/run_comparison.sh" "$@"
