# osmflow

[![GitHub repo](https://img.shields.io/badge/github-nyurik/osmflow-8da0cb?logo=github)](https://github.com/nyurik/osmflow)
[![crates.io version](https://img.shields.io/crates/v/osmflow)](https://crates.io/crates/osmflow)
[![crate usage](https://img.shields.io/crates/d/osmflow)](https://crates.io/crates/osmflow)
[![docs.rs status](https://img.shields.io/docsrs/osmflow)](https://docs.rs/osmflow)
[![crates.io license](https://img.shields.io/crates/l/osmflow)](https://github.com/nyurik/osmflow/blob/main/LICENSE-APACHE)
[![CI build status](https://github.com/nyurik/osmflow/actions/workflows/ci.yml/badge.svg)](https://github.com/nyurik/osmflow/actions)
[![Codecov](https://img.shields.io/codecov/c/github/nyurik/osmflow?component=osmflow)](https://app.codecov.io/gh/nyurik/osmflow/components)

Fast, parallel reading of [OpenStreetMap](https://www.openstreetmap.org) PBF files, from a city extract to the planet.

```rust
use std::path::Path;
use osmflow::{MemberType, PbfError, PbfReader, Select};

fn read(path: &Path) -> Result<(), PbfError> {
    let file = PbfReader::open(path)?;
    println!("written by {:?}", file.header().writing_program);

    // Every block on a pool of threads: the fastest way. Blocks come in no particular order within each run of one
    // object type, and a run starts only once every block before it is done, so in a sorted file every node is handled
    // before any way, and every way before any relation. The pool is shared by every reader and has two threads fewer
    // than the cores; `PbfReader::with_pool` gives a reader the caller's pool instead.
    file.par_unordered(Select::all(), |block| {
        // Nodes stored in plain and dense blocks come out the same way.
        for node in block.nodes() {
            let (id, lat, lon) = (node.id(), node.decimicro_lat(), node.decimicro_lon());
            for (key, value) in node.tags() { /* handle node tags */ }
            if let Some(info) = node.info() {
                let (version, user) = (info.version, info.user);
            }
        }
        // A tag key looked up once per block, in its string table: then each way's tag is found by comparing integers.
        let highway = block.tag_key("highway");
        for way in block.ways() {
            let (id, node_ids) = (way.id(), way.refs());
            if let Some(highway) = highway
                && let Some(kind) = way.tag(highway)
            { /* a road of this kind */ }
            for (key, value) in way.tags() { /* handle way tags */ }
        }
        for relation in block.relations() {
            for member in relation.members() {
                let is_way = member.kind == MemberType::Way;
                let (id, role) = (member.id, member.role);
            }
            for (key, value) in relation.tags() { /* handle relation tags */ }
        }
        Ok(())
    })?;

    // Blocks in file order, decoded ahead on a pool of threads and consumed on this one.
    for block in file.par_ordered(Select::node_locations())? {
        for node in block?.nodes() {
            let (_id, _lat, _lon) = (node.id(), node.decimicro_lat(), node.decimicro_lon());
        }
    }
    Ok(())
}
```

## Ways as geometries

```rust
use std::path::Path;

use osmflow::nodes::MemoryStore;
use osmflow::{PbfError, PbfReader, Select};

fn roads(path: &Path) -> Result<(), PbfError> {
    let file = PbfReader::open(path)?;
    // Node locations by ID: in memory for an extract, in a `PagedStore` file for the planet.
    let mut store = MemoryStore::new();
    file.load_nodes(&mut store)?;

    // Only ways, and only from blocks that have the string `highway`: the others are not decoded.
    let file = file.ways_where(|strings| strings.has("highway"));
    let ways = Select::all().without_nodes().without_relations();
    file.par_resolve(ways, &store, |block, locator| {
        let Some(highway) = block.tag_key("highway") else {
            return Ok(());
        };
        for way in block.ways() {
            if way.tag(highway).is_some()
                && let Ok(line) = locator.line_string(&way)
            { /* a road, as a geo_types::LineString */ }
        }
        Ok(())
    })
}
```

A way's nodes are looked up only when its geometry is asked for, and its node IDs decoded only then. Ways that leave an
extract give `GeometryError::MissingNode`; files that store locations on their ways need no store.
`Locator::area_or_line` makes a closed way a polygon when `AreaRules` say its tags make it an area (`building`,
`landuse`, `natural` but not `coastline`, `area=yes`, and so on), and a line otherwise; the rules have a built-in list,
which `add` and `remove` change.

## Skipping blocks

[`PbfReader::ways_where`], [`nodes_where`](PbfReader::nodes_where) and [`relations_where`](PbfReader::relations_where)
test each block's strings (every key, value, role and user name its objects use) before decoding it, and skip the
objects of that type in blocks that cannot hold a wanted one. Object types left out of [`Select`] are skipped the same
way. A skipped block that provably holds nothing else is not inflated past its string table: osmium writes one group per
block, and in a file sorted by type, as the planet is, a block holds one type when the next one starts with the same
type. Large blobs of such blocks are not even read past their start while most blocks are skipped, so a pass over only
the planet's ways or relations reads a fraction of the file. [`PbfReader::read_mode`] overrides how blobs are read, for
storage where many small reads or much data cost more than usual.

## Changes

```rust
use std::path::Path;

use osmflow::osc::{Action, Object, OscError, OscReader, Replication};

fn changes(diff: &Path, replication: &Path) -> Result<(), OscError> {
    // One osmChange file, `.osc` or `.osc.gz`, change by change in file order.
    for change in OscReader::open(diff)? {
        let change = change?;
        if change.action == Action::Delete && let Object::Way(way) = &change.object {
            let (id, version) = (way.id, way.meta.version);
        }
    }

    // A replication stream kept in a directory as the servers lay it out (`000/001/234.osc.gz`), from a sequence to
    // the latest state, the next diffs decoded ahead on other threads. `Replication::http` downloads them into the
    // directory first, with the `http` feature.
    let replication = Replication::local(replication);
    let latest = replication.state()?.sequence;
    for diff in replication.diffs(latest.saturating_sub(2)..)? {
        let diff = diff?;
        println!("{} changes up to {}", diff.changes.len(), diff.state.timestamp);
    }
    Ok(())
}
```

The parser reads attributes as bytes and coordinates as fixed-point integers, so a location from a diff equals the same
location from a PBF file, and it checks the structure strictly: an error names the file, the byte offset and the
element.

## Node location stores

`osmflow::nodes` has stores of node locations by node ID, for building ways from their nodes' IDs: `PagedStore`, about
5 bytes per node in a memory-mapped file (55.6 GB for the planet); `DenseStore`, 8 bytes per node ID, for the fastest
lookups; and `MemoryStore`, a hash map in memory. They bulk load and take replication changes, the two kept in a file
keep their last commit through a crash, and none of them needs the PBF reader.
[`docs/nodes.md`](https://github.com/nyurik/osmflow/blob/main/crates/osmflow/docs/nodes.md) has the details.

## Features

- `geometry` (default): ways as [`geo-types`](https://crates.io/crates/geo-types) geometries. It enables `pbf` and
  `nodes`.
- `pbf`: the PBF reader.
- `nodes`: the node location stores, `osmflow::nodes`, which work without the reader. With `pbf`, it also loads a file's
  nodes into a store and looks up ways' nodes in it.
- `osc` (default): osmChange files and replication diffs, `osmflow::osc`.
- `http`: downloading replication diffs and state files.

## How it reads

[`docs/design.md`](https://github.com/nyurik/osmflow/blob/main/crates/osmflow/docs/design.md) explains how osmflow is
built, and why; in short:

- **Protobuf through [`buffa`](https://crates.io/crates/buffa) views.** The blocks are decoded with a read projection
  of the format's schema (`proto/osmflow_raw.proto`) that declares only what osmflow reads, so the rest is skipped
  unread. Dense nodes are decoded only when nodes are selected, and only their locations when no tags or metadata
  are. Plain nodes, ways and relations are read by osmflow's own field reader, straight into columns. Packed arrays
  may be split into several fields or written unpacked, and message fields may arrive in several occurrences, which
  are merged, as the format allows.
- **Columns:** a block's objects are stored as columns of IDs, locations, references and string indexes. Its string
  table is checked as UTF-8 once and copied into one string, so a block keeps no decompressed buffer.
- **Selection:** [`Select`] skips the object types and fields a caller does not need; whether tags are decoded is a
  type, so code reading tags from a read without them does not compile.
- **Lazy refs:** a way's node IDs stay packed as in the file, checked, until they are iterated.
- **Parallel:** one thread reads blobs; a pool decompresses and decodes them, and hands them out unordered (a
  callback on the pool, in any order within each run of one object type, with a barrier wherever the type changes)
  or ordered (an iterator on the caller's thread, in file order). Either way, the bytes in flight are bounded (16 MiB
  per pool thread, at least 256 MiB), so memory stays the same whatever the file's size.
- **Strict:** blob and header size limits, the declared uncompressed size, string indexes, array lengths and UTF-8 are
  all checked; an error names the blob and its byte offset.

The generated code in `src/generated` is checked in, so building needs no `protoc`. `just update-generated` regenerates
it with `examples/codegen.rs` (it needs `protoc`), and CI's `just codegen-check` fails if it is stale.

## Performance

![osmflow, osmpbf and osmpbfreader reading OSM PBF files, in parallel and on one thread](https://raw.githubusercontent.com/nyurik/osmflow/main/crates/osmflow/docs/perf.svg)

Measured on an AWS m8gd.4xlarge (16 Graviton4 cores, 64 GiB of memory) with the PBF files on its local NVMe disk,
using District of Columbia (2026-10-27) and Massachusetts (2026-11-06) from Geofabrik, and North America and the planet
cut from the planet of 2026-10-19. Every reader reads every object, tag and member of the whole file, and all of them
read the same data; every read comes from the disk, not the page cache. The inflate library differs: osmflow uses its
default zlib-rs, `osmpbf` 0.3.8 its fastest option, zlib-ng, rather than its default `miniz_oxide`, and `osmpbfreader`
0.19.1 the default of `flate2`, `miniz_oxide`, as it has no option of its own. osmflow decodes on its default pool
of 14 threads; `osmpbf` and `osmpbfreader` use rayon's global pool of 16. osmflow and `osmpbf` also decode each
object's metadata, which no reader's summary uses; `osmpbfreader` has none.
[`benches/README.md`](https://github.com/nyurik/osmflow/blob/main/benches/README.md) has what the benchmark measures,
and how to run it.

| File | PBF size | Nodes | Ways | Relations | Way node refs | Tags |
|---|---:|---:|---:|---:|---:|---:|
| District of Columbia | 21 MB | 1,958,977 | 285,566 | 5,284 | 2,507,044 | 2,165,560 |
| Massachusetts | 312 MB | 37,868,785 | 4,163,209 | 38,056 | 44,049,684 | 14,924,497 |
| North America | 22.5 GB | 2,631,813,110 | 239,690,445 | 2,596,205 | 3,032,615,039 | 925,100,962 |
| Planet | 95.5 GB | 10,949,467,954 | 1,225,912,321 | 14,957,553 | 13,047,631,063 | 3,998,641,940 |

| Reader | District of Columbia | Massachusetts | North America | Planet |
|---|---:|---:|---:|---:|
| osmflow, `par_unordered` | 0.029 s, 726 MB/s | 0.270 s, 1,154 MB/s | 17.9 s, 1,257 MB/s | 79.5 s, 1,201 MB/s |
| osmflow, `par_ordered` | 0.044 s, 478 MB/s | 0.475 s, 656 MB/s | 31.5 s, 715 MB/s | 140.6 s, 679 MB/s |
| osmpbf, `par_map_reduce` | 0.049 s, 429 MB/s | 0.401 s, 777 MB/s | 25.6 s, 878 MB/s | 116.2 s, 822 MB/s |
| osmpbfreader, `par_iter` | 0.116 s, 181 MB/s | 1.74 s, 179 MB/s | 109.3 s, 206 MB/s | 554.6 s, 172 MB/s |
| osmflow, `blocks` | 0.252 s, 84 MB/s | 3.63 s, 86 MB/s |  |  |
| osmpbf, `ElementReader::for_each` | 0.421 s, 50 MB/s | 5.35 s, 58 MB/s |  |  |
| osmpbfreader, `iter` | 0.608 s, 35 MB/s | 7.71 s, 40 MB/s |  |  |

The disk reads 2.5 GB/s (10 GiB of the planet file with direct I/O). Every time is from one pass of each reader, all
on the same host the same day; a second pass of every reader but `osmpbfreader`'s on the large files was within 5%.

* **osmflow reads 1.4 to 1.7 times as fast as `osmpbf` in parallel, and 1.5 to 1.7 times as fast on one thread;** in
  parallel it is 4 to 7 times as fast as `osmpbfreader`.
* **The reads are bound by the CPU, not the disk:** osmflow reads the planet at under half the disk's speed.
* **`par_unordered` keeps object types apart:** with its barriers between runs of one type, every node of a sorted
  file is handled before any way, and every way before any relation. Blocks are still read and decoded ahead while a
  run's last blocks finish, so a barrier costs only that short tail.
* **`par_ordered` hands out blocks in file order** on the caller's thread, which is the limit when the consumer does
  much per block, as this benchmark's does. Way refs stay packed until iterated, so this benchmark decodes every ref
  of the file on that one thread, where `par_unordered` decodes them on the pool.

With blocks skipped: one `par_unordered` pass over the planet on the same host, from its NVMe disk, counting every
object and tag it is given:

| Pass | Time |
|---|---:|
| Everything | 87.6 s |
| Ways only (`Select::all().without_nodes().without_relations()`) | 29.1 s |
| Ways in blocks with `highway` (`ways_where(\|s\| s.has("highway"))`, nodes and relations left out) | 28.9 s |
| Relations only | 12.1 s |
| Ways only, the disk limited to 500 MB/s | 71.4 s |
| Relations only, the disk limited to 500 MB/s | 19.2 s |
| Ways only, from an EBS gp3 volume (125 MB/s, 12 minutes to read the file) | 272 s |
| Relations only, from the EBS gp3 volume | 78 s |

Blobs of nodes before the ways, and of ways before the relations, are read only far enough to find their type.
