# From `imposm-changes`

- **Source:** <https://github.com/omniscale/imposm-changes>, `test/build/`, commit
  `4ee44f4b63623e5247f48a8902e5e83a82a5b92a` (2020-06-19).
- **License:** Apache-2.0 (`LICENSE`, copied from the repository). The data is made up.

| File | What it tests |
|---|---|
| `testdata_import.pbf` | One block with dense nodes, ways and relations in three groups, written by a development build of Osmosis; timestamps before 1970 and after 2030, a relation member that is a relation. `../osc/imposm-changes/testdata_diff.osc` changes it |
