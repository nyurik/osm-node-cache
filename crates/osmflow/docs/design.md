# How osmflow is built

osmflow reads OpenStreetMap data from files: PBF files, from a city extract to the planet, and osmChange diffs with
the replication streams that publish them. It also keeps node locations by node ID, for building ways from their
nodes. This page says how its parts work together, and why they work as they do. The [README](../README.md) has
examples of each part and the performance measurements.

* [Reading PBF files](pbf.md): blobs and blocks, skipping what a read does not want, decoding into columns, the three
  ways to read a file, and node locations for ways.
* [osmChange files and replication](osc.md): the osmChange parser, state files, and reading a replication stream from
  a directory or a server.
* [Node location stores](nodes.md): the three stores, their files, and how they keep changes through a crash.

## The parts

| Part | Module | Feature | Needs |
|---|---|---|---|
| PBF reader | the crate root: `PbfReader`, `Block`, `Select` | `pbf` | `buffa`, `flate2`, `rayon` |
| Node location stores | `osmflow::nodes` | `nodes` | `memmap2`, `crc32fast`, `rayon` |
| Node locations for ways | `PbfReader::load_nodes`, `PbfReader::par_resolve`, `Locator` | `pbf` and `nodes` | |
| Ways as geometries | `Locator::line_string`, `Locator::polygon`, `GeometryError` | `geometry` | `geo-types` |
| osmChange and replication | `osmflow::osc` | `osc` | `flate2`, `memchr` |
| Downloading replication files | `Replication::http`, `HttpFetch` | `http` | `ureq` |

* **The default features are `geometry` and `osc`.** `geometry` turns on `pbf` and `nodes`, and `http` turns on `osc`.
* **Each part builds without the others,** so a program that only keeps a node store, or only reads diffs, compiles
  none of the PBF reader. The parts share only the types in the crate root that every feature has: `Location`,
  `Member` and `MemberType`.
* **A location is two `i32` in 1e-7 degrees,** as OSM stores them. The PBF reader, the stores and the osmChange parser
  all give and take this one type, so a location read from a diff can be compared with one from a PBF file or a store
  as integers.
* **Decompression is pure Rust:** `flate2` with its `zlib-rs` backend, for zlib blobs and gzip diffs, so osmflow needs
  no C library.
* **The protobuf code is generated and checked in,** so building osmflow needs no `protoc`. The `codegen` example
  regenerates it (`just update-generated`), and CI's `just codegen-check` fails if the checked-in code is stale. It is
  a program, not a build script, so no build ever runs `protoc`.

## Threads, not async

Every part is synchronous, and runs its parallel work on operating-system threads.

* **The PBF reader** reads the file on one thread and decodes on a `rayon` pool. One reader thread with large reads
  keeps a local disk busy, and the decoding is bound by the CPU, which a pool of threads uses fully.
* **Replication** downloads one file at a time, as the servers ask, on a background thread, and decodes the next
  diffs on threads of their own.
* **The node stores** answer a lookup in tens of nanoseconds once they are in memory, far less than an async task
  switch; [nodes.md](nodes.md#design-notes) has more.

A program that runs an async runtime calls osmflow from a thread of its own, or with its runtime's way of running
blocking code. A replication server can still be reached with an async HTTP client: `osc::Fetch` is a small trait that
any client can implement.

## Errors

Each part has its own error type, named for what it reads:

| Error | From | Details in |
|---|---|---|
| `PbfError` | the PBF reader, and the reads that load or use a node store | `PbfFormatError`, with the `BlobPosition` it was found at |
| `OscError` | the osmChange parser and replication | `XmlError` with its `XmlPosition`, and `StateError` |
| `NodeStoreError` | the node stores | |

* **An error that breaks a format is one variant of the top error, with a detail enum,** such as
  `PbfError::Format { position, kind: PbfFormatError::StringIndex(7) }`. Code that only reports errors prints the top
  one; code that handles one case matches the detail.
* **Every error says where:** a PBF error names the blob and its byte offset, an osmChange error the file, the byte
  offset and the element, and a store error the file.
* **`PbfError::Nodes` wraps a `NodeStoreError`,** for the reads that load or use a store, so they return one error type.
* **There are no public `Result` aliases.** Functions return `Result<T, PbfError>` and so on, written out, so the error
  type shows in every signature.

## Unsafe code

The workspace denies unsafe code. Two functions allow it, each with its reason and a `SAFETY` comment:

* `nodes::file::map_file` maps a store's file into memory. A memory map is unsafe because the file may change under it;
  the stores lock their files and only ever grow them.
* `nodes::codec::zeroed` allocates zeroed memory for a reader's cache of decoded blocks, so the operating system gives
  memory only to the parts that are used.

The PBF reader, the osmChange parser and the generated protobuf code have no unsafe code.

## Testing

* **Fixtures from other projects.** `fixtures/` holds PBF and osmChange files from other readers and tools, each in a
  directory of its own with a README saying where it came from, at which commit, under which license, and what it
  tests, and with the project's license file copied beside it where it has one. They come only from projects under
  permissive licenses (MIT, Apache-2.0, BSD, the Boost Software License, WTFPL, public domain) and from the
  OpenStreetMap and Geofabrik servers. Files with OpenStreetMap data are under the Open Database License.
* **Generated PBF fixtures,** in `fixtures/generated/`, for cases no published file has: packed arrays split or
  written unpacked, message fields in several occurrences, groups of every type in one block, unusual granularities,
  negative IDs, unsupported compressions, and broken files for the format errors. `tests/generated.rs` writes them byte
  by byte with a minimal protobuf encoder, and fails if a checked-in file differs from what it builds.
* **Every PBF fixture is read every way:** on one thread, ordered and unordered on a pool, with each object type
  filtered out, and with each `ReadMode`. All of these must give the same objects. The unordered read also checks its
  barriers between object types as it reads.
* **Compared with other readers:** the `osmpbf` crate must read the same objects from each fixture, metadata included,
  and `osmpbfreader` the same objects, tags and coordinates. The result is kept as an `insta` snapshot. A file osmflow
  rejects snapshots its error, and what `osmpbf` does with the same file.
* **osmChange cases inline:** `tests/osc.rs` writes most of its cases in the test itself, one feature or one error
  each. It also reads a real planet minute diff and the change and state files of other projects. It checks that the
  result does not depend on how the input is split into reads, down to one byte at a time, and that every node of a PBF fixture written as
  osmChange reads back at the same location.
* **Replication** is tested against a mock `Fetch`, and, with the `http` feature, against a local HTTP server.
* **Pools:** reads on a caller's pool of one thread, started from that thread, must finish.
* **Node stores** all pass one test suite, written against their traits.

Benchmarks compare osmflow with `osmpbf` and `osmpbfreader` on the same files; the
[README](../README.md#performance) has the results, and [`benches/README.md`](../../../benches/README.md) what they
measure.
