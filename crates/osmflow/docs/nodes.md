Stores of OpenStreetMap node locations by node ID.

They are for tools that read a PBF file's nodes first and then need their locations to build ways: an importer, a
renderer, a router. A store is loaded once from a planet file or an extract, and then kept up to date with replication
diffs, which move, delete and add nodes.

The stores are osmflow's `nodes` feature, on by default, and need none of the PBF reader: with
`default-features = false, features = ["nodes"]`, osmflow builds only them.

## Choosing a store

Three stores implement the same traits, so code written against them switches stores by changing a type. They trade
lookup speed, size, and the cost and safety of changes differently:

| Store         | Kept in                                             | Disk size, North America / planet                                                                                                 | Lookups in random order, 16 threads | Commit a planet minute diff, median | After a crash                              |
|---------------|-----------------------------------------------------|-----------------------------------------------------------------------------------------------------------------------------------|-------------------------------------|-------------------------------------|--------------------------------------------|
| `PagedStore`  | 4 KiB pages of delta-coded blocks, memory-mapped    | 13.7 GB / 55.6 GB: about 5 bytes per node                                                                                         | 27 M/s                              | 2.5 ms                              | the last commit, also after a power loss   |
| `DenseStore`  | 8 bytes per ID, at the ID's position, memory-mapped | 64.3 GB / 106.7 GB: a sparse file of 8 bytes × the highest node ID; its holes, the ID ranges with no nodes, may use no disk space, except on Windows, where the file takes its whole size | 1,152 M/s                           | 8.1 ms                              | the last commit, and some changes after it |
| `MemoryStore` | a hash map in RAM, with no file                     | none; about 30 bytes per node of RAM instead                                                                                      | 386 M/s                             | none needed                         | nothing: it has no file                    |

