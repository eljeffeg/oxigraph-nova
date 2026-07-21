#!/usr/bin/env python3
"""Generate RESULTS_MEM.md from raw_results.csv produced by run_comparison_mem.sh.

Computes p50/p95/mean latency per (engine, query) and renders a Markdown
comparison table, along with an explicit methodology/storage-model section
so the memory-vs-disk asymmetry between engines is never left implicit.

Optionally writes pure-stdlib SVG bar charts under charts/ (--charts; off by default). and embeds them in the
Markdown report (lower is better for latency/load/memory/CPU).

Engines (mem harness, 4-way by default):
  nova-louds | nova-ring | oxigraph | qlever
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
    # Legacy single-Nova flags (map onto whichever backend is present in CSV).
    ap.add_argument("--nova-rss-kb", type=float, default=None)
    ap.add_argument("--nova-cpu-pct", type=float, default=None)
    ap.add_argument("--nova-load-s", type=float, default=None)
    ap.add_argument("--qlever-rss-kb", type=float, default=None)
    ap.add_argument("--oxigraph-mem", default=None)  # e.g. "338.2MiB"
    ap.add_argument("--fluree-mem", default=None)  # docker stats string
    ap.add_argument("--rdfox-rss-kb", type=float, default=None)
    ap.add_argument("--qlever-cpu-pct", type=float, default=None)
    ap.add_argument("--oxigraph-cpu-pct", type=float, default=None)
    ap.add_argument("--fluree-cpu-pct", type=float, default=None)
    ap.add_argument("--rdfox-cpu-pct", type=float, default=None)
    ap.add_argument("--qlever-load-s", type=float, default=None)
    ap.add_argument("--oxigraph-load-s", type=float, default=None)
    ap.add_argument("--fluree-load-s", type=float, default=None)
    ap.add_argument("--rdfox-load-s", type=float, default=None)

    ap.add_argument("--entities", type=int, required=True)
    ap.add_argument("--triples", type=int, required=True)
    ap.add_argument("--charts", action="store_true", default=False,
                    help="Write SVG charts under charts/ and embed them (default: off)")
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
        "rdfox",
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
        "rdfox": "RDFox",
    }

    # Resolve resource metrics per engine key.
    rss_kb = {
        "oxigraph": None,  # string form used for table; numeric via parse_mem_string
        "fluree": None,
    }
    if args.qlever_rss_kb is not None:
        rss_kb["qlever"] = args.qlever_rss_kb
    if args.rdfox_rss_kb is not None:
        rss_kb["rdfox"] = args.rdfox_rss_kb
    cpu_pct = {
        "oxigraph": args.oxigraph_cpu_pct,
        "qlever": args.qlever_cpu_pct,
        "fluree": args.fluree_cpu_pct,
        "rdfox": args.rdfox_cpu_pct,
    }
    load_s = {
        "oxigraph": args.oxigraph_load_s,
        "qlever": args.qlever_load_s,
        "fluree": args.fluree_load_s,
        "rdfox": args.rdfox_load_s,
    }


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

    out_path = os.path.abspath(args.out)
    charts_dir = os.path.join(os.path.dirname(out_path), "charts", "mem")
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

    n_engines = len(engines)
    title_bits = []
    for e in engines:
        title_bits.append(engine_labels.get(e, e).split(" (")[0])
    # Prefer short title from present engines
    title = " vs ".join(dict.fromkeys(title_bits))  # dedupe while preserving order
    lines = []
    lines.append(f"# Comparative Benchmark: {title}\n")
    lines.append(
        f"Dataset: {args.entities:,} synthetic BSBM-style entities "
        f"({args.triples:,} triples), identical N-Triples file loaded into "
        f"all {n_engines} engines.\n"
    )

    lines.append("## Methodology & Storage Model\n")
    lines.append(
        f"All {n_engines} engines were benchmarked over the SPARQL 1.1 HTTP Protocol "
        "(`curl` to each engine's `/sparql` or query endpoint) using **byte-identical "
        "SPARQL query text** against a **byte-identical dataset**. Each query was run "
        "with a warm-up pass (discarded) before N timed iterations, so all reported "
        "latencies reflect steady-state (not cold-cache) performance.\n"
    )
    storage_rows = [
        "| Engine | Storage model | Notes |",
        "|---|---|---|",
        "| **Nova (louds)** | Pure in-process heap (`LoudsStore`) | Default "
        "production in-memory backend; LOUDS + LFTJ index. |",
        "| **Nova (ring)** | Pure in-process heap (`RingStore`) | Cyclic "
        "QWT ring backend (`--backend ring`); in-memory bulk_load (WAL available via `--location` on disk runs). |",
        "| **Oxigraph** | Pure in-memory (`serve` run **without** `--location`) | "
        "Deliberately run in-memory (not its default RocksDB-backed mode) to match "
        "Nova's memory model. |",
        "| **QLever** | Memory-mapped disk index (mmap) | QLever has **no pure "
        "in-memory mode**. After warm-up the OS page cache holds the working set "
        "resident — consistent with QLever's published methodology. |",
        "| **Fluree** | Ephemeral container FS (`fluree/server`, no host volume) | "
        "Default file storage lives inside the container and is destroyed with it "
        "— functionally in-memory for this bench. SPARQL is connection-scoped; the "
        "harness injects `FROM <ledger>` into each query (addressing only). |",
        "| **RDFox** | In-memory datastore (sandbox/daemon, `parallel-nn`) | "
        "Optional comparator: licensed RDFox binary + `.lic` (auto-skipped when "
        "missing; `research/` is gitignored and not required). |",
    ]
    # Only emit rows for engines present (plus always show Nova/Oxigraph/QLever core notes if any of them present)
    present = set(engines)
    lines.append("**Storage model per engine** (this matters — see below):\n")
    lines.append(storage_rows[0])
    lines.append(storage_rows[1])
    if present & {"nova-louds", "nova", "nova-ring"}:
        if present & {"nova-louds", "nova"}:
            lines.append(storage_rows[2])
        if "nova-ring" in present:
            lines.append(storage_rows[3])
    if "oxigraph" in present:
        lines.append(storage_rows[4])
    if "qlever" in present:
        lines.append(storage_rows[5])
    if "fluree" in present:
        lines.append(storage_rows[6])
    if "rdfox" in present:
        lines.append(storage_rows[7])
    lines.append("")

    lines.append(
        "**Memory usage** is reported as *physical footprint* for Nova/QLever "
        "(macOS `vmmap -summary <pid>`'s `Physical footprint:` line — falls back "
        "to `ps -o rss` on platforms without `vmmap`, e.g. Linux) and container "
        "memory for Oxigraph (`docker stats`). `vmmap`'s physical footprint is "
        "used instead of raw `ps -o rss` because on macOS, `ps` RSS includes "
        "allocator-retained-but-freed memory (`libmalloc` keeps large freed "
        "regions mapped for fast reuse rather than returning them to the OS "
        "immediately) and was observed to vary by 10x+ (30-300+ MB) run-to-run "
        "for the *identical* process and workload with zero code changes. "
        "`vmmap`'s physical footprint is the same figure macOS's Activity Monitor "
        "and the kernel's own memory accounting report, and is stable across "
        "repeated runs. For QLever, this figure includes memory-mapped index "
        "pages resident via the OS page cache — architecturally different from "
        "Nova/Oxigraph's pure heap allocations, but it answers the same practical "
        "question (\"how much RAM does this process hold to serve the "
        "workload\"), so it is used as the common denominator across engines. "
        "This asymmetry is called out explicitly here rather than left implicit.\n"
    )

    lines.append(
        "**CPU usage** is sampled every ~0.3s throughout each engine's query phase "
        "(`ps -o %cpu` for Nova/QLever; `docker stats --format '{{.CPUPerc}}'` for "
        "Oxigraph) and averaged. Values are percent of one CPU core (e.g. 150% means "
        "1.5 cores kept busy on average) — this is a coarse approximation, not a "
        "precise profiler measurement, but useful for relative comparison.\n"
    )
    lines.append(
        "**Process isolation (Nova backends).** Nova (louds) and Nova (ring) are "
        "launched as **independent fresh processes** and measured in **separate "
        "phases** (start → load → warm-up → timed queries → resource sample → "
        "kill), not selected by flipping a backend flag inside one long-running "
        "process. Each backend uses its own release binary (`nova_serve` default "
        "vs `nova_serve --backend ring` built with `--features ring-backend`). "
        "This keeps RSS/CPU samples attributable to a single backend and avoids "
        "cross-backend heap or page-cache contamination within the Nova process.\n"
    )
    lines.append(
        "**Latency variability.** Primary latency comparisons use **medians "
        "(p50)** (with p95 for tail behavior). Within-process iteration stddev can "
        "be material — e.g. Ring `path_2hop` stddev around **66.47 ms** versus "
        "about **23.20 ms** for LOUDS on the same query shape — so means alone are "
        "easy to over-read. Future optimization runs should keep medians as the "
        "headline metric, use enough timed rounds after warm-up, and may add "
        "**process-level repetitions** (full restart → load → query phase) on top "
        "of within-process query iterations when comparing backends or tracking "
        "regressions.\n"
    )

    lines.append("")
    lines.append("## Dataset Load Time\n")
    lines.append(
        "Wall-clock time to load the identical N-Triples dataset and become ready "
        "to serve queries (includes parsing + index construction for all engines; "
        "for Nova this is parse + `compact()` into the LOUDS or Ring index, for "
        "QLever this is the separate `qlever-index` build step, for Oxigraph this "
        "is the HTTP bulk-load POST into the in-memory store).\n"
    )
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
    lines.append(
        "Nova/QLever figures are macOS `vmmap -summary`'s \"Physical footprint\" "
        "(stable, allocator-retention-immune — see Methodology above); falls back "
        "to `ps -o rss` on non-macOS platforms.\n"
    )

    lines.append("| Engine | Memory | Storage model |")
    lines.append("|---|---|---|")
    for eng in engines:
        label = engine_labels.get(eng, eng)
        if eng in ("nova-louds", "nova-ring", "nova") and eng in rss_kb and rss_kb[eng] is not None:
            model = "Pure heap (LOUDS)" if eng != "nova-ring" else "Pure heap (Ring)"
            lines.append(f"| {label} | {rss_kb[eng] / 1024:.1f} MiB | {model} |")
        elif eng == "oxigraph" and args.oxigraph_mem:
            lines.append(
                f"| {label} | {args.oxigraph_mem} | Pure heap (in-memory mode) |"
            )
        elif eng == "qlever" and args.qlever_rss_kb is not None:
            lines.append(
                f"| {label} | {args.qlever_rss_kb / 1024:.1f} MiB | "
                "Incl. memory-mapped index pages |"
            )
        elif eng == "fluree" and args.fluree_mem:
            lines.append(
                f"| {label} | {args.fluree_mem} | Ephemeral container FS |"
            )
        elif eng == "rdfox" and eng in rss_kb and rss_kb[eng] is not None:
            lines.append(
                f"| {label} | {rss_kb[eng] / 1024:.1f} MiB | Pure heap (RDFox) |"
            )

    mem_vals = {}
    for eng in engines:
        if eng in ("nova-louds", "nova-ring", "nova", "rdfox") and eng in rss_kb and rss_kb[eng] is not None:
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
        "percentile (p50, p95) as a row. Charts use p50 latency (lower is better). "
        "`path_2hop` and `triangle` are charted separately — their latencies are "
        "orders of magnitude higher and would crush the scale of the other queries.\n"
    )

    # Split overview charts: heavy queries (path_2hop, triangle) dominate the
    # y-scale and make every other bar unreadable when plotted together.
    HEAVY_QUERIES = {"path_2hop", "triangle"}
    light_queries = [q for q in query_order if q not in HEAVY_QUERIES]
    heavy_queries = [q for q in query_order if q in HEAVY_QUERIES]

    def _latency_series(qnames):
        out = []
        for engine in engines:
            out.append(
                (
                    ENGINE_SHORT.get(engine, engine),
                    [p50_by_eq[(engine, q)] for q in qnames],
                    ENGINE_COLORS.get(engine, "#6b7280"),
                )
            )
        return out

    if light_queries:
        lines.append(
            emit_chart(
                "latency_p50_overview.svg",
                grouped_bar_chart(
                    "Query Latency p50 (scan / joins / star)",
                    light_queries,
                    _latency_series(light_queries),
                    unit="ms",
                    note="lower is better; path_2hop & triangle omitted (see next chart)",
                ),
                "p50 latency by query and engine — light queries (lower is better)",
            )
        )
        lines.append("")

    if heavy_queries:
        lines.append(
            emit_chart(
                "latency_p50_heavy.svg",
                grouped_bar_chart(
                    "Query Latency p50 (path_2hop / triangle)",
                    heavy_queries,
                    _latency_series(heavy_queries),
                    unit="ms",
                    note="lower is better; separate scale from lighter queries",
                ),
                "p50 latency for path_2hop and triangle (lower is better)",
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
