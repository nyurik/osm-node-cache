# From libosmium

- **Source:** <https://github.com/osmcode/libosmium>, `test/t/io/`, commit `5c9ba482d25c3d5157719c8cd8f2379a5cab0bf7`
  (2026-11-01).
- **License:** Boost Software License 1.0 (`LICENSE`, copied from the repository).

| File | What it tests |
|---|---|
| `data_pbf_version-1.osm` | The OSM XML the next two files encode |
| `data_pbf_version-1.osm.pbf` | A plain node whose version and changeset are -1, written by Osmosis |
| `data_pbf_version-1-densenodes.osm.pbf` | The same, as a dense node |
| `deleted_nodes.osh`, `deleted_nodes.osh.pbf` | A history file whose deleted nodes have no location |
| `empty_file.osm.pbf` | An empty file (renamed from `empty_file`, so the tests pick it up): an error, not an empty result |
