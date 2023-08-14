# osm-node-cache

[![GitHub](https://img.shields.io/badge/github-nyurik/osmnodecache-8da0cb?logo=github)](https://github.com/nyurik/osm-node-cache)
[![crates.io version](https://img.shields.io/crates/v/osmnodecache.svg)](https://crates.io/crates/osmnodecache)
[![docs.rs docs](https://docs.rs/osmnodecache/badge.svg)](https://docs.rs/osmnodecache)
[![crates.io version](https://img.shields.io/crates/l/osmnodecache.svg)](https://github.com/nyurik/osm-node-cache/blob/main/LICENSE-APACHE)
[![CI build](https://github.com/nyurik/osmnodecache/workflows/CI/badge.svg)](https://github.com/nyurik/osm-node-cache/actions)

Flat file node cache stores lat,lon coordinate pairs as `u64` values with their index being the position in the file. In
other words - 0th u64 value is stored as the first 8 bytes, etc.

The library allows multithreaded access to the cache, and can dynamically grow the cache file.

```rust,no_run
// This example uses osmpbf crate
use std::path::PathBuf;
use rayon::iter::{ParallelBridge, ParallelIterator};
use osmnodecache::{DenseFileCache, CacheStore as _};
use osmpbf::{BlobReader, BlobDecode};

fn main() {
  let reader = BlobReader::from_path("planet.osm.pbf").unwrap();
  let file_cache = DenseFileCache::new(PathBuf::from("node.cache")).unwrap();

  reader.par_bridge().for_each_with(
    file_cache,
    |fc, blob| {
      let mut cache = fc.get_accessor();
      if let BlobDecode::OsmData(block) = blob.unwrap().decode().unwrap() {
        for node in block.groups().flat_map(|g| g.dense_nodes()) {
          cache.set_lat_lon(node.id as usize, node.lat(), node.lon());
        }
      };
    });
}
```

## Development
* This project is easier to develop with [just](https://github.com/casey/just#readme), a modern alternative to `make`. Install it with `cargo install just`.
* To get a list of available commands, run `just`.
* To run tests, use `just test`.
* On `git push`, it will run a few validations, including `cargo fmt`, `cargo clippy`, and `cargo test`.  Use `git push --no-verify` to skip these checks.

## License

Licensed under either of

* Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or <http://www.apache.org/licenses/LICENSE-2.0>)
* MIT license ([LICENSE-MIT](LICENSE-MIT) or <http://opensource.org/licenses/MIT>)
  at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in the work by you, as defined in the
Apache-2.0 license, shall be dual licensed as above, without any
additional terms or conditions.
