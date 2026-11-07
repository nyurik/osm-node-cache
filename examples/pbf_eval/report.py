#!/usr/bin/env python3
"""Makes the README's performance chart (SVG) and tables (Markdown) from `pbf_eval` logs.

Usage: report.py <log dir> <chart.svg> <tables.md>

The log dir is what measure.sh wrote:
<log dir>/regions.txt: the names of the small and large extracts, and the large one's ID limit
<log dir>/mem/<region>-<store>-<run>.txt: `build`, `verify` and `bench` of a store that fits in
memory, run more than once; the tables show the better run and how far apart the runs were.
<log dir>/whole/<region>-<store>.txt: `load` of a whole extract, for its size and build time, and
for the planet a `resolve` with a store larger than the free memory.
Each log of a store kept in a file has `== disk` and `== apparent` sections from `du -cB1` of its
files. A memory store has no file; its logs give the memory its nodes take instead.
"""

import re
import sys
from pathlib import Path

STORES = [("cache", "PagedStore"), ("redb", "RedbStore"), ("dense", "DenseStore"), ("mem", "MemoryStore")]
# set from regions.txt by main
WHOLE, MEM, MEM_TITLES = [], [], {}


def read_regions(d):
    """The regions' names, from what measure.sh wrote."""
    names = dict(line.split("=", 1) for line in (d / "regions.txt").read_text().splitlines() if "=" in line)
    small, large, limit = names["small"], names["large"], int(names["slice"])
    WHOLE[:] = [("small", small), ("large", large), ("planet", "Planet")]
    MEM[:] = [("small", small), ("slice", f"{large}, IDs to {limit / 1e9:g} B")]
    MEM_TITLES.update({"small": small, "slice": f"{large}, the nodes up to ID {limit:,}"})
MORE_IS_BETTER = {"build_mnps", "resolve_mps", "verify_mnps", "way_mps", "random_mps", "way_cold_mps"}


def num(s):
    return float(s.replace(",", ""))


def parse(path):
    """The measurements in one log; a run stopped early has only some of them."""
    text = path.read_text()
    r = {}
    if m := re.search(r"built (\d+) nodes in ([\d.]+) s", text):
        r["nodes"], r["build_s"] = int(m[1]), num(m[2])
        r["build_mnps"] = r["nodes"] / r["build_s"] / 1e6
    if m := re.search(r"the store took ([\d.]+) s of the load", text):
        r["store_s"] = num(m[1])
    if m := re.search(r"preloaded the store in ([\d.]+) s", text):
        r["preload_s"] = num(m[1])
    if m := re.search(r"resolved (\d+) ways' (\d+) node refs, (\d+) missing, in ([\d.]+) s", text):
        r["ways"], r["refs"], r["missing"], r["resolve_s"] = int(m[1]), int(m[2]), int(m[3]), num(m[4])
        r["resolve_mps"] = r["refs"] / r["resolve_s"] / 1e6
    for cmd, rss in re.findall(r"== \S+ (?:--[\w-]+ (?:\d+ )?)*(\w+) .*?wall [\d.]+ s, peak RSS (\d+) KB", text, re.S):
        r[f"{cmd}_rss_gb"] = int(rss) * 1024 / 1e9
    r.setdefault("build_rss_gb", r.get("load_rss_gb"))

    def du(section):
        part = text.split(f"== {section}\n")
        return int(part[1].split()[0]) if len(part) > 1 else None

    # allocated, not apparent: files grow in steps they have not written yet
    r["disk_bytes"], r["apparent_bytes"] = du("disk"), du("apparent")
    if r["disk_bytes"] and "nodes" in r:
        r["bytes_per_node"] = r["disk_bytes"] / r["nodes"]
    if m := re.search(r"^built: \d+ nodes, ([\d.]+) GB in memory, ([\d.]+) bytes per node", text, re.M):
        r["memory_bytes"], r["memory_per_node"] = num(m[1]) * 1e9, num(m[2])
    if m := re.search(r"(\d+) nodes checked, (\d+) wrong, in ([\d.]+) s", text):
        r["verify_wrong"], r["verify_s"] = int(m[2]), num(m[3])
        r["verify_mnps"] = int(m[1]) / r["verify_s"] / 1e6
    names = {"ways' nodes in way order": "way_cold", "the same nodes in random order": "random",
             "ways' nodes in way order, after the edits": "way"}
    for label, ns, mps in re.findall(r"^(.*?): \d+ lookups, [\d.]+% found; (\d+) ns each on one thread, ([\d.]+) M/s",
                                     text, re.M):
        r[f"{names[label]}_ns"], r[f"{names[label]}_mps"] = int(ns), num(mps)
    if m := re.search(r"edits in \d+ commits of \d+: [\d.]+ s, ([\d.]+) µs per edit; per commit median ([\d.]+) ms, "
                      r"99th percentile ([\d.]+) ms", text):
        r["edit_us"], r["commit_median_ms"], r["commit_p99_ms"] = num(m[1]), num(m[2]), num(m[3])
    if m := re.search(r"(\d+) edited nodes checked, (\d+) wrong", text):
        r["edits_wrong"] = int(m[2])
    for k in ["missing", "verify_wrong", "edits_wrong"]:
        assert r.get(k, 0) == 0, (path, k)
    return {k: v for k, v in r.items() if v is not None}


