//! [`KvBackend`] in redb, a copy-on-write B-tree database in one file.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use ::redb::{
    Database, DatabaseError, Durability, ReadOnlyDatabase, ReadOnlyTable, ReadableDatabase,
    TableDefinition, TableError,
};

use super::{backend_err, KvBackend, KvStore};
use crate::file::At as _;
use crate::{Error, ReadWrite, Result};

const TABLE: TableDefinition<u64, &[u8]> = TableDefinition::new("nodes");
/// The first bytes of a redb file
const REDB_MAGIC: &[u8; 4] = b"redb";

/// Node locations in a redb database: slower lookups than [`crate::PagedStore`], and commits that
/// a power loss cannot undo, through a well-tested database.
pub type RedbStore<M = ReadWrite> = KvStore<RedbBackend, M>;

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

enum Db {
    ReadWrite(Database),
    ReadOnly(ReadOnlyDatabase),
}

/// A redb database. It keeps a read transaction of the last commit open, so lookups need none of
/// their own. A commit closes it, and the next lookup opens it again.
pub struct RedbBackend {
    db: Db,
    table: OnceLock<ReadOnlyTable<u64, &'static [u8]>>,
    path: PathBuf,
}

impl RedbBackend {
    fn table(&self) -> &ReadOnlyTable<u64, &'static [u8]> {
        self.table.get_or_init(|| {
            Self::read_table(&self.db, &self.path).expect("cannot read the database")
        })
    }

    /// Opens the table of the last commit, for reading.
    fn read_table(db: &Db, path: &Path) -> Result<ReadOnlyTable<u64, &'static [u8]>> {
        let txn = match db {
            Db::ReadWrite(db) => db.begin_read(),
            Db::ReadOnly(db) => db.begin_read(),
        }
        .map_err(backend_err)?;
        txn.open_table(TABLE).map_err(|e| match e {
            TableError::TableDoesNotExist(_) | TableError::TableTypeMismatch { .. } => {
                Error::NotAStore(path.to_path_buf())
            }
            e => backend_err(e),
        })
    }

    /// The error for a database that cannot be opened.
    fn open_err(e: DatabaseError, path: &Path) -> Error {
        match e {
            DatabaseError::DatabaseAlreadyOpen => Error::Locked(path.to_path_buf()),
            // a file that is not redb's
            _ if fs::read(path).is_ok_and(|b| !b.starts_with(REDB_MAGIC)) => {
                Error::NotAStore(path.to_path_buf())
            }
            e => backend_err(e),
        }
    }
}

impl KvBackend for RedbBackend {
    type Opts = RedbOpts;

    fn open(path: &Path, opts: &RedbOpts) -> Result<(Self, bool)> {
        let created = fs::metadata(path).map_or(true, |m| m.len() == 0);
        let db = if created {
            let db = opts
                .builder()
                .create(path)
                .map_err(|e| Self::open_err(e, path))?;
            let txn = db.begin_write().map_err(backend_err)?;
            txn.open_table(TABLE).map_err(backend_err)?;
            txn.commit().map_err(backend_err)?;
            db
        } else {
            opts.builder()
                .open(path)
                .map_err(|e| Self::open_err(e, path))?
        };
        let db = Db::ReadWrite(db);
        let table = OnceLock::from(Self::read_table(&db, path)?);
        let path = path.to_path_buf();
        Ok((Self { db, table, path }, created))
    }

    fn open_read_only(path: &Path, opts: &RedbOpts) -> Result<Self> {
        if !path.exists() {
            return Err(Error::NotFound(path.to_path_buf()));
        }
        let db = opts
            .builder()
            .open_read_only(path)
            .map_err(|e| Self::open_err(e, path))?;
        let db = Db::ReadOnly(db);
        let table = OnceLock::from(Self::read_table(&db, path)?);
        let path = path.to_path_buf();
        Ok(Self { db, table, path })
    }

    fn get(&self, key: u64, out: &mut Vec<u8>) -> Result<bool> {
        let Some(v) = self.table().get(key).map_err(backend_err)? else {
            return Ok(false);
        };
        out.clear();
        out.extend_from_slice(v.value());
        Ok(true)
    }

    fn for_each(&self, below: u64, f: &mut dyn FnMut(u64, &[u8])) -> Result<()> {
        for entry in self.table().range(..below).map_err(backend_err)? {
            let (k, v) = entry.map_err(backend_err)?;
            f(k.value(), v.value());
        }
        Ok(())
    }

    fn has_keys_below(&self, below: u64) -> Result<bool> {
        Ok(self
            .table()
            .range(..below)
            .map_err(backend_err)?
            .next()
            .is_some())
    }

    fn commit(&mut self, values: &[(u64, Option<&[u8]>)], durable: bool) -> Result<()> {
        let Db::ReadWrite(db) = &self.db else {
            unreachable!("a read-only store has no commit");
        };
        self.table = OnceLock::new();
        let mut txn = db.begin_write().map_err(backend_err)?;
        if !durable {
            txn.set_durability(Durability::None).map_err(backend_err)?;
        }
        {
            let mut table = txn.open_table(TABLE).map_err(backend_err)?;
            for &(key, value) in values {
                match value {
                    Some(v) => table.insert(key, v).map(drop),
                    None => table.remove(key).map(drop),
                }
                .map_err(backend_err)?;
            }
        }
        txn.commit().map_err(backend_err)?;
        Ok(())
    }

    /// Reads the file into the OS's page cache, which redb reads from beyond its own cache.
    fn preload(&self) -> Result<()> {
        crate::file::preload(&self.path)
    }

    fn disk_bytes(&self) -> Result<u64> {
        Ok(fs::metadata(&self.path).at(&self.path)?.len())
    }
}

// a store is read from many threads
const _: () = {
    const fn sync<T: Sync>() {}
    sync::<RedbBackend>();
    sync::<KvStore<RedbBackend, crate::ReadOnly>>();
};

#[cfg(test)]
mod tests {
    use ::redb::{Database, TableDefinition};

    use crate::file::tests::TestFile;
    use crate::{Error, FileStore as _, RedbStore};

    #[test]
    fn rejects_another_programs_database() {
        let f = TestFile::new("other_redb");
        let db = Database::create(&f.0).unwrap();
        let txn = db.begin_write().unwrap();
        txn.open_table(TableDefinition::<&str, u64>::new("things"))
            .unwrap()
            .insert("a", 1)
            .unwrap();
        txn.commit().unwrap();
        drop(db);
        assert!(matches!(RedbStore::open(&f.0), Err(Error::NotAStore(_))));
        assert!(matches!(
            RedbStore::open_read_only(&f.0),
            Err(Error::NotAStore(_))
        ));
    }
}
