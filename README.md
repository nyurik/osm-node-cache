# osmflow

[![CI build status](https://github.com/nyurik/osmflow/actions/workflows/ci.yml/badge.svg)](https://github.com/nyurik/osmflow/actions)
[![Codecov](https://img.shields.io/codecov/c/github/nyurik/osmflow)](https://app.codecov.io/gh/nyurik/osmflow)

Rust crates for OpenStreetMap data.

| Crate                                 | Status                                                                                                                                                                                                                                                                                                                                                      | Description                                                                                                    |
|---------------------------------------|-------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|----------------------------------------------------------------------------------------------------------------|
| [`osmflow`](crates/osmflow)           | [![crates.io](https://img.shields.io/crates/v/osmflow)](https://crates.io/crates/osmflow)<br>[![docs.rs](https://img.shields.io/docsrs/osmflow)](https://docs.rs/osmflow)<br>[![coverage](https://img.shields.io/codecov/c/github/nyurik/osmflow?component=osmflow&label=coverage)](https://app.codecov.io/gh/nyurik/osmflow/components)                          | Fast, parallel reading of OpenStreetMap PBF files, and node location stores, from a city extract to the planet |
| [`osmflow-sync`](crates/osmflow-sync) | [![crates.io](https://img.shields.io/crates/v/osmflow-sync)](https://crates.io/crates/osmflow-sync)<br>[![docs.rs](https://img.shields.io/docsrs/osmflow-sync)](https://docs.rs/osmflow-sync)<br>[![coverage](https://img.shields.io/codecov/c/github/nyurik/osmflow?component=osmflow-sync&label=coverage)](https://app.codecov.io/gh/nyurik/osmflow/components) | OpenStreetMap data in PostgreSQL with PostGIS, kept current from replication diffs                             |
| [`osmflow-cli`](crates/osmflow-cli)   | [![crates.io](https://img.shields.io/crates/v/osmflow-cli)](https://crates.io/crates/osmflow-cli)<br>[![coverage](https://img.shields.io/codecov/c/github/nyurik/osmflow?component=osmflow-cli&label=coverage)](https://app.codecov.io/gh/nyurik/osmflow/components)                                                                                           | The `osmflow` command: dumps PBF files as text or GeoJSON, summarizes them, and keeps them in PostgreSQL       |

## Development

* This project is easier to develop with [just](https://github.com/casey/just#readme), a modern alternative to `make`.
  Install it with `cargo install just`.
* To get a list of available commands, run `just`.
* To run tests, use `just test`.
* [`AGENTS.md`](AGENTS.md) describes the layout of the repository, and the rules for changing it, such as which other
  projects' code may be read.

## License

Licensed under either of

* Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or <https://www.apache.org/licenses/LICENSE-2.0>)
* MIT license ([LICENSE-MIT](LICENSE-MIT) or <https://opensource.org/licenses/MIT>)
  at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in the work by you, as defined in the
Apache-2.0 license, shall be dual-licensed as above, without any
additional terms or conditions.
