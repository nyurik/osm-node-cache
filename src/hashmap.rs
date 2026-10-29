use std::fs::File;
use std::io::{BufReader, BufWriter};
use std::path::Path;
use std::sync::Arc;

use dashmap::DashMap;

use crate::traits::{open_cache_file, Cache, CacheStore};
use crate::OsmNodeCacheResult;

#[derive(Clone, Default)]
pub struct HashMapCache {
    data: Arc<DashMap<u64, u64>>,
}

fn open_for_read<P: AsRef<Path>>(filename: P) -> OsmNodeCacheResult<BufReader<File>> {
    Ok(BufReader::new(File::open(filename)?))
}

fn open_for_write<P: AsRef<Path>>(filename: P) -> OsmNodeCacheResult<BufWriter<File>> {
    Ok(BufWriter::new(open_cache_file(
        filename.as_ref(),
        true,
        true,
    )?))
}

impl HashMapCache {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            data: Arc::new(DashMap::with_capacity(capacity)),
        }
    }

    pub fn from_json<P: AsRef<Path>>(filename: P) -> OsmNodeCacheResult<Self> {
        Ok(Self {
            data: Arc::new(serde_json::from_reader(open_for_read(filename)?)?),
        })
    }

    pub fn from_bin<P: AsRef<Path>>(filename: P) -> OsmNodeCacheResult<Self> {
        Ok(Self {
            data: Arc::new(bincode_next::serde::decode_from_std_read(
                &mut open_for_read(filename)?,
                bincode_next::config::legacy(),
            )?),
        })
    }

    pub fn save_as_json<P: AsRef<Path>>(&self, filename: P) -> OsmNodeCacheResult<()> {
        Ok(serde_json::to_writer(
            open_for_write(filename)?,
            self.data.as_ref(),
        )?)
    }

    pub fn save_as_pretty_json<P: AsRef<Path>>(&self, filename: P) -> OsmNodeCacheResult<()> {
        Ok(serde_json::to_writer_pretty(
            open_for_write(filename)?,
            self.data.as_ref(),
        )?)
    }

    pub fn save_as_bin<P: AsRef<Path>>(&self, filename: P) -> OsmNodeCacheResult<usize> {
        Ok(bincode_next::serde::encode_into_std_write(
            self.data.as_ref(),
            &mut open_for_write(filename)?,
            bincode_next::config::legacy(),
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
    use std::path::Path;

    use crate::hashmap::HashMapCache;
    use crate::traits::tests::{check_parallel, TestFile};
    use crate::traits::Cache;
    use crate::OsmNodeCacheResult;

    type Save = fn(&HashMapCache, &Path) -> OsmNodeCacheResult<()>;
    type Load = fn(&Path) -> OsmNodeCacheResult<HashMapCache>;

    /// A cache of `0..items`, each value its own key
    fn filled(items: usize) -> HashMapCache {
        let mut cache = HashMapCache::with_capacity(items);
        for v in 0..items {
            cache.set(v, v as u64);
        }
        cache
    }

    #[test]
    fn hashmap() {
        check_parallel(&HashMapCache::new());
    }

    #[test]
    fn clone_shares_data() {
        let mut cache = HashMapCache::new();
        let mut clone = cache.clone();
        cache.set(11, 42);
        clone.set(12, 43);
        assert_eq!(clone.get(11), 42);
        assert_eq!(cache.get(12), 43);
    }

    #[test]
    fn saves_and_loads_files() {
        let formats: [(&str, Save, Load); 3] = [
            (
                "json",
                |c, p| c.save_as_json(p),
                |p| HashMapCache::from_json(p),
            ),
            (
                "pretty.json",
                |c, p| c.save_as_pretty_json(p),
                |p| HashMapCache::from_json(p),
            ),
            (
                "bin",
                |c, p| c.save_as_bin(p).map(drop),
                |p| HashMapCache::from_bin(p),
            ),
        ];
        for (ext, save, load) in formats {
            let f = TestFile::new(&format!("hashmap.{ext}"));
            for items in [100_000, 10] {
                // the second, smaller save replaces all of the first
                save(&filled(items), &f.0).unwrap();
                let cache = load(&f.0).unwrap();
                assert!((0..items).all(|v| cache.get(v) == v as u64), "{ext}");
                assert_eq!(cache.get(items), 0, "{ext}");
            }
        }
    }

    #[test]
    fn bin_is_bincode_legacy() {
        let f = TestFile::new("hashmap.legacy.bin");
        let bytes = [1_u64, 42, 7].map(u64::to_le_bytes).concat();
        let mut cache = HashMapCache::with_capacity(1);
        cache.set(42, 7);
        cache.save_as_bin(&f.0).unwrap();
        assert_eq!(fs::read(&f.0).unwrap(), bytes);
        fs::write(&f.0, &bytes).unwrap();
        assert_eq!(HashMapCache::from_bin(&f.0).unwrap().get(42), 7);
    }
}
