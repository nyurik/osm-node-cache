# PBF reading benchmarks

These benchmarks read a whole PBF file with osmflow and with other PBF crates, and compare their speed. The osmflow
[README](../crates/osmflow/README.md#performance) shows the results.

Each crate is timed by a package of its own (`osmflow`, `osmpbf` and `osmpbfreader` here), built on its own, so each is
measured with the dependency features it asks for. Cargo merges the features of all packages in one build, so with all
readers in one program, osmpbf's choice of zlib-ng would also be osmflow's. `common` holds what they share: the summary,
dropping the file from the page cache, and the timing loop.

`crates/osmflow/benches/nodes.rs` times lookups and commits of every node location store on synthetic nodes:
`just bench`.

## What it measures

Every reader computes the same summary of the file: counts and sums of IDs, coordinates and node refs, and the length of
every tag key, tag value and member role, each taken as a string slice and passed through `black_box`. So every crate
does the work of handing out its strings, and the benchmark copies none. Each reader adds what it reads to one summary
in place, except `osmpbf/par_map_reduce`, whose API maps every element on its own and so builds a summary per element.
The summary has no object metadata: osmflow (`Select::all()`) and `osmpbf` decode it all the same, and `osmpbfreader`
does not have it. The benchmark refuses to run unless every
reader's summary is the same, and `report.py` checks the summaries of separate runs against each other.

| Reader | API |
|---|---|
| `osmflow/one thread` | `PbfReader::blocks` |
| `osmflow/parallel ordered` | `PbfReader::par_ordered`, on osmflow's default pool (two threads fewer than the cores) |
| `osmflow/parallel unordered` | `PbfReader::par_unordered`, on the same pool |
| `osmpbf/one thread` | `osmpbf::ElementReader::for_each` |
| `osmpbf/par_map_reduce` | `osmpbf::ElementReader::par_map_reduce`, on rayon's global pool |
| `osmpbfreader/one thread` | `osmpbfreader::OsmPbfReader::iter` |
| `osmpbfreader/par_iter` | `osmpbfreader::OsmPbfReader::par_iter`, on rayon's global pool |

**Every read comes from the disk.** Before each reader starts, the file is dropped from the OS page cache with
`posix_fadvise(POSIX_FADV_DONTNEED)`, which needs no root, and the benchmark fails unless `mincore` then finds at most
1% of the file cached. Criterion's samples drop it the same way, outside the timed part. On systems other than Linux
the cache is left as it is, with a warning.

## Running it

| Variable | Effect |
|---|---|
| `OSMFLOW_BENCH_PBF` | The file to read; the Monaco fixture by default |
| `OSMFLOW_BENCH_ONCE=1` | Time one pass of each reader, instead of criterion's ten samples, which take too long on large files |
| `OSMFLOW_BENCH_READERS` | Run only the readers whose names contain one of these comma-separated parts, e.g. `osmflow/parallel,osmpbf/par` |

Each pass prints the reader's time, then a line naming the file with its size and summary. `just bench-pbf` runs
`cargo bench -p osm-bench-<name>` for each package it is given (all three by default), one build at a time.

The README's numbers come from a host with the files on a local NVMe disk; a laptop's numbers say little about such a
host. All readers on the small files, and the parallel ones on the large files (`osmpbfreader`'s take 2 and 10 minutes
there):

```sh
dd if=planet.osm.pbf of=/dev/null bs=16M count=640 iflag=direct 2>> perf.log  # the disk's read speed
for f in district-of-columbia massachusetts; do
  OSMFLOW_BENCH_ONCE=1 OSMFLOW_BENCH_PBF=$f.osm.pbf just bench-pbf 2>> perf.log
done
for f in north-america planet; do
  OSMFLOW_BENCH_ONCE=1 OSMFLOW_BENCH_PBF=$f.osm.pbf OSMFLOW_BENCH_READERS=osmflow/parallel,osmpbf/par \
    just bench-pbf osmflow osmpbf 2>> perf.log
done
```

## The chart and tables

`report.py` makes the README's chart and tables from the logs:

```sh
python3 benches/report.py crates/osmflow/docs/perf.svg tables.md earlier.log perf.log
```

A later log's time replaces an earlier one's for the same file and reader, and times from any log but the last are
marked with `*`, so a slow reader's time can come from a run of its own. The `dd` line gives the disk's speed, drawn as
a line on the chart.
