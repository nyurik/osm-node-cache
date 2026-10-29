//! [`KvBackend`] in redb, a copy-on-write B-tree database in one file.

use std::fs;
use std::path::{Path, PathBuf};

use ::redb::{Database, Durability, ReadOnlyTable, ReadableDatabase as _, TableDefinition};

use super::{store_err, KvBackend, KvReader};
use crate::OsmNodeCacheResult;

const TABLE: TableDefinition<u64, &[u8]> = TableDefinition::new("nodes");

/// Options for [`RedbBackend`].
#[derive(Clone, Debug, Default)]
pub struct RedbOpts {
    cache_bytes: Option<usize>,
}

impl RedbOpts {
    /// The memory redb may use to cache pages it read; redb's default is 1 GiB.
    #[must_use]
    pub fn cache_bytes(mut self, bytes: usize) -> Self {
        self.cache_bytes = Some(bytes);
        self
    }

    fn builder(&self) -> ::redb::Builder {
        let mut b = Database::builder();
        if let Some(bytes) = self.cache_bytes {
            b.set_cache_size(bytes);
        }
        b
    }
}

pub struct RedbBackend {
    db: Database,
    path: PathBuf,
}

pub struct RedbReader(ReadOnlyTable<u64, &'static [u8]>);

impl KvReader for RedbReader {
    fn get(&mut self, key: u64, out: &mut Vec<u8>) -> bool {
        let Some(v) = self.0.get(key).expect("redb read failed") else {
            return false;
        };
        out.clear();
        out.extend_from_slice(v.value());
        true
    }
}

impl KvBackend for RedbBackend {
    type Opts = RedbOpts;
    type Reader<'a> = RedbReader;

    fn create(path: &Path, opts: &RedbOpts) -> OsmNodeCacheResult<Self> {
        if path.exists() {
            fs::remove_file(path)?;
        }
        let db = opts.builder().create(path).map_err(store_err)?;
        let txn = db.begin_write().map_err(store_err)?;
        txn.open_table(TABLE).map_err(store_err)?;
        txn.commit().map_err(store_err)?;
        Ok(Self {
            db,
            path: path.to_path_buf(),
        })
    }

    fn open(path: &Path, opts: &RedbOpts) -> OsmNodeCacheResult<Self> {
        let db = opts.builder().open(path).map_err(store_err)?;
        Ok(Self {
            db,
            path: path.to_path_buf(),
        })
    }

    fn reader(&self) -> RedbReader {
        let txn = self.db.begin_read().expect("redb read failed");
        RedbReader(txn.open_table(TABLE).expect("redb table missing"))
    }

    fn commit(&mut self, values: &[(u64, Option<&[u8]>)], durable: bool) -> OsmNodeCacheResult<()> {
        let mut txn = self.db.begin_write().map_err(store_err)?;
        if !durable {
            txn.set_durability(Durability::None).map_err(store_err)?;
        }
        {
            let mut table = txn.open_table(TABLE).map_err(store_err)?;
            for &(key, value) in values {
                match value {
                    Some(v) => table.insert(key, v).map(drop),
                    None => table.remove(key).map(drop),
                }
                .map_err(store_err)?;
            }
        }
        txn.commit().map_err(store_err)
    }

    fn disk_bytes(&self) -> OsmNodeCacheResult<u64> {
        Ok(fs::metadata(&self.path)?.len())
    }
}