The disk sizes are of whole extracts. The lookups were measured on a local NVMe disk, with each store in memory, on
North America's nodes up to ID 3 billion, and the commits on the planet's minute diffs; the [performance](#performance)
section has the details.

* `PagedStore` is the smallest for large data and the fastest to load: the planet in 80 s, into 55.6 GB. Its lookups
  are fast as long as its file, about 5 bytes per node, fits in memory, which makes it the only store that resolves the
  planet's ways on a 64 GiB host at memory speed: 66 s, once the store is in memory.
* `DenseStore` has by far the fastest lookups, one memory read each, but its file is huge: 8 bytes for every ID up to
  the highest one stored, 107 GB for the planet. Extracts have nodes with IDs all the way up, so their files are as
  long; the parts with no nodes take no disk space on most file systems, but an extract still takes far more than in
  the other stores: 64.3 GB for North America, 167 MB (85 bytes per node) for District of Columbia. It is only fast
  with memory for the whole file.
* `MemoryStore` keeps the nodes in a hash map in RAM, and has no file: it takes no disk space, and nothing is left of it
  when the process ends. It takes the most memory, about 30 bytes per node once loaded and 44 at the peak of a load,
  while its hash map grows (the planet's 11 billion nodes would take about 330 GB), and is the fastest to change: a
  change is one hash map update, and a commit only records the stamp. It suits a city or a region, which loads from its
  PBF file in a second or two.

All stores:

* take node IDs up to `MAX_NODE_ID` (about 68.7 billion) and locations in 1e-7 degrees, as PBF files have them, and give
  them back exactly;
* bulk load from nodes in ascending ID order, much faster than setting them one by one;
* keep a stamp, such as the replication sequence number, and up to 1 KiB of named metadata values, committed with the
  nodes.

The two stores kept in a file (all but `MemoryStore`) also:

* lock their files, so one process changes a store, and any number read it while no one changes it;
* open in one of three ways: to read only, which has no methods that change the store; to read and write, creating the
  store if it is missing; and to read, write and repair what can be repaired of a damaged store;
* keep checkpoints during a bulk load, so a load cut short by a crash resumes: it is fed the same nodes from the start,
  checks those it kept, and goes on.

## How the stores work

![How each store looks up a node and keeps a change](https://raw.githubusercontent.com/nyurik/osmflow/main/crates/osmflow/docs/nodes-how-stores-work.svg)

`PagedStore` groups node IDs into blocks of 64. A block keeps which of its IDs are present, and each location as the
step from the one before it, bit-packed or as varints, whichever is smaller. A reader per thread keeps up to 4,096
blocks it decoded (2 MiB), block `n` in slot `n % 4096`, so most of a way's nodes need no decoding at all.

The blocks are packed into 4 KiB pages of one file, read through a memory map, with a CRC-32 per page. A directory of
the first block of each page is kept in memory, and in `<file>.dir`, to which pages are only ever added. A bulk load
encodes the blocks on all cores, fills pages to 97%, so most later changes fit in place, and writes them 1 MiB at a
time; every 1 GiB, it keeps a checkpoint on another thread while it goes on loading. Until a commit, changed pages wait
decoded in memory. A commit encodes them on all cores, moving the blocks of a page that overflows to new pages at the
end of the file, filled as a bulk load fills them. It saves the old contents of the pages it changes to `<file>.undo`,
writes the new ones and the new directory entries, then a header with the stamp and metadata, syncing each in turn; the
header has two copies, written in turn. Opening the store after a crash during a commit rolls it back.

`DenseStore` keeps 8 bytes per node ID, at the ID's position in a memory-mapped file, so a lookup is one memory read
and a change one write. The file takes 8 bytes times the highest ID in it, whatever the number of nodes: over 100 GB
for the planet, and as much for an extract, although unwritten parts of it take no disk space on most file systems. A
bulk load writes the values of a million IDs at a time, not through the memory map, and keeps its checkpoints on
another thread. Changes are written in place, so after a crash the store may have some of the changes since its last
commit; applying the changes since its stamp again makes it right.

`MemoryStore` keeps a hash map in memory from node ID to location, 16 bytes and a control byte per slot of the map, with
at most 7 of 8 slots filled. It has no file: changes take effect when they are made, and a commit only records the
stamp.

## Using a store

```rust
use osmflow::nodes::prelude::*;
use osmflow::Location;
use osmflow::nodes::PagedStore;

fn load_and_update() -> Result<(), Box<dyn std::error::Error>> {
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
`PagedStore<ReadOnly>`, which has no methods that change it. Opening checks the header and the directory, but not the
pages, which would read the whole file: a damaged page shows only when it is read, as wrong locations or a panic.
Opening with `open_and_fix` also reads every page, for a store that may be damaged, as after a disk error. It rebuilds a
lost directory file, and drops pages whose CRC is wrong, reporting the node IDs they held:

```rust
use osmflow::nodes::prelude::*;
use osmflow::nodes::PagedStore;

fn check_and_fix() -> Result<(), Box<dyn std::error::Error>> {
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
use osmflow::nodes::prelude::*;
use osmflow::Location;

/// The locations of a way's nodes, leaving out those the store does not have.
fn way_geometry(store: &impl NodeRead, refs: &[u64]) -> Vec<Location> {
    let mut reader = store.reader();
    refs.iter().filter_map(|&id| reader.get(id)).collect()
}
```

### Importing the planet fast

What made the most difference for the planet, in the [performance](#performance) runs:

* **Decode only what the store needs** from the PBF file: node IDs and locations, and way refs. osmflow's PBF reader
  decodes only the objects and fields its `Select` asks for.
* **Push nodes in batches** of a million or so with `BulkLoad::push_batch`: the paged store checks and encodes big
  batches on all cores.
* **Keep one reader per thread** (`NodeRead::reader`) for all of a thread's lookups: it keeps up to 4,096 decoded blocks
  (2 MiB), and making one has a cost.
* **Read the store into memory before resolving**, if it fits but is not there, as after a restart: `NodeRead::preload`
  reads it in big reads on all cores, 28 s for the planet's 55.6 GB `PagedStore`. Preload right after a load too:
  the OS may have dropped parts of the store from memory, and reading them back a page at a time, on first use, is much
  slower.
* **Give the memory back after the load.** glibc's allocator can keep much of the memory that the PBF decoding freed,
  which leaves too little for the store; `malloc_trim(0)` returns it to the OS.

## Performance

Measured with the repository's [node store
benchmark](https://github.com/nyurik/osmflow/blob/main/benches/README.md#node-location-stores) on an AWS m8gd.4xlarge
(16 Graviton4 cores, 64 GiB of memory), with the PBF files and the stores on its local 885 GB NVMe disk, using District
of Columbia from Geofabrik (2026-10-27), and North America and the planet cut from the planet of 2026-10-19. Every load,
verify and resolve read the PBF file from the disk, not the page cache. Lookups were measured with each store in memory:
District of Columbia, and the nodes of North America up to ID 3 billion (14 GB as a dense store). The changes are real
diffs, each one commit: the next two days of District of Columbia and of North America from Geofabrik, and for the
planet, the 1,440 minute diffs of the day after its snapshot, then the next day as one daily diff. An instance's local
disk is lost when the instance stops, so it suits a store that can be rebuilt, with the PBF file kept elsewhere and
copied to it first.

### The planet

10.9 billion nodes, and 1.23 billion ways with 13.0 billion node refs:

|                                                             | `PagedStore`                                | `DenseStore`                |
|-------------------------------------------------------------|---------------------------------------------|-----------------------------|
| On disk                                                     | 55.6 GB, 5.08 bytes per node                | 106.7 GB, 9.74              |
| Bulk load from the PBF file                                 | 80 s                                        | 121 s                       |
| Resolve all ways, with none of the store in memory at first | 94 s: 28 s to read the store in, then 66 s  | larger than memory          |
| A day of minute diffs: 1,440 commits, 3.65 M node changes   | 4.2 s; per commit median 2.5 ms, 99th percentile 9.8 ms | 15.2 s; 8.1 ms, 52.7 ms |
| The next day as one diff: 4.0 M node changes                | 2.8 s                                       | 13.2 s                      |

* **Importing the planet** with the paged store, from the PBF file to every way's node locations, takes 3 minutes.
* **Only the paged store fits in the host's memory**, so only it resolves the planet's ways at memory speed: 197
  million refs a second once it is in memory. The PBF reader keeps the file from pushing the store out of memory
  ([pbf.md](https://github.com/nyurik/osmflow/blob/main/crates/osmflow/docs/pbf.md#threads-and-memory)).
* **Keeping up with the planet costs little:** a day of minute diffs takes 4.2 s in the paged store, and 15 s in the
  dense store, which writes the 107 GB file in place.

Whole extracts:

|  | Nodes | On disk | Bytes per node | Bulk load | Load peak memory |
|---|---:|---:|---:|---:|---:|
| District of Columbia, `PagedStore` | 1,958,977 | 10 MB | 4.90 | 35 ms | 0.1 GB |
| District of Columbia, `DenseStore` | 1,958,977 | 167 MB | 85.29 | 343 ms | 0.1 GB |
| District of Columbia, `MemoryStore` | 1,958,977 | none | | 87 ms | 0.2 GB |
| North America, `PagedStore` | 2,631,813,110 | 13.7 GB | 5.20 | 22.6 s | 0.4 GB |
| North America, `DenseStore` | 2,631,813,110 | 64.3 GB | 24.43 | 73.0 s | 0.3 GB |
| Planet, `PagedStore` | 10,949,467,954 | 55.6 GB | 5.08 | 80.5 s | 1.1 GB |
| Planet, `DenseStore` | 10,949,467,954 | 106.7 GB | 9.74 | 121.0 s | 0.9 GB |

District of Columbia: 1,958,977 nodes, 2,507,044 way node refs. Two runs of the paged store were within 3%:

|  | `PagedStore` | `DenseStore` | `MemoryStore` |
|---|---:|---:|---:|
| Bulk load | 35 ms, 55.4 M/s | 343 ms, 5.7 M/s | 87 ms, 22.5 M/s |
| Resolve all ways | 22 ms, 115.1 M/s | 18 ms, 136.0 M/s | 21 ms, 118.1 M/s |
| Verify every node | 30 ms, 65 M/s | not run, see below | 117 ms, 17 M/s |
| Lookups in way order, 16 threads | 312.6 M/s | 1,474.9 M/s | 762.1 M/s |
| Lookups in way order, 1 thread | 17 ns | 5 ns | 16 ns |
| Lookups in random order, 16 threads | 70.2 M/s | 1,765.2 M/s | 861.7 M/s |
| Lookups in random order, 1 thread | 221 ns | 8 ns | 16 ns |
| Two days of Geofabrik diffs, 1,998 node changes: per commit | 2.5 ms | 0.8 ms | under 0.1 ms |

North America, the nodes up to ID 3,000,000,000: 618,849,582 nodes, 706,551,105 way node refs. Two runs of the paged
store were within 3%:

|  | `PagedStore` | `DenseStore` | `MemoryStore` |
|---|---:|---:|---:|
| Bulk load | 5.9 s, 105.3 M/s | 16.2 s, 38.1 M/s | 56.9 s, 10.9 M/s |
| Resolve all ways | 13.6 s, 52.0 M/s | 11.6 s, 60.8 M/s | 15.4 s, 45.9 M/s |
| Verify every node | 7.6 s, 81 M/s | 12.6 s, 49 M/s | 69.7 s, 9 M/s |
| Lookups in way order, 16 threads | 95.6 M/s | 1,435.2 M/s | 393.8 M/s |
| Lookups in way order, 1 thread | 40 ns | 5 ns | 44 ns |
| Lookups in random order, 16 threads | 26.8 M/s | 1,152.2 M/s | 385.5 M/s |
| Lookups in random order, 1 thread | 612 ns | 14 ns | 44 ns |
| Two days of Geofabrik diffs, 72,126 node changes: per commit | 130 ms | 68 ms | 2.9 ms |

* **Resolving North America's ways is bound by reading the file**, not by the stores: the ways of the whole file are
  read and decoded, and their refs above ID 3 billion left out.
* **Opened read-only**, every store resolves and looks up as fast as opened to read and write, within 1%.
* **A dense store's verify reads every ID up to the highest**, holes included: District of Columbia's IDs reach 14
  billion, so it would read 115 GB for 2 million nodes.
* **The memory store took 18.8 GB for North America's nodes up to ID 3 billion** once loaded, 30 bytes per node, and
  27.0 GB at the peak of its load, while its hash map grew.

Load peak memory counts the pages of memory-mapped files that were in memory; the OS drops them when it needs room.
`crates/osmflow/benches/nodes.rs` is a quicker check of the same stores on synthetic nodes.

### Design notes

* **Writing a memory-mapped file that the page cache holds in large folios** (XFS does, for big writes) dirties whole
  folios, so a dense store commit after a bulk load would write far more than it changed. The dense store writes
  changes to the file instead, and maps it only to read.
* **Writing past the end of a file on XFS** has it keep blocks after it, which take disk space once the file grows past
  them, so the dense store grows its file before writing past its end.
* **An async API would not make the stores faster.** Loading is limited by encoding and the disk, and the checkpoints
  are already made durable on another thread. With the store in memory, a lookup takes tens of nanoseconds, much less
  than an async task switch. With a store larger than memory, the disk's random reads are the limit.
