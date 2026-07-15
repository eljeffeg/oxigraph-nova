#!/usr/bin/env python3
"""Generate RESULTS_DISK.md from raw_results_disk.csv produced by
run_comparison_disk.sh.

Same shape as generate_report.py (the in-memory comparison), but for the
disk-backed/persistent configuration of each engine: Nova (`--location`,
WAL-backed RingStore), Oxigraph (`--location`, RocksDB-backed), and QLever
(memory-mapped disk index, unchanged — it has no other mode). Adds an
on-disk-footprint table alongside the existing memory/CPU/latency tables.

Also writes pure-stdlib SVG bar charts under charts/disk/ and embeds them
in the Markdown report (lower is better for latency/load/memory/disk/CPU).
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
    ap.add_argument("--nova-rss-kb", type=float, required=True)
    ap.add_argument("--qlever-rss-kb", type=float, required=True)
    ap.add_argument("--oxigraph-mem", required=True)  # e.g. "338.2MiB"
    ap.add_argument("--nova-cpu-pct", type=float, default=None)
    ap.add_argument("--qlever-cpu-pct", type=float, default=None)
    ap.add_argument("--oxigraph-cpu-pct", type=float, default=None)
    ap.add_argument("--nova-load-s", type=float, default=None)
    ap.add_argument("--qlever-load-s", type=float, default=None)
    ap.add_argument("--oxigraph-load-s", type=float, default=None)
    ap.add_argument("--nova-disk-kb", type=float, default=None)
    ap.add_argument("--oxigraph-disk-kb", type=float, default=None)
    ap.add_argument("--qlever-disk-kb", type=float, default=None)
    ap.add_argument("--entities", type=int, required=True)
    ap.add_argument("--triples", type=int, required=True)
    ap.add_argument("--out", required=True)
    args = ap.parse_args()

    with open(args.queries) as f:
        query_defs = json.load(f)
    query_order = [q["name"] for q in query_defs]

    data = defaultdict(list)  # (engine, query) -> [time_s, ...]
    with open(args.csv) as f:
        reader = csv.DictReader(f)
        for row in reader:
            data[(row["engine"], row["query"])].append(float(row["time_s"]))

    engines = ["nova", "oxigraph", "qlever"]
    engine_labels = {
        "nova": "Nova (--location, WAL-backed)",
        "oxigraph": "Oxigraph (--location, RocksDB-backed)",
        "qlever": "QLever (mmap, warmed)",
    }

    out_path = os.path.abspath(args.out)
    charts_dir = os.path.join(os.path.dirname(out_path), "charts", "disk")
    os.makedirs(charts_dir, exist_ok=True)
    chart_paths = []

    def emit_chart(filename, svg, alt):
        path = write_svg(os.path.join(charts_dir, filename), svg)
        chart_paths.append(path)
        return rel_md_image(out_path, path, alt)

    lines = []
    lines.append("# Comparative Benchmark (Disk-Backed): Nova vs Oxigraph vs QLever\n")
    lines.append(
        f"Dataset: {args.entities:,} synthetic BSBM-style entities "
        f"({args.triples:,} triples), identical N-Triples file loaded into all three engines.\n"
    )

    lines.append("## Methodology & Storage Model\n")
    lines.append(
        "This is the **disk-backed/persistent-storage** sibling of "
        "`RESULTS.md` (the pure in-memory comparison). All three engines "
        "were benchmarked over the SPARQL 1.1 HTTP Protocol using "
        "**byte-identical SPARQL query text** against a **byte-identical "
        "dataset**. Each query was run with a warm-up pass (discarded) "
        "before N timed iterations.\n"
    )
    lines.append(
        "**Storage model per engine** (this matters — see below):\n\n"
        "| Engine | Storage model | Notes |\n"
        "|---|---|---|\n"
        "| **Nova** | `RingStore::open(dir)` — WAL-backed | Every "
        "`insert()` is durably logged (fsync-per-write) to a "
        "write-ahead log before being applied in memory; periodic "
        "`compact()` merges the delta into an ε-serde snapshot on disk. |\n"
        "| **Oxigraph** | `serve --location <dir>` — RocksDB-backed | "
        "Oxigraph's own default/production persistent storage mode "
        "(`oxrocksdb-sys`). |\n"
        "| **QLever** | Memory-mapped disk index (mmap) | Unchanged from "
        "the in-memory comparison — QLever has no other mode. A warm-up "
        "pass ensures the OS page cache holds the working set resident "
        "before timed measurements. |\n"
    )
    lines.append(
        "**Memory usage** is reported as *physical footprint* for "
        "Nova/QLever (macOS `vmmap -summary <pid>`'s `Physical "
        "footprint:` line — falls back to `ps -o rss` on platforms "
        "without `vmmap`) and container memory for Oxigraph (`docker "
        "stats`). See `README.md` for the full rationale behind this "
        "choice over raw `ps -o rss`.\n"
    )

    lines.append(
        "**On-disk footprint** is measured via `du -sk` on each engine's "
        "data directory after the query phase completes (includes WAL + "
        "snapshot files for Nova, the full RocksDB directory for "
        "Oxigraph, and all QLever index/permutation files).\n"
    )
    lines.append(
        "**CPU usage** is sampled every ~0.3s throughout each engine's "
        "query phase and averaged. Values are percent of one CPU core.\n"
    )

    lines.append("")
    lines.append("## Dataset Load Time\n")
    lines.append(
        "Wall-clock time to load the identical N-Triples dataset and "
        "become ready to serve queries. For Nova this includes WAL-logging "
        "every triple (fsync-per-write) plus a `compact()` pass — "
        "necessarily slower than the in-memory `bulk_load()` path measured "
        "in `RESULTS.md`. For Oxigraph this is the HTTP bulk-load POST into "
        "the RocksDB-backed store. For QLever this is the same "
        "`qlever-index` build step as the in-memory comparison (QLever's "
        "index is always disk-based).\n"
    )
    lines.append("| Engine | Load time |")
    lines.append("|---|---|")
    if args.nova_load_s is not None:
        lines.append(f"| Nova (--location) | {args.nova_load_s:.2f} s |")
    if args.oxigraph_load_s is not None:
        lines.append(f"| Oxigraph (--location) | {args.oxigraph_load_s:.2f} s |")
    if args.qlever_load_s is not None:
        lines.append(f"| QLever (mmap, warmed) | {args.qlever_load_s:.2f} s |")

    load_vals = {
        "nova": args.nova_load_s,
        "oxigraph": args.oxigraph_load_s,
        "qlever": args.qlever_load_s,
    }
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
    lines.append(
        f"| Nova (--location) | {args.nova_rss_kb / 1024:.1f} MiB | "
        "WAL-backed heap (recovered/compacted state resident) |"
    )
    lines.append(
        f"| Oxigraph (--location) | {args.oxigraph_mem} | RocksDB-backed "
        "(block cache + heap) |"
    )
    lines.append(
        f"| QLever (mmap, warmed) | {args.qlever_rss_kb / 1024:.1f} MiB | "
        "Incl. memory-mapped index pages |"
    )

    mem_vals = {
        "nova": args.nova_rss_kb / 1024,
        "oxigraph": parse_mem_string(args.oxigraph_mem),
        "qlever": args.qlever_rss_kb / 1024,
    }
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

    if args.nova_disk_kb is not None or args.oxigraph_disk_kb is not None or args.qlever_disk_kb is not None:
        lines.append("")
        lines.append("## On-Disk Footprint\n")
        lines.append(
            "`du -sk` on each engine's data directory after the query "
            "phase (WAL + snapshot for Nova, full RocksDB dir for "
            "Oxigraph, all index/permutation files for QLever).\n"
        )
        lines.append("| Engine | On-disk size |")
        lines.append("|---|---|")
        if args.nova_disk_kb is not None:
            lines.append(f"| Nova (--location) | {args.nova_disk_kb / 1024:.1f} MiB |")
        if args.oxigraph_disk_kb is not None:
            lines.append(f"| Oxigraph (--location) | {args.oxigraph_disk_kb / 1024:.1f} MiB |")
        if args.qlever_disk_kb is not None:
            lines.append(f"| QLever (mmap, warmed) | {args.qlever_disk_kb / 1024:.1f} MiB |")

        disk_vals = {
            "nova": (args.nova_disk_kb / 1024) if args.nova_disk_kb is not None else None,
            "oxigraph": (args.oxigraph_disk_kb / 1024) if args.oxigraph_disk_kb is not None else None,
            "qlever": (args.qlever_disk_kb / 1024) if args.qlever_disk_kb is not None else None,
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

    if (
        args.nova_cpu_pct is not None
        or args.qlever_cpu_pct is not None
        or args.oxigraph_cpu_pct is not None
    ):
        lines.append("")
        lines.append("## CPU Usage (average % of one core during query phase)\n")
        lines.append("| Engine | Avg CPU % |")
        lines.append("|---|---|")
        if args.nova_cpu_pct is not None:
            lines.append(f"| Nova (--location) | {args.nova_cpu_pct:.1f}% |")
        if args.oxigraph_cpu_pct is not None:
            lines.append(f"| Oxigraph (--location) | {args.oxigraph_cpu_pct:.1f}% |")
        if args.qlever_cpu_pct is not None:
            lines.append(f"| QLever (mmap, warmed) | {args.qlever_cpu_pct:.1f}% |")

        cpu_vals = {
            "nova": args.nova_cpu_pct,
            "oxigraph": args.oxigraph_cpu_pct,
            "qlever": args.qlever_cpu_pct,
        }
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
                ENGINE_SHORT[engine],
                [p50_by_eq[(engine, q)] for q in query_order],
                ENGINE_COLORS[engine],
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
        header_cells = ["Metric"] + [engine_labels[engine] for engine in engines]
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
        header_cells = ["Metric"] + [engine_labels[engine] for engine in engines]
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
    print(f"Wrote {len(chart_paths)} SVG chart(s) under {charts_dir}")


if __name__ == "__main__":
    main()
