# Nova patch on qwt 0.4.0 (E5.7B.1)

**Upstream:** crates.io `qwt` 0.4.0 (`https://github.com/rossanoventurini/qwt`)  
**Workspace patch:** root `Cargo.toml` → `[patch.crates-io] qwt = { path = "vendor/qwt" }`

## Diff (narrow)

Added on `QWaveletTree`:

- `range_next_value(range, target) -> Option<T>` — guided O(log σ) successor
- `range_next_value_unchecked` — unsafe counterpart
- `range_next_value_scan` — linear `get` oracle (diagnostic)
- `range_distinct_iter(range)` — fixed-stack stateful distinct enumerator
- `RangeDistinctIter` — O(log σ) scratch, 0 persistent bytes

Does **not** expose `qvs` publicly. Zero extra persistent bytes.

## Intent

Upstreamable PR candidate for paper-faithful Ring `range_next_value` without row scans.
