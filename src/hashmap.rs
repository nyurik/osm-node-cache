use std::fs::File;
use std::io::{BufReader, BufWriter};
use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use dashmap::DashMap;

use crate::{open_cache_file, Cache, CacheStore};

#[derive(Clone, Default)]
pub struct HashMapCache {
    data: Arc<DashMap<u64, u64>>,
}

fn open_for_read<P: AsRef<Path>>(filename: P) -> Result<BufReader<File>> {
    Ok(BufReader::new(File::open(filename)?))
}

fn open_for_write<P: AsRef<Path>>(filename: P) -> Result<BufWriter<File>> {
    Ok(BufWriter::new(open_cache_file(filename)?))
}

impl HashMapCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            data: Arc::new(DashMap::with_capacity(capacity)),
        }
    }

    pub fn from_json<P: AsRef<Path>>(filename: P) -> Result<Self> {
        Ok(Self {
            data: Arc::new(serde_json::from_reader(open_for_read(filename)?)?),
        })
    }

    pub fn from_bin<P: AsRef<Path>>(filename: P) -> Result<Self> {
        Ok(Self {
            data: Arc::new(bincode::deserialize_from(open_for_read(filename)?)?),
        })
    }

    pub fn save_as_json<P: AsRef<Path>>(&self, filename: P) -> Result<()> {
        Ok(serde_json::to_writer(
            open_for_write(filename)?,
            self.data.as_ref(),
        )?)
    }

    pub fn save_as_pretty_json<P: AsRef<Path>>(&self, filename: P) -> Result<()> {
        Ok(serde_json::to_writer_pretty(
            open_for_write(filename)?,
            self.data.as_ref(),
        )?)
    }

    pub fn save_as_bin<P: AsRef<Path>>(&self, filename: P) -> Result<()> {
        Ok(bincode::serialize_into(
            open_for_write(filename)?,
            self.data.as_ref(),
        )?)
    }
}

impl CacheStore for HashMapCache {
    fn get_accessor(&self) -> Box<dyn Cache + '_> {
        Box::new(self.clone())
    }
}

impl Cache for HashMapCache {
    fn get(&self, index: usize) -> u64 {
        self.data.get(&(index as u64)).map_or(0_u64, |v| *v.value())
    }

    fn set(&mut self, index: usize, value: u64) {
        self.data.insert(index as u64, value);
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use rayon::iter::ParallelBridge;
    use rayon::iter::ParallelIterator;

    use crate::hashmap::HashMapCache;
    use crate::tests::get_random_items;
    use crate::*;

    #[test]
    fn hashmap_test() {
        let threads = 10;
        let items = 100000;
        let cache = HashMapCache::new();
        (0_usize..threads)
            .par_bridge()
            .for_each_with(cache.clone(), |c, _thread_id| {
                for v in get_random_items(items) {
                    c.set(v, v as u64);
                }
            });
        (0_usize..threads)
            .par_bridge()
            .for_each_with(cache, |c, _thread_id| {
                for v in get_random_items(items) {
                    assert_eq!(v as u64, c.get(v))
                }
            });
    }

    #[test]
    fn hashmap_file_json_pretty_test() {
        let items = 100000;
        let filename = Path::new("./hashmap_test.pretty.json");
        let cache = new_hashmap(items);
        let _ = fs::remove_file(filename);
        cache.save_as_pretty_json(filename).unwrap();
        test_values(&HashMapCache::from_json(filename).unwrap(), items);
        cleanup_test_file(filename);
    }

    #[test]
    fn hashmap_file_json_test() {
        let items = 100000;
        let filename = Path::new("./hashmap_test.json");
        let cache = new_hashmap(items);
        let _ = fs::remove_file(filename);
        cache.save_as_json(filename).unwrap();
        test_values(&HashMapCache::from_json(filename).unwrap(), items);
        cleanup_test_file(filename);
    }

    #[test]
    fn hashmap_file_bin_test() {
        let items = 100000;
        let filename = Path::new("./hashmap_test.bin");
        let cache = new_hashmap(items);
        let _ = fs::remove_file(filename);
        cache.save_as_bin(filename).unwrap();
        test_values(&HashMapCache::from_bin(filename).unwrap(), items);
        cleanup_test_file(filename);
    }

    fn test_values(c: &dyn Cache, items: usize) {
        for v in 0..items {
            assert_eq!(v as u64, c.get(v))
        }
    }

    fn new_hashmap(items: usize) -> HashMapCache {
        let mut cache = HashMapCache::with_capacity(items);
        for v in 0..items {
            cache.set(v, v as u64);
        }
        cache
    }

    fn cleanup_test_file(filename: &Path) {
        if !cfg!(feature = "keeptestfiles") {
            let _ = fs::remove_file(filename);
        }
    }
}
