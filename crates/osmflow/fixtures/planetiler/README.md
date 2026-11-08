# From Planetiler

- **Source:** <https://github.com/onthegomap/planetiler>, `planetiler-core/src/test/resources/`, commit
  `21f3d5d03bcf1e4b11704e9be699aa8676c0ff22` (2026-11-06).
- **License:** the code is Apache-2.0 (`LICENSE`, copied from the repository). The data is © OpenStreetMap
  contributors, ODbL: Geofabrik's extract of Monaco.

| File | What it tests |
|---|---|
| `monaco-latest.osm.pbf` | A real extract with replication fields in its header (Planetiler checks 25,423 nodes, 4,106 ways and 243 relations) |
| `monaco-latest-without-changesets.osm.pbf` | The same without user names and changesets |
| `monaco-latest.lz4.osm.pbf` | The same with lz4-compressed blobs, which osmflow rejects with an error naming lz4 |
