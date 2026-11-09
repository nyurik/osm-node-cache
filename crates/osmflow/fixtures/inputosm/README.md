# From `inputosm`

- **Source:** <https://github.com/stefankarschti/inputosm>, `test/unit/data/`, commit
  `d93762b63398afee948ec0e1ca4e4328bba25177` (2026-10-11).
- **License:** Apache-2.0 (`LICENSE`, copied from the repository). The data is made up, written with osmium from
  generated XML.

| File | What it tests |
|---|---|
| `comprehensive.osm.pbf` | 17,005 dense nodes in several blocks, IDs beyond 32 bits, negative coordinate and reference deltas, 270 tags on one node, empty and Unicode values, a way with every node |
| `ordinary.osm.pbf` | The same kinds of objects, fewer of them, as plain (not dense) nodes in raw (uncompressed) blobs, with no `DenseNodes` required feature |
