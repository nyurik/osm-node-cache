# osm-node-cache

[![GitHub repo](https://img.shields.io/badge/github-osmnodecache-8da0cb?logo=github)](https://github.com/nyurik/osm-node-cache)
[![crates.io version](https://img.shields.io/crates/v/osmnodecache)](https://crates.io/crates/osmnodecache)
[![crate usage](https://img.shields.io/crates/d/osmnodecache)](https://crates.io/crates/osmnodecache)
[![docs.rs status](https://img.shields.io/docsrs/osmnodecache)](https://docs.rs/osmnodecache)
[![crates.io license](https://img.shields.io/crates/l/osmnodecache)](https://github.com/nyurik/osm-node-cache/blob/main/LICENSE-APACHE)
[![CI build status](https://github.com/nyurik/osm-node-cache/actions/workflows/ci.yml/badge.svg)](https://github.com/nyurik/osm-node-cache/actions)
[![Codecov](https://img.shields.io/codecov/c/github/nyurik/osm-node-cache)](https://app.codecov.io/gh/nyurik/osm-node-cache)

A store of OpenStreetMap node locations by node ID, for tools that read a PBF file's nodes first and then need their
locations to build ways: an importer, a renderer, a router. A store is loaded once from a planet file or an extract, and
then kept up to date with replication diffs, which move, delete and add nodes.
,
## Choosing a store

Four stores, each behind its own feature, implement the same traits, so code written against them switches stores by
changing a type. They trade lookup speed, size, and the cost and safety of changes differently:

| Store         | Feature           | Kept in                                             | Disk size, North America / planet                      | Lookups in random order | Commit 2,000 changes | After a crash                              |
|---------------|-------------------|-----------------------------------------------------|--------------------------------------------------------|-------------------------|----------------------|--------------------------------------------|
| `PagedStore`  | `paged` (default) | 4 KiB pages of delta-coded blocks, memory-mapped    | 13.7 GB / 55.6 GB: about 5 bytes per node              | 26 M/s                  | 23 ms                | the last commit, also after a power loss   |
| `RedbStore`   | `redb`            | the same blocks, 4 per value in a redb B-tree       | 20.7 GB / 75.7 GB: about 7 to 8 bytes per node         | 9.1 M/s                 | 54 ms                | the last commit, also after a power loss   |
| `DenseStore`  | `dense`           | 8 bytes per ID, at the ID's position, memory-mapped | 64.3 GB / 106.7 GB: up to 8 bytes × the highest node ID | 1,245 M/s              | 14 ms                | the last commit, and some changes after it |
| `MemoryStore` | `memory`          | a hash map in RAM, with no file                     | none; 20 to 39 bytes per node of RAM instead           | 424 M/s                 | none needed          | nothing: it has no file                    |

The disk sizes are of whole extracts. The lookups and commits were measured with each store in memory, on North America's
nodes up to ID 3 billion; the [performance](#performance) section has the details.

* `PagedStore`, the default, is the smallest for large data and the fastest to load: the planet in 115 s, into 55.6 GB.
  Its lookups are fast as long as its file, about 5 bytes per node, fits in memory, which makes it the only store that
  resolves the planet's ways on a 64 GiB host at memory speed: 55 s, once the store is in memory.
* `DenseStore` has by far the fastest lookups, one memory read each, but its file is huge: 8 bytes for every ID up to
  the highest one stored, 107 GB for the planet. Extracts have nodes with IDs all the way up, so their files are as
  long; the parts with no nodes take no disk space on most file systems, but an extract still takes far more than in
  the other stores: 64.3 GB for North America, 167 MB (85 bytes per node) for District of Columbia. It is only fast
  with memory for the whole file.
* `RedbStore` puts the nodes in [redb](https://crates.io/crates/redb), a well-tested database, at the cost of slower
  lookups and loads, and a larger file.
* `MemoryStore` keeps the nodes in a hash map in RAM, and has no file: it takes no disk space, and nothing is left of it
  when the process ends. It takes the most memory, 20 to 39 bytes per node depending on how full its hash map is (the
  planet's 11 billion nodes would take about 290 GB), and is the fastest to change: a change is one hash map update,
  and a commit only records the stamp. It suits a city or a region, which loads from its PBF file in a second or two.

All stores:

* take node IDs up to `MAX_NODE_ID` (about 68.7 billion) and locations in 1e-7 degrees, as PBF files have them, and give
  them back exactly;
* bulk load from nodes in ascending ID order, much faster than setting them one by one;
* keep a stamp, such as the replication sequence number, and up to 1 KiB of named metadata values, committed with the
  nodes.

The three stores kept in a file (all but `MemoryStore`) also:

* lock their files, so one process changes a store, and any number read it while no one changes it;
* open in one of three ways: to read only, which has no methods that change the store; to read and write, creating the
  store if it is missing; and to read, write and repair what can be repaired of a damaged store;
* keep checkpoints during a bulk load, so a load cut short by a crash resumes: it is fed the same nodes from the start,
  checks those it kept, and goes on.

## How the stores work

![How each store looks up a node and keeps a change](docs/how-stores-work.svg)

`PagedStore` and `RedbStore` group node IDs into blocks of 64. A block keeps which of its IDs are present, and each
location as the step from the one before it, bit-packed or as varints, whichever is smaller. A reader per thread keeps
up to 4,096 blocks it decoded (2 MiB), block `n` in slot `n % 4096`, so most of a way's nodes need no decoding at
all.

`PagedStore` packs the blocks into 4 KiB pages of one file, read through a memory map, with a CRC-32 per page. A
directory of the first block of each page is kept in memory, and in `<file>.dir`, to which pages are only ever added. A
bulk load encodes the blocks on all cores, fills pages to 97%, so most later changes fit in place, and writes them
1 MiB at a time; every 1 GiB, it keeps a checkpoint on another thread while it goes on loading. Until a commit, changed pages wait decoded in memory.
A commit encodes them on all cores, moving the blocks of a page that overflows to new pages at the end of the file,
filled as a bulk load fills them. It saves the old contents of the pages it changes to `<file>.undo`, writes the new
ones and the new directory entries, then a header with the stamp and metadata, syncing each in turn; the header has two
copies, written in turn. Opening the store after a crash during a commit rolls it back.

`RedbStore` keeps each group of 4 blocks (256 IDs) as one value in a [redb](https://crates.io/crates/redb) database, a
copy-on-write B-tree. Changes wait in memory until a commit writes them, the stamp and the metadata in one transaction.
A bulk load encodes the groups on all cores, and inserts them in order. `KvStore` and the `KvBackend` trait it uses can
hold the same groups in another key-value database.

`DenseStore` keeps 8 bytes per node ID, at the ID's position in a memory-mapped file, so a lookup is one memory read
and a change one write. The file takes 8 bytes times the highest ID in it, whatever the number of nodes: over 100 GB
for the planet, and as much for an extract, although unwritten parts of it take no disk space on most file systems. A
bulk load writes the values of a million IDs at a time, not through the memory map, and keeps its checkpoints on
another thread.
Changes are written in place, so after a crash the store may have some of the changes since its last commit; applying
the changes since its stamp again makes it right.

`MemoryStore` keeps a hash map in memory from node ID to location, 16 bytes and a control byte per slot of the map, with
at most 7 of 8 slots filled. It has no file: changes take effect when they are made, and a commit only records the
stamp.

## Using a store

```rust,no_run
use osmnodecache::prelude::*;
use osmnodecache::{Location, PagedStore};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // opens the store to read and write, creating it if it is missing
    let mut store = PagedStore::open("nodes.cache")?;

    // a bulk load in ascending ID order, as a sorted PBF file has them; after a crash, the same
    // load run again checks the nodes the store kept, up to `resume_after()`, and goes on
    let mut load = store.bulk_load()?;
    load.push(1, Location::new(515_000_000, -1_000_000))?;
    load.push(2, Location::new(515_000_100, -1_000_050))?;
    load.finish(0)?;

    // reads; a reader per thread keeps up to 4,096 blocks it read decoded
    let mut reader = store.reader();
    assert_eq!(reader.get(2), Some(Location::new(515_000_100, -1_000_050)));

    // a replication diff, kept with its sequence number and metadata at the commit
    store.set(3, Location::new(515_000_200, -1_000_100))?;
    store.remove(1)?;
    store.set_metadata("replication_url", b"https://planet.openstreetmap.org/replication/minute")?;
    store.commit(4_301_000)?;
    Ok(())
}
```

The stores kept in a file open with the `FileStore` trait's functions. A store opened to read only is a
`PagedStore<ReadOnly>`, which has no methods that change it. Opening with `open_and_fix` rebuilds a lost directory file,
and drops pages whose CRC is wrong, reporting the node IDs they held:

```rust,no_run
use osmnodecache::prelude::*;
use osmnodecache::PagedStore;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let store = PagedStore::open_read_only("nodes.cache")?;
    println!("nodes as of replication sequence {}", store.stamp());
    drop(store);
    let (_store, report) = PagedStore::open_and_fix("nodes.cache")?;
    for ids in report.lost {
        println!("lost the nodes from {} to {}", ids.start(), ids.end());
    }
    Ok(())
}
```

A memory store, which has no file, is made with `MemoryStore::new()`. Code written against the traits works with any
store:

```rust
use osmnodecache::prelude::*;
use osmnodecache::Location;

/// The locations of a way's nodes, leaving out those the store does not have.
fn way_geometry(store: &impl NodeRead, refs: &[u64]) -> Vec<Location> {
    let mut reader = store.reader();
    refs.iter().filter_map(|&id| reader.get(id)).collect()
}
```

### Importing the planet fast

What made the most difference for the planet, in the [performance](#performance) runs:

* **Read the PBF file in big reads, with direct I/O** (`O_DIRECT` on Linux). Through the page cache, the OS reads ahead
  only 128 KiB at a time by default, which on a network disk keeps too few reads in flight: one reader got 360 MB/s,
  a third of what the disk gives (1.1 GB/s with direct reads of 8 MiB). Direct reads also keep the 95 GB file from
  pushing the store out of memory.
* **Decode only what the store needs** from the PBF file: node IDs and locations, and way refs. The `osmpbf` crate
  decodes all of each block, and took 93 s for the planet's nodes on 16 cores, against 61 s, at the disk's speed, for
  `examples/pbf_eval`'s decoder of just those fields.
* **Push nodes in batches** of a million or so with `BulkLoad::push_batch`: the paged and redb stores check and encode
  big batches on all cores.
* **Keep one reader per thread** (`NodeRead::reader`) for all of a thread's lookups: it keeps up to 4,096 decoded blocks
  (2 MiB), and making one has a cost.
* **Read the store into memory before resolving**, if it fits but is not there, as after a restart: `NodeRead::preload`
  reads it in big reads on all cores: 52 s for the planet's 55.6 GB `PagedStore`, at the disk's speed. Resolving the
  planet's ways that way took 107 s in all, starting with none of the store in memory; reading it a page at a time on
  first use, with the older reader and PBF decoder, took 29.5 minutes. Preload right after a load too: most of the
  store was still in memory, and reading the rest took 5 s, but without it resolving took 142 s instead of 60 s.
* **Give the memory back after the load.** glibc's allocator kept 12.7 GB that the PBF decoding had freed, with 75 MB
  in use, which left too little memory for the planet's store; `malloc_trim(0)` returned it to the OS.

## Performance

![The four stores compared: size on disk, bulk load, resolving the nodes of all ways, lookups and commits](docs/perf.svg)

Measured with `examples/pbf_eval/measure.sh` on an AWS m8g.4xlarge (16 Graviton4 cores, 64 GiB of memory), with the PBF
files and the stores on a gp3 EBS volume provisioned for 16,000 IOPS and 1,000 MB/s, using District of Columbia from
Geofabrik (2026-10-27), and North America and the planet cut from the planet of 2026-10-19. Whole extracts were read
with direct I/O. Lookups and commits were measured with each store in memory: District of Columbia, and the nodes of
North America up to ID 3 billion (14 GB as a dense store), each run twice, the better run shown.

### The planet

10.9 billion nodes, and 1.23 billion ways with 13.0 billion node refs:

|                                                | `PagedStore`                                 | `RedbStore`          | `DenseStore`          |
|------------------------------------------------|----------------------------------------------|----------------------|-----------------------|
| On disk                                        | 55.6 GB, 5.08 bytes per node                 | 75.7 GB, 6.91        | 106.7 GB, 9.74        |
| Bulk load from the PBF file                    | 115 s                                        | 326 s                | 164 s                 |
| Resolve all ways, with none of the store in memory at first | 107 s: 52 s to read the store in, then 55 s | larger than memory | larger than memory |
| Load, then resolve all ways, in one run        | 176 s: 116 s, then 5 s to read the rest of the store in, then 55 s | | |

* **The paged store's planet load is disk-bound**: it reads 65 GB of PBF blobs and writes 56 GB on one volume that gives
  1 GB/s for both. The dense store writes 107 GB, as the planet's node IDs leave few gaps.
* **Only the paged store fits in the host's memory**, so only it resolves the planet's ways at memory speed: 237 million
  refs a second once it is in memory, 47 ns per lookup on each of 16 threads. Importing the planet, from the PBF file to
  every way's node locations, takes 3 minutes. Resolving with the 107 GB dense store went
  at the disk's speed for random reads, about 1 million lookups a second, and as fast on 64 threads as on 16: the disk's
  16,000 reads a second are the limit, not how many are in flight. An earlier redb run, before these changes, read 82%
  of the planet file in 72 minutes before it was stopped.

### On a gp3 volume at its default speed

The same host, with the PBF files and the stores on a second gp3 volume at its default 3,000 IOPS and 125 MB/s:

|                                                    | `PagedStore`   | `RedbStore`   | `DenseStore`  |
|----------------------------------------------------|---------------:|--------------:|--------------:|
| North America: bulk load                           | 221 s          | 278 s         | 854 s         |
| The planet: bulk load                              | 932 s          | 1,094 s       | 1,321 s       |
| The planet: resolve all ways, with none of the store in memory at first | 663 s | | |

Every run went at the volume's 125 MiB/s (131 MB/s), counting reads and writes together: the paged planet load read 65
GB and wrote 56 GB, and its resolve read the 56 GB store and the rest of the PBF file, 31 GB. On a slow disk, the store that writes
the fewest bytes loads fastest, and redb's load slows the least, as more of its time is spent encoding.

### What made it faster

The first runs of these stores, with the [`osmpbf`](https://crates.io/crates/osmpbf) crate reading the PBF file:

|                                                  | Before     | Now        | What changed                                                         |
|--------------------------------------------------|------------|------------|----------------------------------------------------------------------|
| Planet, `PagedStore` bulk load                   | 314 s      | 115 s      | the PBF file read with direct I/O, and decoded for just the needed fields |
| Planet, resolve all ways with `PagedStore`       | 29.5 min   | 107 s      | the store read into memory first, the PBF file kept out of it, and a bigger reader cache |
| Planet, `RedbStore` bulk load                    | 489 s      | 326 s      | groups encoded on all cores                                          |
| North America nodes to ID 3 B, `PagedStore` bulk load | 8.9 s | 4.9 s      | the decoder, and checks and block boundaries on all cores            |
| The same, resolve all ways with `PagedStore`     | 28.8 s     | 10.4 s     | the decoder, and a reader cache of 4,096 blocks instead of 4         |
| The same, resolve all ways with `RedbStore`      | 52.6 s     | 16.3 s     | the same                                                             |
| The same, `DenseStore` bulk load                 | 31.4 s     | 23.5 s     | the decoder, values written a million IDs at a time, and checkpoints kept on another thread |

Some findings along the way:

* **Reading through the page cache was the first limit.** The data volume's 128 KiB of read-ahead kept one reader to
  360 MB/s; direct reads of 8 MiB got 1.05 GB/s. Asking the OS to read ahead (`POSIX_FADV_WILLNEED`) did not help.
* **Decoding the PBF file**: the planet's nodes took 93 s with `osmpbf`, and 61 s, as fast as the disk read them, with a
  decoder of just node IDs, locations and way refs. Decompressing with zlib-rs was 12% faster than with zlib-ng, from
  memory.
* **The reader cache**, measured with callgrind on District of Columbia: resolving its ways took 2.36 billion
  instructions with the paged store's old cache of 4 blocks, and 0.86 billion with 4,096; 4.29 and 1.18 billion with
  redb. The cache is direct-mapped and allocated zeroed, so a new reader costs next to nothing.
* **Writing a memory-mapped file that the page cache holds in large folios** (XFS does, for big writes) dirties whole
  folios, so a dense store commit after a bulk load wrote far more than it changed: 177 ms instead of 14 ms. The dense
  store now writes changes to the file, and maps it only to read.
* **Writing past the end of a file on XFS** has it keep blocks after it, which take disk space once the file grows past
  them: the dense store grows its file first, as it did through the memory map.
* **Not worth it**: groups of 8 or 16 blocks per redb value made the file 4 to 6% smaller and loads 70% slower;
  4 or 16 times as many groups per redb transaction loaded no faster;
  compacting the redb file after a load saved nothing and took 135 s for North America; entropy coding (rANS) would make
  the paged store about 8% smaller and its lookups slower.

### Async

An async API would not make these stores faster. Loading is limited by the disk's throughput, and the checkpoints are
already made durable on another thread. With the store in memory, a lookup takes 20 to 50 ns, much less than an async
task switch. With a store larger than memory, the disk's random reads are the limit: 64 threads waiting on them
resolved no faster than 16. It would let an async program look up nodes without blocking its runtime's threads on the
disk, as a reader with positioned reads could, trying the page cache first (`preadv2` with `RWF_NOWAIT`), instead of a
memory map; that is worth adding when such a program needs it.

Whole extracts:

|  | Nodes | On disk | Bytes per node | Bulk load | Load peak memory | Resolve all ways |
|---|---:|---:|---:|---:|---:|---:|
| District of Columbia, `PagedStore` | 1,958,977 | 10 MB | 4.90 | 59 ms | 0.1 GB |  |
| District of Columbia, `RedbStore` | 1,958,977 | 19 MB | 9.51 | 154 ms | 0.1 GB |  |
| District of Columbia, `DenseStore` | 1,958,977 | 167 MB | 85.35 | 999 ms | 0.2 GB |  |
| District of Columbia, `MemoryStore` | 1,958,977 | none (71 MB in memory) | 36.40 in memory | 96 ms | 0.1 GB |  |
| North America, `PagedStore` | 2,631,813,110 | 13.7 GB | 5.20 | 26.8 s | 14.0 GB |  |
| North America, `RedbStore` | 2,631,813,110 | 20.7 GB | 7.88 | 105 s | 18.5 GB |  |
| North America, `DenseStore` | 2,631,813,110 | 64.3 GB | 24.43 | 160 s | – |  |
| Planet, `PagedStore` | 10,949,467,954 | 55.6 GB | 5.08 | 115 s | 62.9 GB | 107 s, 122.1 M refs/s (52.1 s of it reading the store into memory) |
| Planet, `RedbStore` | 10,949,467,954 | 75.7 GB | 6.91 | 326 s | 34.1 GB |  |
| Planet, `DenseStore` | 10,949,467,954 | 106.7 GB | 9.74 | 164 s | 15.3 GB |  |

District of Columbia: 1,958,977 nodes, 2,507,044 way node refs. The better of two runs, and how far apart they were:

|  | `PagedStore` | `RedbStore` | `DenseStore` | `MemoryStore` |
|---|---:|---:|---:|---:|
| Bulk load | 59 ms, 33.2 M/s (±5%) | 154 ms, 12.7 M/s (±82%) | 999 ms, 2.0 M/s (±9%) | 96 ms, 20.4 M/s (±3%) |
| Resolve all ways | 16 ms, 156.7 M/s (±12%) | 22 ms, 114.0 M/s (±18%) | 17 ms, 147.5 M/s (±0%) | 18 ms, 139.3 M/s (±0%) |
| Verify every node | 46 ms, 43 M/s (±4%) | 56 ms, 35 M/s (±4%) | 42 ms, 47 M/s (±21%) | 132 ms, 15 M/s (±1%) |
| Lookups in way order, 16 threads | 535.5 M/s (±0%) | 234.3 M/s (±6%) | 2464.2 M/s (±11%) | 811.1 M/s (±2%) |
| Lookups in way order, 1 thread | 19 ns (±0%) | 33 ns (±6%) | 11 ns (±0%) | 15 ns (±7%) |
| Lookups in random order, 16 threads | 56.3 M/s (±0%) | 20.4 M/s (±0%) | 1285.3 M/s (±4%) | 808.6 M/s (±3%) |
| Lookups in random order, 1 thread | 242 ns (±0%) | 440 ns (±2%) | 8 ns (±12%) | 16 ns (±0%) |
| Commit 2,000 changes, median | 22 ms (±0%) | 15 ms (±0%) | 20 ms (±1%) | 0 ms (±0%) |
| Commit 2,000 changes, 99th percentile | 33 ms (±1%) | 32 ms (±0%) | 34 ms (±5%) | 0 ms (±0%) |

North America, the nodes up to ID 3,000,000,000: 618,849,582 nodes, 706,551,105 way node refs. The better of two runs, and how far apart they were:

|  | `PagedStore` | `RedbStore` | `DenseStore` | `MemoryStore` |
|---|---:|---:|---:|---:|
| Bulk load | 4.9 s, 127.6 M/s (±1%) | 26.8 s, 23.1 M/s (±8%) | 23.5 s, 26.3 M/s (±20%) | 55.8 s, 11.1 M/s (±0%) |
| Resolve all ways | 10.4 s, 67.6 M/s (±0%) | 16.3 s, 43.3 M/s (±3%) | 8.3 s, 84.9 M/s (±530%) | 10.5 s, 67.2 M/s (±0%) |
| Verify every node | 8.4 s, 74 M/s (±1%) | 12.7 s, 49 M/s (±1%) | 4.2 s, 148 M/s (±0%) | 74.6 s, 8 M/s (±1%) |
| Lookups in way order, 16 threads | 322.1 M/s (±1%) | 97.2 M/s (±3%) | 1936.7 M/s (±5%) | 426.8 M/s (±2%) |
| Lookups in way order, 1 thread | 29 ns (±0%) | 58 ns (±0%) | 11 ns (±0%) | 39 ns (±5%) |
| Lookups in random order, 16 threads | 26.4 M/s (±0%) | 9.1 M/s (±1%) | 1244.9 M/s (±0%) | 423.8 M/s (±5%) |
| Lookups in random order, 1 thread | 649 ns (±4%) | 1503 ns (±1%) | 13 ns (±0%) | 42 ns (±2%) |
| Commit 2,000 changes, median | 23 ms (±0%) | 54 ms (±0%) | 14 ms (±1%) | 0 ms (±0%) |
| Commit 2,000 changes, 99th percentile | 38 ms (±4%) | 75 ms (±2%) | 32 ms (±2%) | 0 ms (±0%) |

"Load peak memory" counts the pages of a memory-mapped file that were in memory; the OS drops them when it needs room.


## Evaluating the stores

`examples/pbf_eval` builds, checks and times a store from a PBF file; a path ending in `.redb` or `.dense` picks that
store, and any other the paged one. A path ending in `.mem` picks the memory store, which has no file, so each command
loads it from the PBF file again. It reads PBF files with its own decoder of just node IDs, locations and way refs, on
all cores; `decode` times reading, decompressing and decoding a file with no store, and compares with `osmpbf`.
`examples/pbf_eval/measure.sh` runs it on a small extract, a large one and the planet, and
`examples/pbf_eval/report.py` makes the chart and tables above from the logs. `cargo bench -p bench` times lookups and
commits of every store on synthetic nodes.

```sh
# build a store, then time resolving the nodes of all ways with it
cargo run --release --all-features --example pbf_eval -- build europe-latest.osm.pbf europe.cache
# check that the store has exactly the file's nodes
cargo run --release --all-features --example pbf_eval -- verify europe-latest.osm.pbf europe.cache
# time lookups of the ways' nodes, then 300,000 changes committed 2,000 at a time
cargo run --release --all-features --example pbf_eval -- bench europe-latest.osm.pbf europe.cache 10 300000 2000
# the same with nodes up to ID 3 billion only, so the store fits in memory
cargo run --release --all-features --example pbf_eval -- --max-id 3000000000 build europe-latest.osm.pbf europe.redb
# the planet: read it with direct I/O, and the store into memory before resolving
cargo run --release --all-features --example pbf_eval -- --direct --preload build planet.osm.pbf planet.cache
# the parts of reading a PBF file, with no store
cargo run --release --all-features --example pbf_eval -- --direct decode planet.osm.pbf
```

## Development

* This project is easier to develop with [just](https://github.com/casey/just#readme), a modern alternative to `make`.
  Install it with `cargo install just`.
* To get a list of available commands, run `just`.
* To run tests, use `just test`.

## License

Licensed under either of

* Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or <https://www.apache.org/licenses/LICENSE-2.0>)
* MIT license ([LICENSE-MIT](LICENSE-MIT) or <https://opensource.org/licenses/MIT>)
  at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in the work by you, as defined in the
Apache-2.0 license, shall be dual-licensed as above, without any
additional terms or conditions.
