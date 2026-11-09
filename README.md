# osmflow

[![CI build status](https://github.com/nyurik/osmflow/actions/workflows/ci.yml/badge.svg)](https://github.com/nyurik/osmflow/actions)
[![Codecov](https://img.shields.io/codecov/c/github/nyurik/osmflow)](https://app.codecov.io/gh/nyurik/osmflow)

Rust crates for OpenStreetMap data.

| Crate                               | Description                                                                                                    |
|-------------------------------------|----------------------------------------------------------------------------------------------------------------|
| [`osmflow`](crates/osmflow)         | Fast, parallel reading of OpenStreetMap PBF files, and node location stores, from a city extract to the planet |
| [`osmflow-cli`](crates/osmflow-cli) | The `osmflow` command: dumps PBF files as text or GeoJSON, and summarizes them                                 |

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