def best(runs):
    """The better of each measurement over the runs, and how far apart the runs were, as a fraction."""
    out, spread = {}, {}
    for k in runs[0]:
        vals = [r[k] for r in runs if r.get(k) is not None]
        if not vals or not isinstance(vals[0], (int, float)):
            continue
        out[k] = max(vals) if k in MORE_IS_BETTER else min(vals)
        spread[k] = (max(vals) - min(vals)) / out[k] if out[k] else 0
    return out, spread


def load(d):
    mem, spreads, whole = {}, {}, {}
    for region, _ in MEM:
        for store, _ in STORES:
            runs = [parse(f) for f in sorted((d / "mem").glob(f"{region}-{store}-*.txt")) if "DONE" in f.read_text()]
            if runs:
                mem[(region, store)], spreads[(region, store)] = best(runs)
    for f in sorted((d / "whole").glob("*-*.txt")):
        region, store = f.stem.split("-")
        whole[(region, store)] = parse(f)
    # the small extract is whole in the runs in memory
    for store, _ in STORES:
        if ("small", store) in mem:
            whole[("small", store)] = mem[("small", store)]
    return mem, spreads, whole


# chart panels: key, title, unit, whether more is better, value format, which results, regions
PANELS = [
    ("bytes_per_node", "Size on disk", "bytes per node", False, "{:.1f}", "whole", WHOLE),
    ("build_mnps", "Bulk load from the PBF file", "M nodes/s", True, "{:.1f}", "mem", MEM),
    ("resolve_mps", "Resolve the nodes of all ways", "M node refs/s, PBF decoding included", True, "{:.1f}", "mem", MEM),
    ("way_mps", "Lookups in way order", "M/s on 16 threads", True, "{:.0f}", "mem", MEM),
    ("random_mps", "Lookups in random order", "M/s on 16 threads", True, "{:.0f}", "mem", MEM),
    ("commit_median_ms", "Commit 2,000 changes", "ms, median", False, "{:.0f}", "mem", MEM),
]

# the first four categorical colors of a palette checked for color vision deficiencies, in light and dark
STYLE = """
  .surface { fill: #fcfcfb; }
  .heading { fill: #0b0b0b; font-family: system-ui, sans-serif; font-size: 17px; font-weight: 600; }
  .title { fill: #0b0b0b; font-family: system-ui, sans-serif; font-size: 15px; font-weight: 600; }
  .note { fill: #52514e; font-family: system-ui, sans-serif; font-size: 12px; }
  .label { fill: #52514e; font-family: system-ui, sans-serif; font-size: 12px; }
  .value { fill: #0b0b0b; font-family: system-ui, sans-serif; font-size: 11px; }
  .axis { stroke: #c3c2b7; stroke-width: 1; }
  .s0 { fill: #2a78d6; } .s1 { fill: #eb6834; } .s2 { fill: #1baf7a; } .s3 { fill: #eda100; }
  @media (prefers-color-scheme: dark) {
    .surface { fill: #1a1a19; }
    .heading, .title, .value { fill: #f0efec; }
    .note, .label { fill: #c3c2b7; }
    .axis { stroke: #52514e; }
    .s0 { fill: #3987e5; } .s1 { fill: #d95926; } .s2 { fill: #199e70; } .s3 { fill: #c98500; }
  }
"""


