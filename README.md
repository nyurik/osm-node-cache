# osm-node-cache

[![Build](https://github.com/nyurik/osm-node-cache/actions/workflows/ci.yaml/badge.svg)](https://github.com/nyurik/osm-node-cache/actions/workflows/ci.yaml)
[![Crates.io](https://img.shields.io/crates/v/osmnodecache.svg)](https://crates.io/crates/osmnodecache)
[![Documentation](https://docs.rs/osmnodecache/badge.svg)](https://docs.rs/osmnodecache)

Flat file node cache stores lat,lon coordinate pairs as `u64` values with their index being the position in the file. In
other words - 0th u64 value is stored as the first 8 bytes, etc.

The library allows multithreaded access to the cache, and can dynamically grow the cache file.

```rust
// Libraries:  osmpbf, rayon
fn main() {
    let reader = BlobReader::from_path("planet.osm.pbf").unwrap();
    let file_cache = DenseFileCache::new("node.cache".to_string())?;

    reader.par_bridge().for_each_with(
        file_cache,
        |fc, blob| {
            let mut cache = fc.get_accessor();
            if let BlobDecode::OsmData(block) = blob.unwrap().decode().unwrap() {
                for node in block.groups().flat_map(|g| g.dense_nodes()) {
                    cache.set_value_i32(node.id as usize, node.decimicro_lat(), node.decimicro_lon());
                }
            };
        });
}
```

## License

You may use this library under MIT or Apache 2.0 license
