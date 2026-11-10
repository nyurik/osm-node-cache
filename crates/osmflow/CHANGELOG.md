# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.12.1](https://github.com/nyurik/osmflow/compare/osmflow-v0.12.0...osmflow-v0.12.1) - 2026-11-10

### Other

- reduce dups ([#52](https://github.com/nyurik/osmflow/pull/52))

## [0.12.0](https://github.com/nyurik/osmflow/compare/osmflow-v0.11.0...osmflow-v0.12.0) - 2026-11-10

### Added

- [**breaking**] osmChange support, sequence search, catch-up plans, `osmflow seq` ([#48](https://github.com/nyurik/osmflow/pull/48))

### Other

- changelog per crate, CLI tool is the only one on GitHub release page ([#47](https://github.com/nyurik/osmflow/pull/47))

## [0.11.0](https://github.com/nyurik/osmflow/tree/osmflow-v0.11.0) - 2026-11-09

The first release of osmflow. The repository was renamed from `osm-node-cache` to `osmflow`: its
[`osmnodecache`](https://crates.io/crates/osmnodecache) crate, last released as 0.10.2, is deprecated, and its code
lives on, rewritten, as osmflow's node location stores (`osmflow::nodes`, feature `nodes`). `DenseStore` takes the place
of `DenseFileCache`, with `PagedStore`, about 5 bytes per node, and `MemoryStore` beside it, and osmflow reads the PBF
file itself:

```rust
use osmflow::PbfReader;
use osmflow::nodes::DenseStore;
use osmflow::nodes::prelude::*;

fn cache_nodes() -> Result<(), Box<dyn std::error::Error>> {
    // before: DenseFileCache::new(PathBuf::from("node.cache")), filled with set_lat_lon from osmpbf's blocks
    let mut store = DenseStore::open("node.cache")?;
    PbfReader::open("planet.osm.pbf")?.load_nodes(&mut store)?;

    let mut reader = store.reader();
    if let Some(location) = reader.get(240_109_189) {
        println!("{}, {}", location.lat_degrees(), location.lon_degrees());
    }
    Ok(())
}
```

### Added

- `PbfReader` reads PBF files on a pool of threads (`blocks`, `par_ordered`, `par_unordered`), skipping blocks of
  types left out of `Select` and blocks whose strings can't hold a wanted object (`nodes_where`, `ways_where`,
  `relations_where`).
- `load_nodes`, `par_resolve` and `Locator` build ways' line strings and polygons; `AreaRules` decide which closed
  ways are areas (feature `geometry`).
- `OscReader` reads osmChange files, and `Replication` a replication directory, downloading it with feature `http`
  (feature `osc`).
- `PagedStore`, `DenseStore` and `MemoryStore` keep node locations (feature `nodes`).
