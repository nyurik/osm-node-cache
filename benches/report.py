#!/usr/bin/env python3
"""Makes the osmflow README's performance chart (SVG) and tables (Markdown) from `parse` benchmark logs.

Usage: report.py <chart.svg> <tables.md> <log>...

Each log is the output of `just bench-pbf` with `OSMFLOW_BENCH_ONCE=1`, one or more files per log, as `README.md` in
this directory runs it. Every reader package prints its own summary of each file; within a log, they must all be the
same. A later log's time replaces an earlier one's for the same file and reader;
times taken from any log but the last are marked as from an earlier run. A log may also hold the `dd` line of a direct
read of the disk, drawn as the disk's speed.
"""

import re
import sys
from pathlib import Path

# The files, in the order shown, by the start of their file name.
FILES = [
    ("district-of-columbia", "District of Columbia"),
    ("massachusetts", "Massachusetts"),
    ("na-", "North America"),
    ("planet", "Planet"),
]
ONE_THREAD = [
    ("osmflow/one thread", "osmflow, `blocks`", 0),
    ("osmpbf/one thread", "osmpbf, `ElementReader::for_each`", 2),
    ("osmpbfreader/one thread", "osmpbfreader, `iter`", 3),
]
PARALLEL = [
    ("osmflow/parallel unordered", "osmflow, `par_unordered`", 0),
    ("osmflow/parallel ordered", "osmflow, `par_ordered`", 1),
    ("osmpbf/par_map_reduce", "osmpbf, `par_map_reduce`", 2),
    ("osmpbfreader/par_iter", "osmpbfreader, `par_iter`", 3),
]
LEGEND = [(0, "osmflow"), (1, "osmflow, in file order"), (2, "osmpbf"), (3, "osmpbfreader")]


def region(file_name):
    for prefix, name in FILES:
        if file_name.startswith(prefix):
            return name
    return None


def parse(logs):
    """Per region: its size, counts, and each reader's time (seconds, and whether it is from an earlier log). The
    benchmark prints each reader's time, then a line naming the file with its size and summary."""
    regions, disk = {}, None
    for n, path in enumerate(logs):
        latest = n == len(logs) - 1
        times, summaries = {}, {}
        for line in Path(path).read_text().splitlines():
            if m := re.match(r"(\d+) bytes .* copied, ([\d.]+) s", line):
                disk = int(m[1]) / float(m[2])
            elif m := re.match(r"(\S+/[\w ]+): ([\d.]+) s$", line):
                times[m[1]] = (float(m[2]), not latest)
            elif m := re.match(r"(\S+): (?:(\d+) bytes, )?Summary \{(.*)\}", line):
                # Separate reader packages read the same file separately: they must agree
                if summaries.setdefault(Path(m[1]).name, m[3]) != m[3]:
                    sys.exit(f"{path}: readers of {Path(m[1]).name} differ:\n{summaries[Path(m[1]).name]}\n{m[3]}")
                name = region(Path(m[1]).name)
                if name:
                    r = regions.setdefault(name, {"times": {}})
                    r["times"].update(times)
                    if m[2]:
                        r["size"] = int(m[2])
                    r.update({k: int(v) for k, v in re.findall(r"(\w+): (-?\d+)", m[3])})
                times = {}
    return regions, disk


# Chart drawing.
STYLE = """<style>
  .surface { fill: #fcfcfb; }
  .heading { fill: #0b0b0b; font-family: system-ui, sans-serif; font-size: 17px; font-weight: 600; }
  .title { fill: #0b0b0b; font-family: system-ui, sans-serif; font-size: 15px; font-weight: 600; }
  .note { fill: #52514e; font-family: system-ui, sans-serif; font-size: 12px; }
  .label { fill: #52514e; font-family: system-ui, sans-serif; font-size: 12px; }
  .value { fill: #0b0b0b; font-family: system-ui, sans-serif; font-size: 11px; }
  .axis { stroke: #c3c2b7; stroke-width: 1; }
  .limit { stroke: #52514e; stroke-width: 1; stroke-dasharray: 4 3; }
  .s0 { fill: #2a78d6; } .s1 { fill: #9ec3ef; } .s2 { fill: #eb6834; } .s3 { fill: #eda100; }
  @media (prefers-color-scheme: dark) {
    .surface { fill: #1a1a19; }
    .heading, .title, .value { fill: #f0efec; }
    .note, .label { fill: #c3c2b7; }
    .axis { stroke: #52514e; }
    .limit { stroke: #c3c2b7; }
    .s0 { fill: #3987e5; } .s1 { fill: #2f5f97; } .s2 { fill: #d95926; } .s3 { fill: #c98500; }
  }
</style>"""
WIDTH = 920
ROW, BAR = 12, 8