def bar(x, y, w, h, cls):
    """A horizontal bar from `x`, square at the baseline and rounded at its end."""
    r = min(3, w / 2, h / 2)
    return (f'<path class="{cls}" d="M{x:.1f},{y:.1f} h{w - r:.1f} a{r},{r} 0 0 1 {r},{r} v{h - 2 * r:.1f} '
            f'a{r},{r} 0 0 1 {-r},{r} h{-(w - r):.1f} z"/>')


def chart(mem, whole, path):
    pw, gap, label_w = 430, 28, 150
    bar_h, pair_gap, group_gap = 10, 2, 22
    group_h = len(STORES) * (bar_h + pair_gap) + group_gap
    top, cols = 70, 2
    heights = [52 + len(p[6]) * group_h for p in PANELS]
    rows = [max(heights[i:i + cols]) for i in range(0, len(PANELS), cols)]
    width = cols * pw + (cols - 1) * gap + 32
    height = top + sum(rows) + 8
    out = [f'<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" viewBox="0 0 {width} {height}" '
           'role="img" aria-label="The four stores compared: size, bulk load, way resolving, lookups and commits">',
           f"<style>{STYLE}</style>", f'<rect class="surface" width="{width}" height="{height}" rx="8"/>',
           '<text class="heading" x="16" y="30">osmnodecache stores</text>']
    lx = 16
    for i, (_, name) in enumerate(STORES):
        out.append(f'<rect class="s{i}" x="{lx}" y="44" width="12" height="12" rx="3"/>')
        out.append(f'<text class="label" x="{lx + 18}" y="54">{name}</text>')
        lx += 18 + 8 * len(name) + 14
    out.append(f'<text class="note" x="{lx + 6}" y="54">16 vCPU, 64 GiB; speeds with the store in memory, '
               'better of two runs</text>')
    py = top
    for row, row_h in enumerate(rows):
        for col in range(cols):
            p = row * cols + col
            if p >= len(PANELS):
                break
            key, title, unit, more, fmt, src, regions = PANELS[p]
            res = whole if src == "whole" else mem
            px = 16 + col * (pw + gap)
            better = "higher is better" if more else "lower is better"
            out.append(f'<text class="title" x="{px}" y="{py + 18}">{title}</text>')
            out.append(f'<text class="note" x="{px}" y="{py + 36}">{unit}; {better}</text>')
            vals = [res[(rg, st)][key] for rg, _ in regions for st, _ in STORES if key in res.get((rg, st), {})]
            vmax = max(vals) if vals else 1
            plot_w = pw - label_w - 46
            x0, y = px + label_w, py + 52
            for rg, rname in regions:
                out.append(f'<text class="label" x="{px}" y="{y + group_h / 2 - 8}">{rname}</text>')
                for i, (st, _) in enumerate(STORES):
                    v = res.get((rg, st), {}).get(key)
                    by = y + i * (bar_h + pair_gap)
                    in_memory = res.get((rg, st), {}).get("memory_per_node")
                    if key == "bytes_per_node" and in_memory:
                        out.append(f'<text class="note" x="{x0 + 4}" y="{by + 9}" style="font-size:10px">'
                                   f'no file; {in_memory:.1f} bytes per node in memory</text>')
                        continue
                    if v is None:
                        out.append(f'<text class="note" x="{x0 + 4}" y="{by + 9}" style="font-size:10px">–</text>')
                        continue
                    w = max(plot_w * v / vmax, 2)
                    out.append(bar(x0, by, w, bar_h, f"s{i}"))
                    out.append(f'<text class="value" x="{x0 + w + 4:.1f}" y="{by + 9}">{fmt.format(v)}</text>')
                y += group_h
            out.append(f'<line class="axis" x1="{x0}" y1="{py + 48}" x2="{x0}" y2="{y - group_gap + 4}"/>')
        py += row_h
    out.append("</svg>")
    path.write_text("\n".join(out) + "\n")


def gb(b):
    return f"{b / 1e9:.1f} GB" if b >= 1e9 else f"{b / 1e6:.0f} MB"


