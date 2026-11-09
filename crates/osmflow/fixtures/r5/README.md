# From R5

- **Source:** <https://github.com/conveyal/r5>, `src/test/resources/`, commit
  `6f3554291d6eef41469610224b4f8a3ee36e7276` (2026-08-01).
- **License:** the code is MIT (`LICENSE`, copied from the repository). The data is © OpenStreetMap contributors,
  ODbL.

| File | What it tests |
|---|---|
| `felton.pbf` | A real extract written by osmconvert 0.8.10 without authors: versions and timestamps, but no changesets or users; a `timestamp=` optional feature and a replication timestamp |
| `snake-rd.pbf` | An Overpass API result written by Osmosis 0.44.1: dense nodes, ways and relations in one block, in three groups |
| `missing-nodes.pbf` | A way that refers to a node the file lacks, and a made-up way with a user ID of 0 and no user name |
