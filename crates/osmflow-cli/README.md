# osmflow-cli

The `osmflow` command, built on the [`osmflow`](https://crates.io/crates/osmflow) crate: it dumps the objects of
OpenStreetMap PBF files and the changes of osmChange files and replication streams as text or GeoJSON, and summarizes
them.

```sh
cargo install --path crates/osmflow-cli
```

In this repository, `just run …` builds it in release mode and runs it with the same arguments, and `just dump …`
and `just stats …` run its two commands: `just stats dc.osm.pbf`.

## `osmflow dump`

```sh
osmflow dump dc.osm.pbf dc.geojson                          # every object, as one FeatureCollection
osmflow dump dc.osm.pbf --metadata | less                   # text: the header, then one line per object
osmflow dump planet.osm.pbf --id w4097656 way.geojson       # one way of the planet, with its geometry
osmflow dump planet.osm.pbf --id 10,15                      # node, way and relation 10 and 15, as text
osmflow dump ny.osm.pbf -t way --tag highway=motorway,trunk roads.geojsonl
osmflow dump dc.osm.pbf --blocks --limit 100                # each block's blob, strings and groups
```

The output goes to the file given after the input, or to standard output. Its format follows the file's extension, or
`--format`:

| Format | Extension | Holds |
|---|---|---|
| `text` | anything else | `#` lines with the file's header, then one line per object: `n…` with its location, `w…` with its node IDs, `r…` with its members, each with its tags as JSON |
| `geojson` | `.geojson`, `.json` | one FeatureCollection |
| `geojsonseq` | `.geojsonl`, `.geojsons`, `.ndjson` | one Feature per line, for large outputs and line-based tools |

Choosing objects; an object must pass every flag given:

| Flag | Chooses |
|---|---|
| `--id 10`, `--id w15,r7` | objects by ID: a plain number of any type, or with `n`, `w` or `r` before it of one. In a file whose header says it is sorted by type and ID, as the planet's and Geofabrik's do, the read stops once it is past the highest ID asked for; `osmflow stats` checks whether a file really is in that order. |
| `-t, --type way,relation` | object types; the blocks of other types are skipped unread |
| `--tag highway`, `--tag highway=primary,trunk` | objects with the key, or with one of the values; given more than once, any of them. Blocks without the key are skipped unread. |
| `--limit N` | stops after N objects |

A file over 100 MiB is dumped whole only with `--all`; otherwise it needs `--id`, `--tag` or `--limit`.

Geometry:

- **Nodes** are points.
- **Ways** are lines, built from their nodes' locations. A closed way is a polygon when its tags make it an area, by
  osmflow's built-in rules (`AreaRules`): `building`, `landuse`, `amenity`, `natural` but not `coastline`,
  `highway=services`, `railway=platform`, and so on, with `area=yes` and `area=no` overriding the rest.
  `--area KEY[=VALUES]` adds a tag that makes areas, `--not-area KEY[=VALUES]` takes one away, and
  `--no-default-areas` starts from none. A way whose nodes are not all in the file has no geometry, and an `@error`
  property says which node is missing. Text output has geometry, as WKT, with `--geometry`.
- **Relations** have no geometry; GeoJSON gives their members in `@members`.

To build ways, a whole-file dump keeps every node location in memory, at about 20 to 40 bytes per node: an extract
fits, while the planet's 11 billion nodes would need hundreds of GB. A dump of chosen objects reads the file twice
more: once for the ways it will write, then once for only their nodes, which are all it keeps. So a few ways of the
planet need little memory, but `--tag highway` on a large file still keeps the nodes of every road.

GeoJSON properties hold the object's tags and, starting with `@`, its type, ID and, with `--metadata`, its version,
timestamp, changeset, user ID and user name. Metadata a file leaves out is left out, rather than written as a version
of -1 or a changeset of 0. A way without geometry lists its node IDs in `@nodes`.

## osmChange files and replication streams

`dump` and `stats` also read osmChange input. What an input is comes from its contents, not its name:

| Input | Is read as |
|---|---|
| an `http://` or `https://` URL | a replication server, such as `https://planet.openstreetmap.org/replication/minute` |
| a directory | a replication stream laid out as the servers lay theirs out: `000/004/956.osc.gz` and `956.state.txt` |
| a file that is gzip, or starts with `<` | an osmChange file (`.osc`, `.osc.gz`) |
| a file with a `sequenceNumber` line | a replication state file |
| anything else | a PBF file |

```sh
osmflow dump 956.osc.gz                                    # one line per change: its action, then the object
osmflow dump 956.osc.gz -t node --action delete deleted.geojson
osmflow stats 956.osc.gz                                   # changes by type and action, tags, extent, time span
osmflow stats deltas/district-of-columbia                  # the latest state, and the diffs the directory has
osmflow stats deltas/district-of-columbia --seq 4955..4956 -f json
osmflow stats https://planet.openstreetmap.org/replication/minute    # the latest state, and how long ago it was
osmflow dump https://planet.openstreetmap.org/replication/minute --cache minute --seq latest --tag highway
osmflow stats https://download.geofabrik.de/north-america/us/district-of-columbia-updates --cache dc \
    --since district-of-columbia-261027.osm.pbf             # the changes since the extract was made
```

| Flag | Does |
|---|---|
| `--seq N`, `--seq A..B`, `--seq A..`, `--seq latest` | which diffs of a stream to read, both ends included; `A..` runs to the latest state. Without it or `--since`, `dump` of a stream reads the latest diff, and `stats` shows only the latest state. |
| `--since FILE`, `--since TIME` | the diffs after what is already applied, up to the latest state: after a Geofabrik extract by the sequence in its header, when the stream is the server its header names; after a state file of the stream; the changes made after a time such as `2026-11-09T21:00:00Z` or `2026-11-09`; or after a planet file, from `--overlap` before its timestamp, as a change can show up in a diff that much after it is made. `osmflow seq` shows where each starts. |
| `--overlap DURATION` | how late a change may show up in a diff, as `3m` (the default), `90s` or `1h`: how much earlier `--since` starts after a planet file |
| `--cache DIR` | the directory a server's files are downloaded into, once each, and read from after; needed to read a server's diffs |
| `--action create,modify,delete` | only changes in these sections |
| `--changeset ID,...` | only changes made in these changesets |
| `--user NAME,...`, `--uid UID,...` | only changes by these user names or user IDs |

`--id`, `-t`, `--tag`, `--limit`, `--metadata` and the output formats work as for PBF files. Each change is written
with its action: `modify n49778624 lat=… lon=…` in text, and `"@action":"modify"` in GeoJSON, with `"@sequence"` for
the diff of a stream it is in. Nodes are points, deleted ones too when the file gives their last location; ways and
relations have no geometry, as a diff has the locations of the nodes it changes only. Geofabrik's public diffs have no
changesets or users, so `--changeset`, `--user` and `--uid` match nothing there.

`stats` of osmChange input counts its changes by type and action, and the objects they change. The columns under
`created or modified` are of the versions created and modified: how many, how many of them have tags (`tagged`), and
their tags in all (`tags`). Deletions are counted apart, without their tags: the planet's diffs give deleted objects
none, and other writers may or may not.

```text
              --------------- created or modified ---------------
                    create       modify       tagged         tags       delete      objects
nodes                  744          125           19          100          194        1,063
ways                    49           44           93          367            2           94
relations                0            3            3           39            0            3
total                  793          172          115          506          196        1,160
```

Reading servers needs the `http` feature, which is on by default; `cargo install --no-default-features` builds a
command without network code.

## `osmflow stats`

```sh
osmflow stats dc.osm.pbf
osmflow stats planet.osm.pbf --metadata -f json
```

```text
file          district-of-columbia.osm.pbf (21.0 MB)
written by    osmium/1.16.0
features      OsmSchema-V0.6, DenseNodes
optional      Sort.Type_then_ID
header bbox   lon -77.1201 .. -76.90906, lat 38.790988 .. 38.99603
node extent   lon -77.1926236 .. -76.8316112, lat 38.569216 .. 39.2377262
replication   sequence 4954 at 2026-10-27T21:23:59Z
updates       https://download.geofabrik.de/north-america/us/district-of-columbia-updates
timestamps    not read; --metadata reads them
order         by type, then ID (the header says sorted)
blocks        282 (282 zlib): 21.0 MB in blobs, 49.0 MB uncompressed; 346,158 strings in 3.2 MB
groups        245 dense nodes, 36 ways, 1 relations
way refs      2,507,044 node IDs in all ways
members       299,614 members of all relations
                        count          tagged            tags   IDs
nodes               1,958,977         110,044         400,171   281072 .. 14324055101
ways                  285,566         281,052       1,735,321   4681186 .. 1572294636
relations               5,284           5,284          30,068   62152 .. 21811917
total               2,249,827         396,380       2,165,560
```

- **The node extent** is the smallest and largest location of the file's nodes, not of its ways' geometry. An extract
  includes nodes outside its bounding box that its ways and relations use, so it is often larger than the header's.
- **`--metadata`** also reads each object's version, timestamp and visibility, for the time span, the highest version
  and the deleted objects of a history file. Metadata is much of what a block holds, so decoding it is off by default.
  A timestamp of 0, which some writers give objects without one, is left out of the span.
- **The order line** checks every object, whatever the header claims.
- **The replication and updates lines** are what the header says, not checked against a server. A Geofabrik extract
  gives the sequence of its own stream's diff it is up to date with, and that stream's URL on the `updates` line; the
  sequence means nothing on another stream. A planet file gives only a timestamp, the newest of its objects.
  `osmflow seq` finds the sequence of any stream for such a file.

## `osmflow seq`

Finds the diff of a replication stream at a time, or the one a PBF or state file is up to, and the diffs to read after
it; or with `latest`, the stream's latest diff. It reads state files only: the latest, and a few more to search by
time. A server is read without a cache unless `--cache` gives one.

```sh
osmflow seq https://planet.openstreetmap.org/replication 2026-11-01
osmflow seq https://planet.openstreetmap.org/replication/day latest
osmflow seq https://download.geofabrik.de/north-america/us/district-of-columbia-updates dc.osm.pbf -f json
```

```text
The latest diff of https://planet.openstreetmap.org/replication/minute is 7366179, at 2026-11-10T03:48:50Z, 1m ago.
The last diff at or before 2026-11-01T00:00:00Z is 7353229, at 2026-10-31T23:59:58Z.
The changes made after 2026-11-01T00:00:00Z are in diffs 7353230 to 7366179 (12,950 diffs). Read them with --seq 7353230..
Through the day, hour and minute streams, catching up to the latest diff takes about 61 diffs, in this order:
   9 day diffs, up to 2026-11-10T00:00:00Z
   3 hour diffs, up to 2026-11-10T03:00:00Z
  49 minute diffs, up to 2026-11-10T03:48:50Z
```

```text
The latest diff of https://download.geofabrik.de/north-america/us/district-of-columbia-updates is 4966, at 2026-11-08T21:21:00Z, 1d 5h ago.
district-of-columbia-261027.osm.pbf is up to date with diff 4954, at 2026-10-27T21:23:59Z, as its header says.
The changes after it are in diffs 4955 to 4966 (12 diffs). Read them with --seq 4955..
```

- **A catch-up through several streams** is shown for a stream named `day`, `hour` or `minute` with the others beside
  it, as on the planet's server, and for the directory or URL that holds them, which is read as its `minute` stream.
  By default it is estimated from each stream's latest state alone, read all at once: day and hour diffs end on
  their interval, so they are counted exactly, and minute diffs at one a minute.
- **`--exact`** plans it instead, as `osmflow` reads it: it finds the minute diff each day and hour diff ends at,
  reading some state files around each place the plan changes streams, up to 3 at a time, and gives each leg's
  diffs, as `--seq 5164..5172`. A search reads one state file after another, as each depends on the last, so this
  takes some tens of requests, about half a minute on the planet's server. With `-f json`, the legs are in
  `catch_up`, with `first` and `last` when planned.
- **`--overlap`** is how late a change may show up in a diff, 3 minutes by default: how much before a planet file's
  time the diffs start, and how far around each change of streams `--exact` looks.
- **A time** finds the last diff at or before it. A diff's time is the newest change in it, so every change made after
  the time is in a later diff: reading from the next one misses none, and reads none made before.
- **A Geofabrik extract** on its own stream, and **a state file**, say exactly which diff they are up to.
- **A planet file** has only a time in its header, the newest change in it. A change can commit after the planet was
  taken and still be older than that, by up to `--overlap`, so the diffs to read start that much before its time, and
  the first of them may repeat changes it has.
- **`--since`** of `dump` and `stats` reads the same diffs as `--seq` with the number given. With `-f json`, `seq`
  gives them as `read_from` and `diffs`.
- **A time before the stream's oldest diff**, or a file whose diff the server no longer keeps, is an error.

## Logging

The log goes to standard error, through [`tracing`](https://docs.rs/tracing). `RUST_LOG` chooses what it shows, as
in `RUST_LOG=osmflow=debug` or `RUST_LOG=ureq=debug`, with warnings by default, such as a request that failed and is
tried again. Each `-v` adds osmflow's events of one more level to what `RUST_LOG` shows: `-v` each download, with its
size and how long it took; `-vv` each state file and diff read, each search for a time and each step of a plan;
`-vvv` each state a search reads.

```text
 INFO downloaded url="https://download.geofabrik.de/europe/monaco-updates/state.txt" bytes=113 took=669.637086ms
```

`RUST_LOG_FORMAT` chooses how the lines are written: `bare`, the default, gives the level and the message with its
fields; `compact` adds the time, the target and the spans, `full` writes them in full, `pretty` takes several lines for
each event, and `json` writes one JSON object for each.

## Threads

Both commands decode PBF files on a pool of threads, two fewer than the cores. `-j N` sets the pool's size; with `-j 1`,
blocks are decoded one after another on one thread. A dump writes objects in file order either way, so its output does
not depend on the number of threads. A small file keeps a large pool only partly busy, so a smaller one can take about
as long with less CPU time.

A replication stream's diffs are decoded each on a thread of its own, `-j N` of them ahead of the one being written;
an osmChange file is read on one thread.
