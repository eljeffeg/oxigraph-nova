#!/usr/bin/env python3
"""Pure-stdlib SVG bar chart helpers for comparative benchmark reports.

No third-party deps (matches the rest of the external harness). Charts are
written as standalone .svg files and linked from RESULTS*.md so they render
on GitHub and in local Markdown previews.

Theme notes
-----------
Background is transparent so charts blend into the host page. Text/grid colors
use CSS classes + ``prefers-color-scheme`` rather than ``currentColor``:
embedded SVG images (Markdown ``![](...)``) do **not** inherit the page's
``color``, so ``currentColor`` just resolves to black and disappears on dark
GitHub/VS Code previews. Engine bar fills stay fixed (brand palette).
"""
from __future__ import annotations

import html
import math
import os
from typing import List, Optional, Sequence, Tuple

# Stable palette: Nova / Oxigraph / QLever / Fluree / RDFox
ENGINE_COLORS = {
    "nova": "#2563eb",  # legacy single-Nova CSV id
    "nova-louds": "#2563eb",
    "nova-ring": "#7c3aed",
    "oxigraph": "#dc2626",
    "qlever": "#16a34a",
    "fluree": "#ea580c",
    "rdfox": "#0891b2",
}

ENGINE_SHORT = {
    "nova": "Nova (louds)",
    "nova-louds": "Nova (louds)",
    "nova-ring": "Nova (ring)",
    "oxigraph": "Oxigraph",
    "qlever": "QLever",
    "fluree": "Fluree",
    "rdfox": "RDFox",
}



# Shared stylesheet: transparent bg, light-default text, dark via media query.
# Classes used by chart elements:
#   .title  — main heading / value labels
#   .muted  — subtitle / axis tick labels
#   .label  — category / legend labels
#   .grid   — horizontal grid lines
#   .axis   — plot axes
_THEME_STYLE = """\
<style>
  .title { fill: #111827; }
  .muted { fill: #6b7280; }
  .label { fill: #374151; }
  .grid  { stroke: #e5e7eb; stroke-width: 1; }
  .axis  { stroke: #9ca3af; stroke-width: 1; }
  @media (prefers-color-scheme: dark) {
    .title { fill: #f3f4f6; }
    .muted { fill: #9ca3af; }
    .label { fill: #d1d5db; }
    .grid  { stroke: #374151; }
    .axis  { stroke: #6b7280; }
  }
</style>
"""


def _nice_max(value: float) -> float:
    """Round up to a clean axis maximum."""
    if value <= 0:
        return 1.0
    exp = math.floor(math.log10(value))
    base = 10**exp
    for mult in (1, 1.5, 2, 2.5, 3, 4, 5, 6, 8, 10):
        candidate = mult * base
        if candidate >= value * 1.05:
            return candidate
    return 10 * base


def _fmt_value(v: float, unit: str) -> str:
    if unit in ("ms", "s", "%", "MiB"):
        if v >= 1000:
            return f"{v:.0f}"
        if v >= 100:
            return f"{v:.1f}"
        if v >= 10:
            return f"{v:.2f}"
        return f"{v:.2f}"
    return f"{v:.2f}"


def _escape(s: str) -> str:
    return html.escape(s, quote=True)


def _svg_open(width: int, height: int, title: str) -> List[str]:
    """Common SVG header: root, title, theme CSS. No opaque background."""
    return [
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" '
        f'viewBox="0 0 {width} {height}" role="img" '
        f'aria-label="{_escape(title)}">',
        f"<title>{_escape(title)}</title>",
        _THEME_STYLE.rstrip(),
    ]


