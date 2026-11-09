# From Baremaps

- **Source:** <https://github.com/apache/incubator-baremaps>, `baremaps-testing/data/osm-sample/`, commit
  `ecbc36295daed6c3e13cefb2204a31c4804efb31` (2026-09-08).
- **License:** Apache-2.0 (`LICENSE`, copied from the repository; `NOTICE` holds the part of the repository's notice
  that applies to these files). The data is made up.

A replication directory for `../../baremaps/sample.osm.pbf`.

| File | What it holds |
|---|---|
| `state.txt` | The state of the extract: sequence 1; no `#` comment and no final newline |
| `000/000/002.osc.gz`, `.state.txt` | Creates 27 nodes, 7 ways and 2 relations, in a `create` section per shape |
| `000/000/003.osc.gz`, `.state.txt` | Modifies every node, way and relation, without changing their versions |
| `000/000/004.osc.gz`, `.state.txt` | Deletes a node, two ways and two relations, whose deleted versions say `visible="true"` |

Each change file starts with an XML comment holding the Apache license header.
