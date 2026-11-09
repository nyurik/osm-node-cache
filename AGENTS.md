# Working on osmflow

Rust crates for OpenStreetMap data, in one Cargo workspace. These notes are for anyone changing the code, people and
coding agents alike. How the library works is in [`crates/osmflow/docs/design.md`](crates/osmflow/docs/design.md).

## Layout

| Path | Holds |
|---|---|
| `crates/osmflow` | The library: reading PBF files, node location stores, way geometries, osmChange files and replication |
| `crates/osmflow/src/blob.rs`, `peek.rs`, `reader.rs` | Reading blobs from the file, peeking at a block's type and strings, and handing out blocks (`blocks`, `par_ordered`, `par_unordered`) |
| `crates/osmflow/src/block.rs`, `packed.rs`, `filter.rs`, `header.rs` | Decoding blocks into columns, packed arrays, block filters, the file header |
| `crates/osmflow/src/resolve.rs`, `area.rs`, `model.rs` | Loading nodes into a store and building ways' geometries; which closed ways are areas; types shared by every part |
| `crates/osmflow/src/nodes/` | The node location stores, usable without the reader |
| `crates/osmflow/src/osc.rs`, `osc/` | osmChange files, replication streams and downloads |
| `crates/osmflow/src/generated/` | Code generated from `proto/`, checked in; `just update-generated` rewrites it and needs `protoc` |
| `crates/osmflow/docs/` | How the library works: `design.md`, `pbf.md`, `osc.md`, `nodes.md` |
| `crates/osmflow/fixtures/` | Test files from other projects, one directory per source with a README and its license; `generated/` is written by `tests/generated.rs` (`OSMFLOW_REGENERATE=1` rewrites it) |
| `crates/osmflow/tests/`, `benches/` | Integration tests, with snapshots in `tests/snapshots/`; benchmarks of osmChange parsing and the node stores |
| `crates/osmflow-cli` | The `osmflow` command: `dump` and `stats` |
| `benches/` | One package per PBF reader, to compare osmflow with other crates; see `benches/README.md` |
| `.github/workflows/ci.yml` | Tests, MSRV, coverage, release-plz, and the release binaries |
| `tmp/` | Ignored by git: scratch files and measurements |

## Licenses of other projects

The crates are MIT OR Apache-2.0, so what they take from other projects must allow that.

- **Never read, copy or adapt code under the GPL, LGPL or AGPL**, such as osm2pgsql or osmium-tool, nor code under the
  Business Source License. Their documentation may be read to learn what they do.
- **Permissive code may be read and adapted:** MIT, Apache-2.0, BSD, ISC, zlib, and the Boost Software License
  (BSL-1.0, which libosmium uses). Keep the original copyright notice with anything copied.
- Check a project's license before opening its source.
- Test files come only from projects under such licenses, or from the OpenStreetMap and Geofabrik servers (data under
  the ODbL). Each source gets a directory in `fixtures/` with a README saying where the files came from, at which
  commit, under which license, and what they test, and with the project's license file.

## Writing

- Docs and comments describe the code as it is: no plans, roadmaps, development history or release status.
- README examples are doctests (`cargo test --doc` runs them): no hidden `#` lines, and the code goes in a named
  function rather than `main`.
- Public items carry docs that say more than their names: units, ranges, when an error happens, what to do about it.
- Test cases written for one feature or error each go inline in the tests; only real data from elsewhere goes in
  `fixtures/`.

## Code

- Errors are named for what they cover (`PbfError`, `OscError`, `NodeStoreError`), with detail enums for the rest
  (`PbfFormatError`, `XmlError`, `StateError`, `GeometryError`). There are no public `Result` aliases: public
  signatures name their error.
- Unsafe code is denied in the workspace. Only `nodes::file::map_file` and `nodes::codec::zeroed` opt in, each with a
  `// SAFETY:` comment.
- Paths are at most two segments (`fs::write`, `io::Error`, or an imported type); code behind a feature flag imports
  what it uses inside the function.
- Performance claims come from measurements with the repository's own tools, and say where they were taken.

## Checks

Use the `just` recipes (`just` lists them):

- `just fmt`, then `just clippy`, which lints with all features and with each of osmflow's features alone
- `just test` and `just test-doc`
- `just ci-test` runs what CI runs
- `just run …`, `just dump …` and `just stats …` run the `osmflow` command

Commit messages use conventional prefixes: `feat`, `fix`, `docs`, `refactor`, `test`, `chore`.