def bar(out, cls, x, y, w):
    """A bar with a rounded end."""
    r = min(3.0, w / 2, BAR / 2)
    w = max(w - r, 0)
    out.append(f'<path class="s{cls}" d="M{x:.1f},{y:.1f} h{w:.1f} a{r:.1f},{r:.1f} 0 0 1 {r:.1f},{r:.1f} '
               f'v{BAR - 2 * r:.1f} a{r:.1f},{r:.1f} 0 0 1 -{r:.1f},{r:.1f} h-{w:.1f} z"/>')


def panel(out, x, y, width, title, note, groups, scale, fmt, limit=None):
    """Horizontal bars, grouped: groups are (label, [(series, value, text)]). Returns the panel's bottom."""
    out.append(f'<text class="title" x="{x}" y="{y}">{title}</text>')
    out.append(f'<text class="note" x="{x}" y="{y + 18}">{note}</text>')
    left, top = x + 150, y + 34
    room = width - 150 - 90
    gy = top
    for label, bars in groups:
        height = len(bars) * ROW
        out.append(f'<text class="label" x="{x}" y="{gy + height / 2 + 4:.1f}">{label}</text>')
        for k, (series, value, text) in enumerate(bars):
            by = gy + k * ROW
            w = room * value / scale
            bar(out, series, left, by, w)
            out.append(f'<text class="value" x="{left + w + 7:.1f}" y="{by + 9}">{fmt(value, text)}</text>')
        gy += height + 16
    bottom = gy - 12
    out.append(f'<line class="axis" x1="{left}" y1="{top - 4}" x2="{left}" y2="{bottom}"/>')
    if limit:
        value, text = limit
        lx = left + room * value / scale
        out.append(f'<line class="limit" x1="{lx:.1f}" y1="{top - 4}" x2="{lx:.1f}" y2="{bottom}"/>')
        out.append(f'<text class="note" x="{lx - 4:.1f}" y="{top - 8}" text-anchor="end">{text}</text>')
    return bottom


def secs(seconds):
    """A time with about three significant digits."""
    return f"{seconds:.3f}" if seconds < 1 else f"{seconds:.2f}" if seconds < 10 else f"{seconds:,.1f}"


def size(n):
    return f"{n / 1e9:.1f} GB" if n >= 1e9 else f"{n / 1e6:.0f} MB"


def shown(regions):
    """The regions in display order; one whose run did not finish has no size, and is left out."""
    return [name for _, name in FILES if "size" in regions.get(name, {})]


def mbps(r, reader):
    seconds, earlier = r["times"][reader]
    return r["size"] / seconds / 1e6, seconds, earlier


