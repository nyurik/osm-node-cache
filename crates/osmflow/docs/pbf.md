# Reading PBF files

How osmflow's PBF reader reads a file, what it skips, and how it decodes the rest. The [README](../README.md) has
examples of each kind of read, and its [performance](../README.md#performance) section the measurements; this page
does not repeat them.

## Blobs and blocks

A PBF file is a sequence of blobs. Each starts with a 4-byte length, then a blob header, which says the blob's type and
size, then the blob. The first blob holds the file header; every other one holds a block of objects, compressed or not.
A block has a string table, which every tag key and value, member role and user name of its objects points into, and
groups of nodes, ways or relations.

* **The header** is read when a file is opened. Its required features must be among those osmflow reads
  (`OsmSchema-V0.6`, `DenseNodes`, `HistoricalInformation` and `LocationsOnWays`); any other one is an error. Its
  optional `Sort.Type_then_ID` says the file is sorted, which some skips rely on (below). Its replication fields say
  where the file's diffs continue it.
* **A file that starts with a data blob,** which the format does not allow but some tools write, gets an empty header.
  An empty file, or one that starts with a blob of another type, is an error.
* **Only zlib and uncompressed blobs are read.** The published files use zlib. A blob compressed with lzma, bzip2, lz4
  or zstd, which the format allows, is rejected with an error naming the compression.
* **Blobs of other types,** neither the header nor data, are skipped unread.

## Reading the file

One thread reads the file, and hands each data blob to a pool of threads that decompress and decode it.

* **Whole blobs** are read in large reads, one after another: 8 MiB of the file at a time into a buffer, from which
  each blob is copied into a buffer of its own, with the rest of a longer blob read straight into it.
* **Only the start of a blob** is read when the rest may not be needed: 4 KiB with its header, enough to find the next
  blob, so the reader moves through the file quickly. The pool thread that takes the blob reads more of its start, from
  128 KiB and twice as much each time, until it can tell whether the block is wanted; it reads the rest only if it is.
  Threads read their own part of the file with positioned reads, so they share one open file.
* **`ReadMode` decides which,** in reads that may skip blocks; a read that skips nothing always reads whole blobs.
  * `Auto`, the default, reads only the start of a blob larger than 512 KiB while most recent blocks were skipped, and
    whole blobs otherwise. So a pass over only the planet's ways reads little of its node blobs, and a read that keeps
    most blocks makes no small reads.
  * `Sequential` always reads whole blobs: the fewest reads, for storage where each read is slow to start, such as a
    network volume.
  * `Selective` reads only the start of every blob: the least data, for a fast local disk when most of a file is
    skipped.
* **Each thread keeps a zlib decompressor and its output buffer,** reused from blob to blob, so decompressing a blob
  allocates nothing once the buffer is as large as the largest blob.

## Skipping blocks

A read skips what it does not want before decoding it, and where it can, before decompressing all of it.

* **By object type:** `Select` names the object types a read wants. Groups of the others are not decoded, and blocks
  holding only those are skipped whole, as below.
* **By strings:** `PbfReader::nodes_where`, `ways_where` and `relations_where` each take a test of a block's strings,
  a `BlockStrings`. Its `has` and `has_prefix` say whether some object of the block uses a string, as a tag key or
  value, a role or a user name. A block whose strings a test rejects cannot hold a wanted object of that type, so its
  objects of that type are not decoded. A block a test accepts is decoded, and its objects still need checking one by
  one: the test only says that the block might hold a wanted one.
* **Peeking:** the string table comes first in a block, then its groups. So the pool thread inflates only the start of
  a block, up to its string table and the first field of its first group, and runs the tests on that. When no type the
  read wants is left, the block is skipped whole, without inflating the rest.
* **Skipping a block whose first group is not wanted** also needs to know that the block holds nothing else. That is
  certain when the block has a single group, as osmium writes them. In a file sorted by type, it is also certain when
  the next blob starts with the same type, or when the type is relations, the last one. Otherwise the block is
  inflated and decoded without the unwanted types.
* **Raw blobs, and starts not laid out as expected,** are read whole and decoded with the same filters, so the result
  never depends on how much of a blob was peeked at.

## Three ways to read

| Read | Order | Where the caller's code runs |
|---|---|---|
| `blocks` | file order | on the calling thread, which also reads and decodes |
| `par_ordered` | file order | on the calling thread, an iterator; a reader thread reads and the pool decodes ahead |
| `par_unordered` | any order within a run of one object type | on the pool, a callback; the calling thread reads |

* **`par_unordered` is the fastest:** the caller's work runs on every thread of the pool. Its order has one rule.
  Consecutive blocks holding the same object types form a run, and a run's callbacks start only once the callbacks of
  every block before it have returned. In a sorted file, every node is handled before any way, and every way before
  any relation, as building ways from stored nodes needs. Blocks are still read and decoded ahead while a run's last
  blocks finish, so a barrier costs only that short tail. A file whose types come back gets a barrier at every change.
  A block mixing types is a run of its own, a block with none of the selected types joins the run it is in, and a
  block skipped whole is not handed out. It stops at the first error, from reading or from the callback.
* **`par_ordered` hands out blocks in file order.** The reader thread reserves a slot for each blob at the back of a
  queue, and the pool decodes the blob into it; the iterator takes slots from the front. Its consumer works on one
  thread, which limits it when it does much per block. Dropping the iterator stops the reader.
* **`blocks` decodes on the calling thread,** with no pool, for small files and tests.

## Threads and memory

* **One pool is shared by every reader** not given one: it is built on first use, with two threads fewer than the
  cores, one left for reading the file and one for the consumer. `PbfReader::with_pool` gives a reader the caller's
  pool instead. osmflow never configures rayon's global pool, which belongs to the program.
* **The bytes in flight are bounded:** 16 MiB per pool thread, and at least 256 MiB. A blob counts three times the
  larger of its stored size and its declared uncompressed size, until its block is handed out, or its callback
  returns. The reader waits when the limit is reached, so memory stays the same whatever the file's size. A blob larger
  than the whole limit is admitted alone.
* **A read started from a thread of its own pool** does not block that thread: while it waits for bytes or for a
  block, the thread runs the pool's other work, so even a pool of one thread finishes.
* **A panic stops the read.** A panicking task closes the budget, so the reader stops, and a panic of `par_ordered`'s
  reader thread is raised again in the iterator.

## Decoding

* **Columns.** A decoded `Block` holds its objects as columns: IDs, raw coordinates, tag key and value indexes into
  the string table, metadata, way refs and relation members, with the end of each object's range in each column. Its
  `nodes`, `ways` and `relations` give views of one object each, which read the columns. Nodes from plain and dense
  groups come out the same way.
* **A block borrows nothing.** Its string table is checked as UTF-8 once, and copied into one string, when tags,
  metadata or relations are selected. The decompressed data stays in the thread's buffer, so a block can move to the
  consumer's thread and outlive the blob it came from.
* **The protobuf messages** of blobs and blocks are read through `buffa` views of `proto/osmflow_raw.proto`, a
  projection of the format's schema. It declares only the fields osmflow reads, so the rest are skipped unread. It
  declares plain objects, dense nodes and their metadata as bytes, so they are decoded only when selected. The file
  header is read with the format's own schema.
* **Dense nodes** are decoded through `buffa` views of two messages: one with every field, and one with only IDs and
  coordinates, used when neither tags nor metadata are selected, which skips the nodes' tags and metadata unread.
* **Plain nodes, ways and relations** are read by osmflow's own field reader, straight into the columns, with no
  object allocated per message.
* **Every way the format allows a field to be written is read:** packed arrays split into several fields or written
  unpacked, one value per field, and message fields in several occurrences, which protobuf merges.
* **Coordinates** are kept as the file stores them, and the block's granularity and offsets applied when read:
  `lat_nano` and `lon_nano` in nanodegrees, `decimicro_lat` and `decimicro_lon` in the 1e-7 degrees OSM uses, truncated
  if the file is more precise.
* **The layout** of each block, from `Block::layout`, tells how the file stores it: the blob's size and compression,
  the uncompressed size, the string table's size, the granularity and offsets, and each group's type and object
  count. It is recorded while the block is decoded; `osmflow dump --blocks` prints it.

### Tags

* **Whether tags are decoded is a type.** `Select::all` gives a `Select<Tags>`; `Select::without_tags` and
  `Select::node_locations` give a `Select<NoTags>`. Blocks and objects carry the type, and only those of a read with
  tags have tag methods, so code that reads tags from a read without them does not compile.
* **Metadata is a run-time switch:** `Select::without_metadata` leaves it out, and `info()` is then `None`, as it is
  for a file without metadata. A field the file leaves out reads as a version of -1, a changeset or user ID of 0, an
  empty user name, or `visible` true.
* **`tags()`** gives each object's (key, value) pairs in file order, as string slices of the block's table.
* **`Block::tag_key` and `tag`** find one tag without comparing strings. `tag_key` looks up a key once per block, in
  one pass over its string table, and gives a `TagKey`; `tag` then finds the key's value in an object by comparing
  string indexes. A block without the key gives `None`, so none of its objects has the tag. A table that holds the key
  twice, which the format allows, makes `tag` compare strings instead. A `TagKey` belongs to its block: using it on an
  object of another block panics.

### Way refs

* **A way's node IDs stay packed** as the file stores them, zigzag varints of the deltas from one ID to the next. They
  are counted and checked when the block is decoded, and decoded when `Way::refs` is iterated, which gives their exact
  number. Refs split into several fields are joined, and refs written unpacked are packed again, so every way's refs
  are one packed run.
* **The decoding happens on the thread that iterates them.** In `par_unordered`, that is a pool thread. In
  `par_ordered`, it is the calling thread, so a consumer that reads every ref does that work on one thread.
* **Locations on ways** (`LocationsOnWays`), when the file has them, are decoded with the way, and given by
  `Way::locations`.

### Strict checks

Data that breaks the format is an error, never silently cut short or read past:

* blob headers over 64 KiB, and blobs or declared uncompressed sizes over 32 MiB;
* a compressed blob without its declared size, and one that inflates to another size than it declares;
* string indexes past the string table, and strings that are not UTF-8;
* arrays that must have the same length and do not: tag keys and values, dense IDs and coordinates, dense metadata,
  dense tags and nodes, member IDs, roles and types, way refs and their locations;
* malformed varints, wire types a field does not allow, member types other than node, way and relation;
* objects without an ID, plain nodes without a coordinate, and a granularity that is not positive;
* a file that ends inside a blob.

Each is a `PbfError::Format`, with a `PbfFormatError` saying what is wrong and a `BlobPosition`: the blob's index (0 is
the header) and the byte offset of the length before its header.

