# osmflow-cli

The `osmflow` command, built on the [`osmflow`](https://crates.io/crates/osmflow) crate: it dumps the objects of
OpenStreetMap PBF files as text or GeoJSON, and summarizes files.

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
timestamp, changeset, user ID and user name.

## `osmflow stats`

```sh
osmflow stats dc.osm.pbf
osmflow stats planet.osm.pbf --json
```

```text
file          district-of-columbia.osm.pbf (21.0 MB)
written by    osmium/1.16.0
features      OsmSchema-V0.6, DenseNodes
optional      Sort.Type_then_ID
header bbox   lon -77.1201 .. -76.90906, lat 38.790988 .. 38.99603
replication   sequence 4954 at 2026-10-27T21:23:59Z
blocks        282 (282 zlib): 21.0 MB in blobs, 49.0 MB uncompressed; 346,158 strings in 3.2 MB
groups        245 dense nodes, 36 ways, 1 relations
                        count          tagged            tags   IDs
nodes               1,958,977         110,044         400,171   281072 .. 14324055101
ways                  285,566         281,052       1,735,321   4681186 .. 1572294636
relations               5,284           5,284          30,068   62152 .. 21811917
way refs            2,507,044
members               299,614
node extent   lon -77.1926236 .. -76.8316112, lat 38.569216 .. 39.2377262
timestamps    2008-02-08T05:29:51Z .. 2026-10-27T21:19:17Z; highest version 1051
order         by type, then ID (the header says sorted)
```

The order line checks every object, whatever the header claims.

## Threads

Both commands decode on a pool of threads, two fewer than the cores. `-j N` sets the pool's size; with `-j 1`, blocks
are decoded one after another on one thread. A dump writes objects in file order either way, so its output does not
depend on the number of threads.