def chart(regions, disk):
    out = []
    names = shown(regions)
    # In parallel: every file.
    groups = []
    for name in names:
        r = regions[name]
        bars = []
        for reader, _, series in PARALLEL:
            if reader in r["times"]:
                speed, seconds, earlier = mbps(r, reader)
                bars.append((series, speed, f"{secs(seconds)} s{' *' if earlier else ''}"))
        groups.append((name, bars))
    scale = max(max(v for _, bars in groups for _, v, _ in bars), (disk or 0) / 1e6) * 1.02
    y = panel(out, 16, 88, WIDTH - 16, "Reading a whole file in parallel",
              "MB of PBF file per second, with the file's time; higher is better", groups, scale,
              lambda v, t: f"{v:,.0f} · {t}", (disk / 1e6, f"disk, {disk / 1e9:.1f} GB/s") if disk else None)
    # One thread: the files every reader read on one thread.
    one = []
    for name in names:
        r = regions[name]
        if all(reader in r["times"] for reader, _, _ in ONE_THREAD):
            one.append((name, [(s, mbps(r, reader)[0], f"{secs(mbps(r, reader)[1])} s") for reader, _, s in ONE_THREAD]))
    half = (WIDTH - 48) // 2
    top = y + 44
    bottom = top
    if one:
        scale = max(v for _, bars in one for _, v, _ in bars) * 1.02
        bottom = panel(out, 16, top, half, "On one thread", "MB/s, with the file's time; higher is better", one,
                       scale, lambda v, t: f"{v:,.0f} · {t}")
    # How many times faster osmflow is, in parallel.
    faster = []
    for name in names:
        t = regions[name]["times"]
        if "osmflow/parallel unordered" not in t:
            continue
        ours = t["osmflow/parallel unordered"][0]
        bars = [(s, t[other][0] / ours, "*" if t[other][1] else "")
                for other, s in [("osmpbf/par_map_reduce", 2), ("osmpbfreader/par_iter", 3)] if other in t]
        faster.append((name, bars))
    scale = max(v for _, bars in faster for _, v, _ in bars) * 1.02
    bottom = max(bottom, panel(out, 32 + half, top, half, "osmflow's lead in parallel",
                               "times faster than each crate's parallel read", faster, scale,
                               lambda v, t: f"{v:.1f}×{' ' + t if t else ''}"))
    height = bottom + 40
    head = [f'<svg xmlns="http://www.w3.org/2000/svg" width="{WIDTH}" height="{height}" '
            f'viewBox="0 0 {WIDTH} {height}" role="img" '
            f'aria-label="osmflow, osmpbf and osmpbfreader reading OSM PBF files, in parallel and on one thread">',
            STYLE, f'<rect class="surface" width="{WIDTH}" height="{height}" rx="8"/>',
            '<text class="heading" x="16" y="30">Reading OSM PBF files</text>']
    x = 16
    for series, label in LEGEND:
        head.append(f'<rect class="s{series}" x="{x}" y="44" width="12" height="12" rx="3"/>')
        head.append(f'<text class="label" x="{x + 18}" y="54">{label}</text>')
        x += 18 + round(6.6 * len(label)) + 20
    head.append(f'<text class="note" x="{x + 8}" y="54">AWS m8gd.4xlarge, 16 Graviton4 cores, local NVMe disk</text>')
    return "\n".join(head + out + ["</svg>", ""])


def tables(regions, disk):
    names = shown(regions)
    md = ["| File | PBF size | Nodes | Ways | Relations | Way node refs | Tags |", "|---|---:|---:|---:|---:|---:|---:|"]
    for name in names:
        r = regions[name]
        md.append(f"| {name} | {size(r['size'])} | {r['nodes']:,} | {r['ways']:,} | {r['relations']:,} | "
                  f"{r['refs']:,} | {r['tags']:,} |")
    md.append("")
    md.append("| Reader | " + " | ".join(names) + " |")
    md.append("|---|" + "---:|" * len(names))
    for readers in (PARALLEL, ONE_THREAD):
        for reader, label, _ in readers:
            cells = []
            for name in names:
                r = regions[name]
                if reader in r["times"]:
                    speed, seconds, earlier = mbps(r, reader)
                    cells.append(f"{secs(seconds)} s, {speed:,.0f} MB/s{' *' if earlier else ''}")
                else:
                    cells.append("")
            md.append(f"| {label} | " + " | ".join(cells) + " |")
    if disk:
        md += ["", f"The disk reads {disk / 1e9:.1f} GB/s (10 GiB of the planet file with direct I/O)."]
    return "\n".join(md) + "\n"


def main():
    if len(sys.argv) < 4:
        sys.exit(__doc__)
    regions, disk = parse(sys.argv[3:])
    Path(sys.argv[1]).write_text(chart(regions, disk))
    Path(sys.argv[2]).write_text(tables(regions, disk))


if __name__ == "__main__":
    main()