## Node locations for ways

A way stores only its nodes' IDs. To build its geometry, osmflow looks the IDs up in a node location store
([nodes.md](nodes.md)), loaded from the same file. This needs the `pbf` and `nodes` features, and turning ways into
`geo-types` geometries the `geometry` feature.

* **`PbfReader::load_nodes`** bulk loads a file's node locations into a store. It reads in file order, with only node
  IDs and coordinates decoded and the other blocks skipped. It copies each block's nodes straight from its columns, and
  hands the store about a million at a time, many blocks' worth, so the paged store encodes them on all threads. The
  file must be sorted, as the stores take IDs in ascending order. Nodes with negative IDs, which only editors' files
  have, are left out. The store's stamp becomes the file's replication sequence, or 0. A store holding an interrupted
  load of the same file resumes it.
* **`Locator`** looks up a way's nodes in a store, for one thread. `line_string` gives the way as a `LineString` in
  degrees, and `polygon` gives a closed way of four nodes or more as a `Polygon` without holes. A node the store does
  not have, as when a way leaves an extract, gives `GeometryError::MissingNode` with its ID.
* **Areas or closed lines.** A closed way is a fence or a roundabout as often as a building, so its tags decide.
  `AreaRules` holds a rule for each key: a closed way with the key is an area with any value (`building`, `landuse`),
  with only some values (`highway=services`, `railway=platform`), or with all values but some (`natural` but not
  `coastline`). `area=yes` and `area=no` override every other tag. The built-in list is the public-domain
  [osm-polygon-features](https://github.com/tyrasd/osm-polygon-features) list, which osmtogeojson and Overpass turbo
  use, changed where the OpenStreetMap wiki page of a tag says it is a line (`boundary=administrative`,
  `aeroway=runway`, `leisure=track` and a few more) or an area (`landcover`). A tag whose value is `no` never makes an
  area. `add` and `remove` change the list, and `AreaRules::none` starts from no rules.
  `Locator::area_or_line` builds a closed way that the rules make an area as a `Polygon`, and any other way as a
  `LineString`.
* **Nodes are looked up only when asked for.** A way's refs are decoded, and its nodes looked up, only when its
  geometry is built, so ways a caller passes over cost nothing.
* **A file with locations on its ways needs no store:** `line_string` takes the locations stored on the way when it has
  them.
* **`PbfReader::par_resolve`** is `par_unordered` with a `Locator` for each callback. Locators are kept in a list
  between callbacks and reused, so there are no more than the pool has threads, and each keeps the store blocks it
  decoded for the ways that follow.
