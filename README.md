#osm-node-cache

Flat file node cache stores lat,lon coordinate pairs as `u64` values with their index being the position in the file.  In other words - 0th u64 value is stored as the first 8 bytes, etc.

The library allows multithreaded access to the cache, and can dynamically grow the cache file.

```rust
// Libraries:  osmpbf, rayon
fn main() {
    let reader = BlobReader::from_path("planet.osm.pbf").unwrap();
    let file_cache = DenseFileCache::new("flat_index.data".to_string(), None)?;

    reader.par_bridge()
        .for_each_with(
            file_cache,
            |dfc: &mut DenseFileCache, blob: Result<Blob, osmpbf::Error>| {
                let mut cache = dfc.get_accessor();
                match blob.unwrap().decode().unwrap() {
                    BlobDecode::OsmHeader(header) => {}
                    BlobDecode::OsmData(block) => {
                        for node in block.groups().flat_map(|g| g.dense_nodes()) {
                            cache.set_value_i32(node.id as usize, node.decimicro_lat(), node.decimicro_lon());
                        }
                    }
                    BlobDecode::Unknown(unk) => {}
                };
            });
}
```

## License
You may use this project under MIT or Apache 2.0 license