def bar_chart(
    title: str,
    items: Sequence[Tuple[str, float, str]],
    *,
    unit: str = "",
    note: str = "lower is better",
    width: int = 640,
    height: int = 320,
) -> str:
    """Vertical bar chart for a small number of categories (e.g. 3 engines).

    items: sequence of (label, value, color). Values that are None/NaN are skipped.
    """
    clean: List[Tuple[str, float, str]] = [
        (label, float(val), color)
        for label, val, color in items
        if val is not None and not (isinstance(val, float) and math.isnan(val))
    ]
    if not clean:
        return _empty_svg(title, width, height)

    max_v = _nice_max(max(v for _, v, _ in clean))
    margin_l, margin_r, margin_t, margin_b = 56, 24, 48, 64
    plot_w = width - margin_l - margin_r
    plot_h = height - margin_t - margin_b
    n = len(clean)
    gap = plot_w * 0.12
    bar_w = (plot_w - gap * (n + 1)) / n if n else plot_w
    bar_w = max(bar_w, 24)

    parts: List[str] = _svg_open(width, height, title)
    parts.append(
        f'<text class="title" x="{width / 2}" y="22" text-anchor="middle" '
        f'font-family="system-ui, -apple-system, sans-serif" font-size="15" '
        f'font-weight="600">{_escape(title)}</text>'
    )
    if note:
        parts.append(
            f'<text class="muted" x="{width / 2}" y="40" text-anchor="middle" '
            f'font-family="system-ui, -apple-system, sans-serif" font-size="11">'
            f"{_escape(note)}</text>"
        )

    # Y grid + labels
    ticks = 4
    for i in range(ticks + 1):
        frac = i / ticks
        y = margin_t + plot_h * (1 - frac)
        val = max_v * frac
        parts.append(
            f'<line class="grid" x1="{margin_l}" y1="{y:.1f}" '
            f'x2="{width - margin_r}" y2="{y:.1f}"/>'
        )
        label = _fmt_value(val, unit)
        if unit and i == ticks:
            label = f"{label} {unit}"
        parts.append(
            f'<text class="muted" x="{margin_l - 8}" y="{y + 4:.1f}" text-anchor="end" '
            f'font-family="system-ui, -apple-system, sans-serif" font-size="11">'
            f"{label}</text>"
        )

    # Axes
    parts.append(
        f'<line class="axis" x1="{margin_l}" y1="{margin_t}" x2="{margin_l}" '
        f'y2="{margin_t + plot_h}"/>'
    )
    parts.append(
        f'<line class="axis" x1="{margin_l}" y1="{margin_t + plot_h}" '
        f'x2="{width - margin_r}" y2="{margin_t + plot_h}"/>'
    )

    for i, (label, val, color) in enumerate(clean):
        x = margin_l + gap + i * (bar_w + gap)
        bar_h = 0 if max_v == 0 else (val / max_v) * plot_h
        y = margin_t + plot_h - bar_h
        parts.append(
            f'<rect x="{x:.1f}" y="{y:.1f}" width="{bar_w:.1f}" height="{bar_h:.1f}" '
            f'fill="{color}" rx="3"/>'
        )
        # Value label above bar
        val_y = y - 6
        parts.append(
            f'<text class="title" x="{x + bar_w / 2:.1f}" y="{val_y:.1f}" '
            f'text-anchor="middle" font-family="system-ui, -apple-system, sans-serif" '
            f'font-size="11" font-weight="600">{_fmt_value(val, unit)}'
            f'{(" " + unit) if unit and bar_h < plot_h * 0.15 else ""}</text>'
        )
        # Category label
        parts.append(
            f'<text class="label" x="{x + bar_w / 2:.1f}" y="{margin_t + plot_h + 18}" '
            f'text-anchor="middle" font-family="system-ui, -apple-system, sans-serif" '
            f'font-size="12">{_escape(label)}</text>'
        )

    parts.append("</svg>")
    return "\n".join(parts) + "\n"


def grouped_bar_chart(
    title: str,
    categories: Sequence[str],
    series: Sequence[Tuple[str, Sequence[Optional[float]], str]],
    *,
    unit: str = "ms",
    note: str = "lower is better",
    width: int = 820,
    height: int = 380,
) -> str:
    """Grouped vertical bars: one group per category, one bar per series.

    series: sequence of (series_label, values_aligned_to_categories, color).
    """
    if not categories or not series:
        return _empty_svg(title, width, height)

    all_vals: List[float] = []
    for _, vals, _ in series:
        for v in vals:
            if v is not None and not (isinstance(v, float) and math.isnan(v)):
                all_vals.append(float(v))
    if not all_vals:
        return _empty_svg(title, width, height)

    max_v = _nice_max(max(all_vals))
    margin_l, margin_r, margin_t, margin_b = 56, 24, 48, 88
    plot_w = width - margin_l - margin_r
    plot_h = height - margin_t - margin_b

    n_cat = len(categories)
    n_ser = len(series)
    group_gap = plot_w * 0.06
    group_w = (plot_w - group_gap * (n_cat + 1)) / n_cat if n_cat else plot_w
    bar_gap = 2
    bar_w = (group_w - bar_gap * (n_ser - 1)) / n_ser if n_ser else group_w
    bar_w = max(min(bar_w, 36), 8)

    parts: List[str] = _svg_open(width, height, title)
    parts.append(
        f'<text class="title" x="{width / 2}" y="22" text-anchor="middle" '
        f'font-family="system-ui, -apple-system, sans-serif" font-size="15" '
        f'font-weight="600">{_escape(title)}</text>'
    )
    if note:
        parts.append(
            f'<text class="muted" x="{width / 2}" y="40" text-anchor="middle" '
            f'font-family="system-ui, -apple-system, sans-serif" font-size="11">'
            f"{_escape(note)}</text>"
        )

    ticks = 4
    for i in range(ticks + 1):
        frac = i / ticks
        y = margin_t + plot_h * (1 - frac)
        val = max_v * frac
        parts.append(
            f'<line class="grid" x1="{margin_l}" y1="{y:.1f}" '
            f'x2="{width - margin_r}" y2="{y:.1f}"/>'
        )
        label = _fmt_value(val, unit)
        if unit and i == ticks:
            label = f"{label} {unit}"
        parts.append(
            f'<text class="muted" x="{margin_l - 8}" y="{y + 4:.1f}" text-anchor="end" '
            f'font-family="system-ui, -apple-system, sans-serif" font-size="11">'
            f"{label}</text>"
        )

    parts.append(
        f'<line class="axis" x1="{margin_l}" y1="{margin_t}" x2="{margin_l}" '
        f'y2="{margin_t + plot_h}"/>'
    )
    parts.append(
        f'<line class="axis" x1="{margin_l}" y1="{margin_t + plot_h}" '
        f'x2="{width - margin_r}" y2="{margin_t + plot_h}"/>'
    )

    for ci, cat in enumerate(categories):
        group_x = margin_l + group_gap + ci * (group_w + group_gap)
        # Center bars within group when bar_w * n_ser < group_w
        used = n_ser * bar_w + (n_ser - 1) * bar_gap
        x0 = group_x + max(0, (group_w - used) / 2)

        for si, (ser_label, vals, color) in enumerate(series):
            if ci >= len(vals):
                continue
            val = vals[ci]
            if val is None or (isinstance(val, float) and math.isnan(val)):
                continue
            val = float(val)
            x = x0 + si * (bar_w + bar_gap)
            bar_h = 0 if max_v == 0 else (val / max_v) * plot_h
            y = margin_t + plot_h - bar_h
            parts.append(
                f'<rect x="{x:.1f}" y="{y:.1f}" width="{bar_w:.1f}" height="{bar_h:.1f}" '
                f'fill="{color}" rx="2">'
                f"<title>{_escape(ser_label)} / {_escape(cat)}: "
                f"{_fmt_value(val, unit)} {unit}</title></rect>"
            )

        parts.append(
            f'<text class="label" x="{group_x + group_w / 2:.1f}" '
            f'y="{margin_t + plot_h + 16}" text-anchor="middle" '
            f'font-family="system-ui, -apple-system, sans-serif" font-size="11">'
            f"{_escape(cat)}</text>"
        )

    # Legend
    legend_y = height - 28
    legend_items = [(lab, col) for lab, _, col in series]
    total_legend_w = sum(14 + 6 + len(lab) * 7 + 16 for lab, _ in legend_items)
    lx = (width - total_legend_w) / 2
    for lab, col in legend_items:
        parts.append(
            f'<rect x="{lx:.1f}" y="{legend_y - 9}" width="12" height="12" '
            f'fill="{col}" rx="2"/>'
        )
        parts.append(
            f'<text class="label" x="{lx + 18:.1f}" y="{legend_y}" '
            f'font-family="system-ui, -apple-system, sans-serif" font-size="12">'
            f"{_escape(lab)}</text>"
        )
        lx += 14 + 6 + len(lab) * 7 + 16

    parts.append("</svg>")
    return "\n".join(parts) + "\n"


