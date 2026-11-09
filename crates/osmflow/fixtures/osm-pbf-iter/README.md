# From `osm_pbf_iter`

- **Source:** <https://github.com/astro/rust-osm-pbf-iter>, `tests/data/`, commit
  `75afbab854ff5749271a939630fd3f8d81a88aa9` (2026-10-31).
- **License:** the code is MIT (`LICENSE`, copied from the repository). The repository says the data comes from
  [osm-testdata](https://github.com/osmcode/osm-testdata), released into the public domain, converted to PBF with
  `osmium cat`; see `UPSTREAM-README.md`, the repository's own note. But `64bit_ids` holds OpenStreetMap data: real
  objects in Antigua, with the names of the users who edited them. It is © OpenStreetMap contributors, available
  under the [Open Database License](https://opendatacommons.org/licenses/odbl/).

| File | What it tests |
|---|---|
| `64bit_ids.osm.pbf` (`.xml`) | IDs beyond 32 bits |
| `tag_lengths.osm.pbf` (`.xml`) | Long tag keys and values |
| `two_primitive_groups.osm.pbf` (`.xml`) | A block with two groups |
| `multipolygon.osm.pbf` (`.xml`) | A small multipolygon relation, simplified upstream |
| `non_default_date_granularity.osm.pbf` | A block whose `date_granularity` is not the default 1000 ms |
