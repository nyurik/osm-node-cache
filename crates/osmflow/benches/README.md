# osmflow benchmarks

`parse.rs` reads a whole PBF file with osmflow and with other PBF crates, and compares their speed. The crate
[README](../README.md#performance) shows the results.

## What it measures

Every reader computes the same summary of the file: counts and sums of IDs, coordinates and node refs, and the length of
every tag key, tag value and member role, each taken as a string slice and passed through `black_box`. So every crate
does the work of handing out its strings, and the benchmark copies none. The benchmark refuses to run unless every
reader's summary is the same.

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

Each pass prints the reader's time, then a line naming the file with its size and summary.

The README's numbers come from a host with the files on a local NVMe disk; a laptop's numbers say little about such a
host. All readers on the small files, and the parallel ones on the large files (`osmpbfreader`'s take 2 and 10 minutes
there):

```sh
dd if=planet.osm.pbf of=/dev/null bs=16M count=640 iflag=direct 2>> perf.log  # the disk's read speed
for f in district-of-columbia massachusetts; do
  OSMFLOW_BENCH_ONCE=1 OSMFLOW_BENCH_PBF=$f.osm.pbf cargo bench -p osmflow --bench parse 2>> perf.log
done
for f in north-america planet; do
  OSMFLOW_BENCH_ONCE=1 OSMFLOW_BENCH_PBF=$f.osm.pbf OSMFLOW_BENCH_READERS=osmflow/parallel,osmpbf/par \
    cargo bench -p osmflow --bench parse 2>> perf.log
done
```

## The chart and tables

`report.py` makes the README's chart and tables from the logs:

```sh
python3 crates/osmflow/benches/report.py crates/osmflow/docs/perf.svg tables.md earlier.log perf.log
```

A later log's time replaces an earlier one's for the same file and reader, and times from any log but the last are
marked with `*`, so a slow reader's time can come from a run of its own. The `dd` line gives the disk's speed, drawn as
a line on the chart.
