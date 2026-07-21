#!/usr/bin/env python3
"""Generate RESULTS_DISK.md from raw_results_disk.csv produced by
run_comparison.sh --disk.

Same shape as generate_report.py (the in-memory comparison), but for the
disk-backed/persistent configuration of each engine:

  - Nova (louds): `--location` WAL-backed LoudsStore
  - Nova (ring): `--location` WAL-backed RingStore (same product surface)
  - Oxigraph: `--location` RocksDB-backed
  - QLever: memory-mapped disk index
  - Fluree (optional): `--storage-path`

Adds an on-disk-footprint table alongside the existing memory/CPU/latency
tables.

Optionally writes pure-stdlib SVG bar charts under charts/disk/ (--charts;
off by default) and embeds them in the Markdown report (lower is better for
latency/load/memory/disk/CPU).
"""

import argparse
import csv
import json
import os
import statistics
import sys
from collections import defaultdict

# Allow `import svg_charts` whether invoked from repo root or this directory.
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from svg_charts import (  # noqa: E402
    ENGINE_COLORS,
    ENGINE_SHORT,
    bar_chart,
    engine_items,
    grouped_bar_chart,
    parse_mem_string,
    rel_md_image,
    write_svg,
)


def pct(sorted_vals, p):
    if not sorted_vals:
        return float("nan")
    k = (len(sorted_vals) - 1) * p
    f = int(k)
    c = min(f + 1, len(sorted_vals) - 1)
    if f == c:
        return sorted_vals[f]
    return sorted_vals[f] + (sorted_vals[c] - sorted_vals[f]) * (k - f)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--csv", required=True)
    ap.add_argument("--queries", required=True)
    # Dual Nova backends (preferred). Legacy single --nova-* still accepted.
    ap.add_argument("--nova-louds-rss-kb", type=float, default=None)
    ap.add_argument("--nova-ring-rss-kb", type=float, default=None)
    ap.add_argument("--nova-louds-cpu-pct", type=float, default=None)
    ap.add_argument("--nova-ring-cpu-pct", type=float, default=None)
    ap.add_argument("--nova-louds-load-s", type=float, default=None)
    ap.add_argument("--nova-ring-load-s", type=float, default=None)
    ap.add_argument("--nova-louds-disk-kb", type=float, default=None)
    ap.add_argument("--nova-ring-disk-kb", type=float, default=None)
    # Legacy single-Nova flags (map onto whichever backend is present in CSV).
    ap.add_argument("--nova-rss-kb", type=float, default=None)
    ap.add_argument("--nova-cpu-pct", type=float, default=None)
    ap.add_argument("--nova-load-s", type=float, default=None)
    ap.add_argument("--nova-disk-kb", type=float, default=None)
    ap.add_argument("--qlever-rss-kb", type=float, default=None)
    ap.add_argument("--oxigraph-mem", default=None)  # e.g. "338.2MiB"
    ap.add_argument("--fluree-mem", default=None)  # docker stats string
    ap.add_argument("--qlever-cpu-pct", type=float, default=None)
    ap.add_argument("--oxigraph-cpu-pct", type=float, default=None)
    ap.add_argument("--fluree-cpu-pct", type=float, default=None)
    ap.add_argument("--qlever-load-s", type=float, default=None)
    ap.add_argument("--oxigraph-load-s", type=float, default=None)
    ap.add_argument("--fluree-load-s", type=float, default=None)
    ap.add_argument("--oxigraph-disk-kb", type=float, default=None)
    ap.add_argument("--qlever-disk-kb", type=float, default=None)
    ap.add_argument("--fluree-disk-kb", type=float, default=None)
    ap.add_argument("--entities", type=int, required=True)
    ap.add_argument("--triples", type=int, required=True)
    ap.add_argument(
        "--charts",
        action="store_true",
        default=False,
        help="Write SVG charts under charts/ and embed them (default: off)",
    )
    ap.add_argument("--out", required=True)
    args = ap.parse_args()

    with open(args.queries) as f:
        query_defs = json.load(f)
    query_order = [q["name"] for q in query_defs]

    data = defaultdict(list)  # (engine, query) -> [time_s, ...]
    with open(args.csv) as f:
        reader = csv.DictReader(f)
        for row in reader:
            t = row["time_s"]
            if t in ("timeout", ""):
                continue
            try:
                data[(row["engine"], row["query"])].append(float(t))
            except ValueError:
                continue

    # Discover engines from CSV so LOUDS vs Ring runs both label honestly.
    engines_seen = sorted({e for (e, _q) in data.keys()})
    preferred = [
        "nova-louds",
        "nova-ring",
        "nova",
        "oxigraph",
        "qlever",
        "fluree",
    ]
    engines = [e for e in preferred if e in engines_seen]
    engines += [e for e in engines_seen if e not in engines]

    engine_labels = {
        "nova": "Nova (louds)",  # legacy CSV rows
        "nova-louds": "Nova (louds)",
        "nova-ring": "Nova (ring)",
        "oxigraph": "Oxigraph",
        "qlever": "QLever",
        "fluree": "Fluree",
    }

    # Resolve resource metrics per engine key.
    rss_kb = {}
    cpu_pct = {
        "oxigraph": args.oxigraph_cpu_pct,
        "qlever": args.qlever_cpu_pct,
        "fluree": args.fluree_cpu_pct,
    }
    load_s = {
        "oxigraph": args.oxigraph_load_s,
        "qlever": args.qlever_load_s,
        "fluree": args.fluree_load_s,
    }
    disk_kb = {
        "oxigraph": args.oxigraph_disk_kb,
        "qlever": args.qlever_disk_kb,
        "fluree": args.fluree_disk_kb,
    }

    if args.qlever_rss_kb is not None:
        rss_kb["qlever"] = args.qlever_rss_kb

    if args.nova_louds_rss_kb is not None:
        rss_kb["nova-louds"] = args.nova_louds_rss_kb
    if args.nova_ring_rss_kb is not None:
        rss_kb["nova-ring"] = args.nova_ring_rss_kb
    if args.nova_louds_cpu_pct is not None:
        cpu_pct["nova-louds"] = args.nova_louds_cpu_pct
    if args.nova_ring_cpu_pct is not None:
        cpu_pct["nova-ring"] = args.nova_ring_cpu_pct
    if args.nova_louds_load_s is not None:
        load_s["nova-louds"] = args.nova_louds_load_s
    if args.nova_ring_load_s is not None:
        load_s["nova-ring"] = args.nova_ring_load_s
    if args.nova_louds_disk_kb is not None:
        disk_kb["nova-louds"] = args.nova_louds_disk_kb
    if args.nova_ring_disk_kb is not None:
        disk_kb["nova-ring"] = args.nova_ring_disk_kb

    # Legacy single-Nova flags: attach to whichever Nova key is present.
    if args.nova_rss_kb is not None:
        for k in ("nova-louds", "nova-ring", "nova"):
            if k in engines and k not in rss_kb:
                rss_kb[k] = args.nova_rss_kb
                break
        else:
            rss_kb["nova"] = args.nova_rss_kb
    if args.nova_cpu_pct is not None:
        for k in ("nova-louds", "nova-ring", "nova"):
            if k in engines and k not in cpu_pct:
                cpu_pct[k] = args.nova_cpu_pct
                break
        else:
            cpu_pct["nova"] = args.nova_cpu_pct
    if args.nova_load_s is not None:
        for k in ("nova-louds", "nova-ring", "nova"):
            if k in engines and k not in load_s:
                load_s[k] = args.nova_load_s
                break
        else:
            load_s["nova"] = args.nova_load_s
    if args.nova_disk_kb is not None:
        for k in ("nova-louds", "nova-ring", "nova"):
            if k in engines and k not in disk_kb:
                disk_kb[k] = args.nova_disk_kb
                break
        else:
            disk_kb["nova"] = args.nova_disk_kb

    has_fluree = "fluree" in engines
    has_ring = "nova-ring" in engines
    has_louds = bool(set(engines) & {"nova-louds", "nova"})
    n_engines = len(engines)


    out_path = os.path.abspath(args.out)
    charts_dir = os.path.join(os.path.dirname(out_path), "charts", "disk")
    chart_paths = []
    write_charts = bool(getattr(args, "charts", False))
    if write_charts:
        os.makedirs(charts_dir, exist_ok=True)

    def emit_chart(filename, svg, alt):
        """Write SVG + return markdown image, or empty string when charts disabled."""
        if not write_charts:
            return ""
        path = write_svg(os.path.join(charts_dir, filename), svg)
        chart_paths.append(path)
        return rel_md_image(out_path, path, alt)

    title_bits = [engine_labels.get(e, e).split(" (")[0] for e in engines]
    title = " vs ".join(dict.fromkeys(title_bits))

    lines = []
    lines.append(f"# Comparative Benchmark (Disk-Backed): {title}\n")
    lines.append(
        f"Dataset: {args.entities:,} synthetic BSBM-style entities "
        f"({args.triples:,} triples), identical N-Triples file loaded into "
        f"all {n_engines} engines. "
        "RDFox is mem-only in this harness and is not included on disk.\n"
    )

    lines.append("## Methodology & Storage Model\n")
    lines.append(
        "This is the **disk-backed/persistent-storage** sibling of "
        "`RESULTS_MEM.md` (the pure in-memory comparison). All engines "
        "were benchmarked over the SPARQL 1.1 HTTP Protocol using "
        "**byte-identical SPARQL query text** against a **byte-identical "
        "dataset**. Each query was run with a warm-up pass (discarded) "
        "before N timed iterations.\n"
    )
    lines.append("**Storage model per engine** (this matters — see below):\n")
    lines.append("| Engine | Storage model | Notes |")
    lines.append("|---|---|---|")
    if has_louds:
        lines.append(
            "| **Nova (louds)** | `LoudsStore::open(dir)` — WAL-backed | Every "
            "`insert()` is durably logged (fsync-per-write) to a "
            "write-ahead log before being applied in memory; periodic "
            "`compact()` merges the delta into an on-disk snapshot. "
            "CSV id: `nova-louds`. |"
        )
    if has_ring:
        lines.append(
            "| **Nova (ring)** | `RingStore::open(dir)` — WAL-backed | Same "
            "product surface as LOUDS: `--location` WAL + snapshot via "
            "`nova_serve --backend ring`. CSV id: `nova-ring`. |"
        )
    lines.append(
        "| **Oxigraph** | `serve --location <dir>` — RocksDB-backed | "
        "Oxigraph's own default/production persistent storage mode "
        "(`oxrocksdb-sys`). |"
    )
    lines.append(
        "| **QLever** | Memory-mapped disk index (mmap) | Unchanged from "
        "the in-memory comparison — QLever has no other mode. A warm-up "
        "pass ensures the OS page cache holds the working set resident "
        "before timed measurements. |"
    )
    if has_fluree:
        lines.append(
            "| **Fluree** | `fluree/server --storage-path` (host volume) | "
            "File-backed persistent ledger. SPARQL is connection-scoped; the "
            "harness injects `FROM <ledger>` into each query. |"
        )
    lines.append(
        "| **RDFox** | N/A (In-memory only) | RDFox is not disk-backed and "
        "thus excluded from this benchmark. |"
    )
    lines.append("")

    lines.append(
        "**Memory usage** is reported as *physical footprint* for "
        "Nova/QLever (macOS `vmmap -summary <pid>`'s `Physical "
        "footprint:` line — falls back to `ps -o rss` on platforms "
        "without `vmmap`) and container memory for Oxigraph/Fluree (`docker "
        "stats`). See `README.md` for the full rationale behind this "
        "choice over raw `ps -o rss`.\n"
    )

    disk_notes = []
    if has_louds:
        disk_notes.append("WAL + snapshot files for Nova (louds)")
    if has_ring:
        disk_notes.append("WAL + snapshot files for Nova (ring)")
    disk_notes.append("the full RocksDB directory for Oxigraph")
    disk_notes.append("all QLever index/permutation files")
    if has_fluree:
        disk_notes.append("Fluree storage-path contents")
    if len(disk_notes) == 1:
        disk_note_str = disk_notes[0]
    elif len(disk_notes) == 2:
        disk_note_str = f"{disk_notes[0]} and {disk_notes[1]}"
    else:
        disk_note_str = ", ".join(disk_notes[:-1]) + f", and {disk_notes[-1]}"

    lines.append(
        "**On-disk footprint** is measured via `du -sk` on each engine's "
        f"data directory after the query phase completes (includes {disk_note_str}).\n"
    )
    lines.append(
        "**CPU usage** is sampled every ~0.3s throughout each engine's "
        "query phase and averaged. Values are percent of one CPU core.\n"
    )
    lines.append(
        "**Process isolation (Nova backends).** Nova (louds) and Nova (ring) "
        "are launched as **independent fresh processes** and measured in "
        "**separate phases** (start → load → warm-up → timed queries → "
        "resource sample → kill), not selected by flipping a backend flag "
        "inside one long-running process.\n"
    )

    lines.append("")
    lines.append("## Dataset Load Time\n")
    load_blurb = (
        "Wall-clock time to load the identical N-Triples dataset and "
        "become ready to serve queries."
    )
    if has_louds:
        load_blurb += (
            " For Nova (louds) this includes WAL-logging every triple "
            "(fsync-per-write) plus a `compact()` pass — necessarily slower "
            "than the in-memory `bulk_load()` path measured in `RESULTS_MEM.md`."
        )
    if has_ring:
        load_blurb += (
            " For Nova (ring) this is parse + `bulk_load()` into a WAL-backed "
            "`--location` store (same crash-safe snapshot commit path as LOUDS)."
        )
    load_blurb += (
        " For Oxigraph this is the HTTP bulk-load POST into the "
        "RocksDB-backed store. For QLever this is the same `qlever-index` "
        "build step as the in-memory comparison (QLever's index is always "
        "disk-based)"
    )
    if has_fluree:
        load_blurb += (
            ". For Fluree this is create-ledger + N-Triples insert into "
            "`--storage-path`"
        )
    load_blurb += ".\n"
    lines.append(load_blurb)
    lines.append("| Engine | Load time |")
    lines.append("|---|---|")
    for eng in engines:
        if eng in load_s and load_s[eng] is not None:
            lines.append(f"| {engine_labels.get(eng, eng)} | {load_s[eng]:.2f} s |")

    load_vals = {e: load_s.get(e) for e in engines}
    if any(v is not None for v in load_vals.values()):
        lines.append("")
        lines.append(
            emit_chart(
                "load_time.svg",
                bar_chart(
                    "Dataset Load Time",
                    engine_items(engines, load_vals),
                    unit="s",
                    note="lower is better",
                ),
                "Dataset load time by engine (lower is better)",
            )
        )

    lines.append("")
    lines.append("## Memory Usage (Physical Footprint)\n")
    lines.append("| Engine | Memory | Storage model |")
    lines.append("|---|---|---|")
    for eng in engines:
        label = engine_labels.get(eng, eng)
        if eng in ("nova-louds", "nova") and eng in rss_kb and rss_kb[eng] is not None:
            lines.append(
                f"| {label} | {rss_kb[eng] / 1024:.1f} MiB | "
                "WAL-backed heap (recovered/compacted state resident) |"
            )
        elif eng == "nova-ring" and eng in rss_kb and rss_kb[eng] is not None:
            lines.append(
                f"| {label} | {rss_kb[eng] / 1024:.1f} MiB | "
                "WAL-backed heap (recovered/compacted state resident) |"
            )
        elif eng == "oxigraph" and args.oxigraph_mem:
            lines.append(
                f"| {label} | {args.oxigraph_mem} | RocksDB-backed "
                "(block cache + heap) |"
            )
        elif eng == "qlever" and args.qlever_rss_kb is not None:
            lines.append(
                f"| {label} | {args.qlever_rss_kb / 1024:.1f} MiB | "
                "Incl. memory-mapped index pages |"
            )
        elif eng == "fluree" and args.fluree_mem:
            lines.append(
                f"| {label} | {args.fluree_mem} | "
                "Container memory (file-backed ledger) |"
            )

    mem_vals = {}
    for eng in engines:
        if eng in ("nova-louds", "nova-ring", "nova") and eng in rss_kb and rss_kb[eng] is not None:
            mem_vals[eng] = rss_kb[eng] / 1024
        elif eng == "oxigraph" and args.oxigraph_mem:
            mem_vals[eng] = parse_mem_string(args.oxigraph_mem)
        elif eng == "qlever" and args.qlever_rss_kb is not None:
            mem_vals[eng] = args.qlever_rss_kb / 1024
        elif eng == "fluree" and args.fluree_mem:
            mem_vals[eng] = parse_mem_string(args.fluree_mem)

    lines.append("")
    lines.append(
        emit_chart(
            "memory.svg",
            bar_chart(
                "Memory Usage (Physical Footprint)",
                engine_items(engines, mem_vals),
                unit="MiB",
                note="lower is better",
            ),
            "Memory usage by engine (lower is better)",
        )
    )

    if any(disk_kb.get(e) is not None for e in engines):
        lines.append("")
        lines.append("## On-Disk Footprint\n")
        lines.append(
            "`du -sk` on each engine's data directory after the query "
            "phase (WAL + snapshot for both Nova backends, full "
            "RocksDB dir for Oxigraph, all "
            "index/permutation files for QLever"
            + (", Fluree storage-path for Fluree" if has_fluree else "")
            + ").\n"
        )
        lines.append("| Engine | On-disk size |")
        lines.append("|---|---|")
        for eng in engines:
            if disk_kb.get(eng) is not None:
                lines.append(
                    f"| {engine_labels.get(eng, eng)} | "
                    f"{disk_kb[eng] / 1024:.1f} MiB |"
                )

        disk_vals = {
            e: (disk_kb[e] / 1024) if disk_kb.get(e) is not None else None
            for e in engines
        }

        lines.append("")
        lines.append(
            emit_chart(
                "disk.svg",
                bar_chart(
                    "On-Disk Footprint",
                    engine_items(engines, disk_vals),
                    unit="MiB",
                    note="lower is better",
                ),
                "On-disk footprint by engine (lower is better)",
            )
        )

    if any(cpu_pct.get(e) is not None for e in engines):
        lines.append("")
        lines.append("## CPU Usage (average % of one core during query phase)\n")
        lines.append("| Engine | Avg CPU % |")
        lines.append("|---|---|")
        for eng in engines:
            if cpu_pct.get(eng) is not None:
                lines.append(
                    f"| {engine_labels.get(eng, eng)} | {cpu_pct[eng]:.1f}% |"
                )

        cpu_vals = {e: cpu_pct.get(e) for e in engines}
        lines.append("")
        lines.append(
            emit_chart(
                "cpu.svg",
                bar_chart(
                    "CPU Usage (avg % of one core)",
                    engine_items(engines, cpu_vals),
                    unit="%",
                    note="lower is better",
                ),
                "CPU usage by engine (lower is better)",
            )
        )

    # Precompute p50/p95 per (engine, query) for tables + charts
    p50_by_eq = {}
    p95_by_eq = {}
    for engine in engines:
        for qname in query_order:
            vals = sorted(v * 1000 for v in data.get((engine, qname), []))
            if vals:
                p50_by_eq[(engine, qname)] = pct(vals, 0.50)
                p95_by_eq[(engine, qname)] = pct(vals, 0.95)
            else:
                p50_by_eq[(engine, qname)] = None
                p95_by_eq[(engine, qname)] = None

    lines.append("")
    lines.append("## Latency Results (milliseconds, HTTP round-trip via curl)\n")
    lines.append(
        "One sub-section per query, with each engine as a column and each "
        "percentile (p50, p95) as a row. Charts use p50 latency (lower is better).\n"
    )

    series = []
    for engine in engines:
        series.append(
            (
                ENGINE_SHORT.get(engine, engine),
                [p50_by_eq[(engine, q)] for q in query_order],
                ENGINE_COLORS.get(engine, "#6b7280"),
            )
        )

    lines.append(
        emit_chart(
            "latency_p50_overview.svg",
            grouped_bar_chart(
                "Query Latency p50 (all queries)",
                query_order,
                series,
                unit="ms",
                note="lower is better",
            ),
            "p50 latency by query and engine (lower is better)",
        )
    )
    lines.append("")

    for qname in query_order:
        lines.append(f"### {qname}\n")
        header_cells = ["Metric"] + [engine_labels.get(engine, engine) for engine in engines]
        lines.append("| " + " | ".join(header_cells) + " |")
        lines.append("|" + "---|" * len(header_cells))

        p50_cells = ["p50 (ms)"]
        p95_cells = ["p95 (ms)"]

        for engine in engines:
            p50 = p50_by_eq[(engine, qname)]
            p95 = p95_by_eq[(engine, qname)]
            p50_cells.append("n/a" if p50 is None else f"{p50:.2f}")
            p95_cells.append("n/a" if p95 is None else f"{p95:.2f}")
        lines.append("| " + " | ".join(p50_cells) + " |")
        lines.append("| " + " | ".join(p95_cells) + " |")
        lines.append("")
        lines.append(
            emit_chart(
                f"latency_p50_{qname}.svg",
                bar_chart(
                    f"{qname} — p50 latency",
                    engine_items(
                        engines,
                        {e: p50_by_eq[(e, qname)] for e in engines},
                    ),
                    unit="ms",
                    note="lower is better",
                ),
                f"{qname} p50 latency (lower is better)",
            )
        )
        lines.append("")

    lines.append("## Raw per-query summary (mean, stddev, n)\n")
    lines.append(
        "One sub-section per query, with each engine as a column and each "
        "statistic (n, mean, stddev, min, max) as a row.\n"
    )
    for qname in query_order:
        lines.append(f"### {qname}\n")
        header_cells = ["Metric"] + [engine_labels.get(engine, engine) for engine in engines]
        lines.append("| " + " | ".join(header_cells) + " |")
        lines.append("|" + "---|" * len(header_cells))

        stats_by_engine = {}

        for engine in engines:
            vals = [v * 1000 for v in data.get((engine, qname), [])]
            if not vals:
                stats_by_engine[engine] = None
                continue
            stats_by_engine[engine] = {
                "n": len(vals),
                "mean": statistics.mean(vals),
                "stddev": statistics.stdev(vals) if len(vals) > 1 else 0.0,
                "min": min(vals),
                "max": max(vals),
            }

        def row(label, key, fmt):
            cells = [label]
            for engine in engines:
                s = stats_by_engine[engine]
                cells.append("n/a" if s is None else fmt(s[key]))
            lines.append("| " + " | ".join(cells) + " |")

        row("n", "n", lambda v: str(v))
        row("mean (ms)", "mean", lambda v: f"{v:.2f}")
        row("stddev (ms)", "stddev", lambda v: f"{v:.2f}")
        row("min (ms)", "min", lambda v: f"{v:.2f}")
        row("max (ms)", "max", lambda v: f"{v:.2f}")
        lines.append("")

    with open(args.out, "w") as f:
        f.write("\n".join(lines) + "\n")

    print(f"Wrote {args.out}")
    if write_charts:
        print(f"Wrote {len(chart_paths)} SVG chart(s) under {charts_dir}")
    else:
        print("SVG charts skipped (pass --charts to enable)")


if __name__ == "__main__":
    main()
