# From `osmpbf`

- **Source:** <https://github.com/b-r-u/osmpbf>, `tests/`, commit `4bc5ce41eedbbeb263c2fb7091dc48d28134df56`
  (2025-10-24).
- **License:** MIT or Apache-2.0, at your option (`LICENSE-MIT`, `LICENSE-APACHE`, copied from the repository).

| File | What it tests |
|---|---|
| `test.osm` | The OSM XML the next three files encode |
| `test.osm.pbf` | Dense nodes, ways, relations, zlib-compressed blobs |
| `test_nozlib.osm.pbf` | The same data in uncompressed (raw) blobs |
| `test_nozlib_nodense.osm.pbf` | The same data with plain (not dense) nodes |
| `loc_on_ways.osm.pbf` | Ways carrying their nodes' locations (`LocationsOnWays`) |
| `deleted_nodes.osh.pbf` | A history file with a deleted (invisible) node; the same bytes as libosmium's copy |
