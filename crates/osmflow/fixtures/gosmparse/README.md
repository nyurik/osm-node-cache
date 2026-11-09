# From `gosmparse`

- **Source:** <https://github.com/thomersch/gosmparse>, `testdata/`, commit
  `2dcc2c4569b005e5dd68333fc6c528c16f1c732c` (2021-08-18).
- **License:** MIT (`LICENSE`, copied from the repository). The data is made up.

Small files written by several programs: osmconvert, Osmosis (whose writing program is just its version, `0.44.1`)
and osmium.

| File | What it tests |
|---|---|
| `base.pbf` | A node, a way and a relation, one per block, written by osmconvert 0.8.4; the relation has no user |
| `bad_header.pbf` | `base.pbf` with its first blob type changed to `NSMHeader`: an error, as gosmparse expects |
| `history.osh.pbf` | A history file with two versions of nodes, ways and relations, some deleted; osmium gives a deleted node the undefined location (2³¹ − 1) |
| `node_kv.osm.pbf` | Tagged dense nodes, written by osmconvert 0.7T |
| `relation.pbf` | A block holding only relations, one of them without metadata, and no nodes or ways |
| `relation_kv.osm.pbf`, `way_kv.osm.pbf` | Nodes, ways and relations in one block, each type in its own group, as Osmosis writes them |
| `stringtable.pbf` | Ways whose tags repeat strings and hold non-ASCII text, written by osmium 1.6 |
