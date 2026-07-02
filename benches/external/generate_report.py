#!/usr/bin/env python3
"""Generate RESULTS.md from raw_results.csv produced by run_comparison.sh.

Computes p50/p95/mean latency per (engine, query) and renders a Markdown
comparison table, along with an explicit methodology/storage-model section
so the memory-vs-disk asymmetry between engines is never left implicit.
"""
import argparse
import csv
import json
import statistics
from collections import defaultdict


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
        "nova": "Nova (Ring+LFTJ)",
        "oxigraph": "Oxigraph (in-memory)",
        "qlever": "QLever (mmap, warmed)",
    }

    lines = []
    lines.append("# Comparative Benchmark: Nova vs Oxigraph vs QLever\n")
    lines.append(
        f"Dataset: {args.entities:,} synthetic BSBM-style entities "
        f"({args.triples:,} triples), identical N-Triples file loaded into all three engines.\n"
    )

    lines.append("## Methodology & Storage Model\n")
    lines.append(
        "All three engines were benchmarked over the SPARQL 1.1 HTTP Protocol "
        "(`curl` to each engine's `/sparql` or query endpoint) using **byte-identical "
        "SPARQL query text** against a **byte-identical dataset**. Each query was run "
        "with a warm-up pass (discarded) before N timed iterations, so all reported "
        "latencies reflect steady-state (not cold-cache) performance.\n"
    )
    lines.append(
        "**Storage model per engine** (this matters — see below):\n\n"
        "| Engine | Storage model | Notes |\n"
        "|---|---|---|\n"
        "| **Nova** | Pure in-process heap memory | No disk persistence exists at all; "
        "the whole dataset + index must fit in RAM. |\n"
        "| **Oxigraph** | Pure in-memory (`serve` run **without** `--location`) | "
        "Deliberately run in-memory (not its default RocksDB-backed mode) to match "
        "Nova's memory model — this is an apples-to-apples memory comparison, not "
        "Oxigraph's disk-persistent configuration. |\n"
        "| **QLever** | Memory-mapped disk index (mmap) | QLever has **no pure "
        "in-memory mode** — its index format is inherently a set of memory-mapped "
        "files. After the warm-up pass, the OS page cache holds the working set "
        "resident in RAM, so steady-state latency is effectively RAM-speed. This is "
        "consistent with how QLever is used and benchmarked in practice. |\n"
    )
    lines.append(
        "**Memory usage** is reported as *physical footprint* for Nova/QLever "
        "(macOS `vmmap -summary <pid>`'s `Physical footprint:` line — falls back "
        "to `ps -o rss` on platforms without `vmmap`, e.g. Linux) and container "
        "memory for Oxigraph (`docker stats`). `vmmap`'s physical footprint is "
        "used instead of raw `ps -o rss` because on macOS, `ps` RSS includes "
        "allocator-retained-but-freed memory (`libmalloc` keeps large freed "
        "regions mapped for fast reuse rather than returning them to the OS "
        "immediately) and was observed to vary by 10x+ (30-300+ MB) run-to-run "
        "for the *identical* process and workload with zero code changes -- see "
        "`CLAUDE.md`'s \"RSS investigation\" section for the full writeup. "
        "`vmmap`'s physical footprint is the same figure macOS's Activity Monitor "
        "and the kernel's own memory accounting report, and is stable across "
        "repeated runs. For QLever, this figure includes memory-mapped index "
        "pages resident via the OS page cache — architecturally different from "
        "Nova/Oxigraph's pure heap allocations, but it answers the same practical "
        "question (\"how much RAM does this process hold to serve the "
        "workload\"), so it is used as the common denominator across all three. "
        "This asymmetry is called out explicitly here rather than left implicit.\n"
    )

    lines.append(
        "**CPU usage** is sampled every ~0.3s throughout each engine's query phase "
        "(`ps -o %cpu` for Nova/QLever; `docker stats --format '{{.CPUPerc}}'` for "
        "Oxigraph) and averaged. Values are percent of one CPU core (e.g. 150% means "
        "1.5 cores kept busy on average) — this is a coarse approximation, not a "
        "precise profiler measurement, but useful for relative comparison.\n"
    )



    lines.append("")
    lines.append("## Dataset Load Time\n")
    lines.append(
        "Wall-clock time to load the identical N-Triples dataset and become ready "
        "to serve queries (includes parsing + index construction for all engines; "
        "for Nova this is parse + `compact()` into the Ring/LFTJ index, for QLever "
        "this is the separate `qlever-index` build step, for Oxigraph this is the "
        "HTTP bulk-load POST into the in-memory store).\n"
    )
    lines.append("| Engine | Load time |")
    lines.append("|---|---|")
    if args.nova_load_s is not None:
        lines.append(f"| Nova (Ring+LFTJ) | {args.nova_load_s:.2f} s |")
    if args.oxigraph_load_s is not None:
        lines.append(f"| Oxigraph (in-memory) | {args.oxigraph_load_s:.2f} s |")
    if args.qlever_load_s is not None:
        lines.append(f"| QLever (mmap, warmed) | {args.qlever_load_s:.2f} s |")

    lines.append("")
    lines.append("## Memory Usage (Physical Footprint)\n")
    lines.append(
        "Nova/QLever figures are macOS `vmmap -summary`'s \"Physical footprint\" "
        "(stable, allocator-retention-immune — see Methodology above); falls back "
        "to `ps -o rss` on non-macOS platforms.\n"
    )

    lines.append("| Engine | Memory | Storage model |")
    lines.append("|---|---|---|")
    lines.append(f"| Nova (Ring+LFTJ) | {args.nova_rss_kb / 1024:.1f} MiB | Pure heap |")
    lines.append(f"| Oxigraph (in-memory) | {args.oxigraph_mem} | Pure heap (in-memory mode) |")
    lines.append(
        f"| QLever (mmap, warmed) | {args.qlever_rss_kb / 1024:.1f} MiB | "
        "Incl. memory-mapped index pages |"
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
            lines.append(f"| Nova (Ring+LFTJ) | {args.nova_cpu_pct:.1f}% |")
        if args.oxigraph_cpu_pct is not None:
            lines.append(f"| Oxigraph (in-memory) | {args.oxigraph_cpu_pct:.1f}% |")
        if args.qlever_cpu_pct is not None:
            lines.append(f"| QLever (mmap, warmed) | {args.qlever_cpu_pct:.1f}% |")

    lines.append("")
    lines.append("## Latency Results (milliseconds, HTTP round-trip via curl)\n")
    lines.append(
        "One sub-section per query, with each engine as a column and each "
        "percentile (p50, p95) as a row.\n"
    )
    for qname in query_order:
        lines.append(f"### {qname}\n")
        header_cells = ["Metric"] + [engine_labels[engine] for engine in engines]
        lines.append("| " + " | ".join(header_cells) + " |")
        lines.append("|" + "---|" * len(header_cells))

        p50_cells = ["p50 (ms)"]
        p95_cells = ["p95 (ms)"]
        for engine in engines:
            vals = sorted(v * 1000 for v in data.get((engine, qname), []))
            if vals:
                p50_cells.append(f"{pct(vals, 0.50):.2f}")
                p95_cells.append(f"{pct(vals, 0.95):.2f}")
            else:
                p50_cells.append("n/a")
                p95_cells.append("n/a")
        lines.append("| " + " | ".join(p50_cells) + " |")
        lines.append("| " + " | ".join(p95_cells) + " |")
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


if __name__ == "__main__":
    main()