def _empty_svg(title: str, width: int, height: int) -> str:
    parts = _svg_open(width, height, title)
    parts.append(
        f'<text class="muted" x="{width / 2}" y="{height / 2}" text-anchor="middle" '
        f'font-family="system-ui, sans-serif" font-size="14">'
        f"{_escape(title)} — no data</text>"
    )
    parts.append("</svg>")
    return "\n".join(parts) + "\n"


def write_svg(path: str, svg: str) -> str:
    """Write SVG to path (creating parent dirs) and return the path."""
    os.makedirs(os.path.dirname(path) or ".", exist_ok=True)
    with open(path, "w") as f:
        f.write(svg)
    return path


def rel_md_image(md_path: str, svg_path: str, alt: str) -> str:
    """Markdown image line with path relative to the markdown file."""
    md_dir = os.path.dirname(os.path.abspath(md_path)) or "."
    rel = os.path.relpath(os.path.abspath(svg_path), md_dir)
    # Normalize to forward slashes for Markdown/GitHub
    rel = rel.replace(os.sep, "/")
    return f"![{alt}]({rel})"


def engine_items(
    engines: Sequence[str],
    values: dict,
    labels: Optional[dict] = None,
) -> List[Tuple[str, float, str]]:
    """Build bar-chart items from engine id -> value mapping."""
    items = []
    for eng in engines:
        if eng not in values or values[eng] is None:
            continue
        label = (labels or ENGINE_SHORT).get(eng, eng)
        # Allow short labels override
        if labels and eng in labels:
            # Prefer short name for chart axis
            label = ENGINE_SHORT.get(eng, labels[eng])
        items.append((label, float(values[eng]), ENGINE_COLORS.get(eng, "#6b7280")))
    return items


def parse_mem_string(s: str) -> Optional[float]:
    """Parse strings like '338.7MiB' / '2.898GiB' / '92.4 MiB' into MiB."""
    if s is None:
        return None
    s = str(s).strip().replace(" ", "")
    if not s or s.lower() == "n/a":
        return None
    s_low = s.lower()
    try:
        if s_low.endswith("gib"):
            return float(s_low[:-3]) * 1024
        if s_low.endswith("mib"):
            return float(s_low[:-3])
        if s_low.endswith("kib"):
            return float(s_low[:-3]) / 1024
        if s_low.endswith("gb"):
            return float(s_low[:-2]) * 1024
        if s_low.endswith("mb"):
            return float(s_low[:-2])
        if s_low.endswith("kb"):
            return float(s_low[:-2]) / 1024
        return float(s)
    except ValueError:
        return None