def secs(s):
    if s < 1:
        return f"{s * 1000:.0f} ms"
    return f"{s:.1f} s" if s < 100 else f"{s / 60:.1f} min" if s >= 600 else f"{s:.0f} s"


def resolved(r):
    """How long resolving all ways took, with reading the store into memory first if it was."""
    cell = f"{secs(r['resolve_s'])}, {r['resolve_mps']:.1f} M refs/s"
    if "preload_s" in r:
        cell += f" ({secs(r['preload_s'])} of it reading the store into memory)"
    return cell


def table(header, rows):
    lines = ["| " + " | ".join(header) + " |", "|---|" + "---:|" * (len(header) - 1)]
    lines += ["| " + " | ".join(r) + " |" for r in rows]
    return "\n".join(lines) + "\n"


def tables(mem, spreads, whole, path):
    out = ["Whole extracts:\n\n"]
    rows = []
    for rg, rname in WHOLE:
        for st, sname in STORES:
            r = whole.get((rg, st))
            if not r or "nodes" not in r:
                continue
            if "memory_bytes" in r:
                size = [f"none ({gb(r['memory_bytes'])} in memory)", f"{r['memory_per_node']:.2f} in memory"]
            else:
                size = [gb(r["disk_bytes"]), f"{r['bytes_per_node']:.2f}"]
            rows.append([f"{rname}, `{sname}`", f"{r['nodes']:,}", *size,
                         secs(r["build_s"]), f"{r['build_rss_gb']:.1f} GB" if "build_rss_gb" in r else "–",
                         resolved(r) if rg == "planet" and "resolve_s" in r else ""])
    out.append(table(["", "Nodes", "On disk", "Bytes per node", "Bulk load", "Load peak memory",
                      "Resolve all ways"], rows))
    for rg, rname in MEM:
        rs = [mem.get((rg, st)) for st, _ in STORES]
        if not any(rs):
            continue
        r0 = next(r for r in rs if r)
        out.append(f"\n{MEM_TITLES[rg]}: {r0['nodes']:,} nodes, {r0['refs']:,} way node refs. The better of two "
                   "runs, and how far apart they were:\n\n")
        sp = [spreads.get((rg, st), {}) for st, _ in STORES]

        def cell(r, s, key, fmt):
            return f"{fmt(r)} (±{100 * s.get(key, 0):.0f}%)" if r and key in r else "–"

        rows = [
            ("Bulk load", "build_s", lambda r: f"{secs(r['build_s'])}, {r['build_mnps']:.1f} M/s"),
            ("Resolve all ways", "resolve_s", lambda r: f"{secs(r['resolve_s'])}, {r['resolve_mps']:.1f} M/s"),
            ("Verify every node", "verify_s", lambda r: f"{secs(r['verify_s'])}, {r['verify_mnps']:.0f} M/s"),
            ("Lookups in way order, 16 threads", "way_mps", lambda r: f"{r['way_mps']:.1f} M/s"),
            ("Lookups in way order, 1 thread", "way_ns", lambda r: f"{r['way_ns']} ns"),
            ("Lookups in random order, 16 threads", "random_mps", lambda r: f"{r['random_mps']:.1f} M/s"),
            ("Lookups in random order, 1 thread", "random_ns", lambda r: f"{r['random_ns']} ns"),
            ("Commit 2,000 changes, median", "commit_median_ms", lambda r: f"{r['commit_median_ms']:.0f} ms"),
            ("Commit 2,000 changes, 99th percentile", "commit_p99_ms", lambda r: f"{r['commit_p99_ms']:.0f} ms"),
        ]
        out.append(table([""] + [f"`{n}`" for _, n in STORES],
                         [[label] + [cell(r, s, key, f) for r, s in zip(rs, sp)] for label, key, f in rows]))
    path.write_text("".join(out))


def main():
    d, svg, md = Path(sys.argv[1]), Path(sys.argv[2]), Path(sys.argv[3])
    read_regions(d)
    mem, spreads, whole = load(d)
    chart(mem, whole, svg)
    tables(mem, spreads, whole, md)
    print(f"{len(mem)} in-memory and {len(whole)} whole-extract results")


if __name__ == "__main__":
    main()
