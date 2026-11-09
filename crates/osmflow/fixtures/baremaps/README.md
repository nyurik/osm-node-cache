# From Baremaps

- **Source:** <https://github.com/apache/incubator-baremaps>, `baremaps-testing/data/osm-sample/`, commit
  `ecbc36295daed6c3e13cefb2204a31c4804efb31` (2026-09-08).
- **License:** Apache-2.0 (`LICENSE`, copied from the repository; `NOTICE` holds the part of the repository's notice
  that applies to these files). The data is made up.

| File | What it tests |
|---|---|
| `sample.osm.pbf` | 36 objects in 16 blocks of one group each, written by osmium 1.16 in file order, not sorted: nodes, a way, nodes, a way, a relation, and so on. `../osc/baremaps/` holds three diffs that change it |
