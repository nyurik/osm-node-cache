# osm-node-cache

[![CI build status](https://github.com/nyurik/osm-node-cache/actions/workflows/ci.yml/badge.svg)](https://github.com/nyurik/osm-node-cache/actions)
[![Codecov](https://img.shields.io/codecov/c/github/nyurik/osm-node-cache)](https://app.codecov.io/gh/nyurik/osm-node-cache)

Rust crates for OpenStreetMap data.

| Crate                                 | Description                                                                                                   |
|---------------------------------------|---------------------------------------------------------------------------------------------------------------|
| [`osmnodecache`](crates/osmnodecache) | OSM node ID to location stores for importing and updating OpenStreetMap data, from a city to the planet       |
| [`osmflow`](crates/osmflow)           | Fast, parallel reading of OpenStreetMap PBF files, from a city extract to the planet                          |

## Development

* This project is easier to develop with [just](https://github.com/casey/just#readme), a modern alternative to `make`.
  Install it with `cargo install just`.
* To get a list of available commands, run `just`.
* To run tests, use `just test`.

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
