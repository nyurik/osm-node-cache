#![doc = include_str!("../README.md")]

mod copy;
mod db;
mod error;
mod files;
mod import;
mod index;
mod rebuild;
mod sort;
mod update;

use std::path::{Path, PathBuf};
use std::{fmt, thread};

use osmflow::nodes::PagedStore;
use osmflow::nodes::prelude::*;
use osmflow::osc::Replication;

pub use crate::db::DbState;
pub use crate::error::SyncError;
pub use crate::import::{ImportOpts, ImportStats};
pub use crate::rebuild::{Rebuild, RebuildStats};
pub use crate::update::{UpdateOpts, UpdateStats};

/// A database kept current from replication, and the directory of its node file and reverse index.
#[derive(Clone)]
pub struct Mirror {
    url: String,
    dir: PathBuf,
}

/// Without the database's URL, which may hold a password.
impl fmt::Debug for Mirror {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Mirror")
            .field("dir", &self.dir)
            .finish_non_exhaustive()
    }
}

/// The revision and identity of the node file or the reverse index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileStamp {
    /// The identity of the database the file was made for, which must be the database's; none for a node file whose
    /// rebuild was cut short.
    pub id: Option<i64>,
    /// The database revision the file is up to.
    pub rev: i64,
}

/// Where the database and its files are.
#[derive(Debug)]
pub struct Status {
    /// The database's position.
    pub db: DbState,
    /// The node file's stamp, or why it could not be read.
    pub nodes: Result<FileStamp, SyncError>,
    /// The reverse index's stamp, or why it could not be read.
    pub index: Result<FileStamp, SyncError>,
}

impl Mirror {
    /// A mirror in the database at `url` (`postgresql://user@host/db`, or `host=… user=…`), with its files in `dir`.
    /// Connections are not encrypted.
    pub fn new(url: impl Into<String>, dir: impl Into<PathBuf>) -> Self {
        Self {
            url: url.into(),
            dir: dir.into(),
        }
    }

    /// Creates the `osm` schema from a PBF file, which must be sorted by type and ID as published files are, and makes
    /// the node file and the reverse index in the directory, replacing any there.
    ///
    /// The file's replication sequence is the one [`ImportOpts::sequence`] or its header gives. `replication`, the
    /// stream [`ImportOpts::replication_url`] or the header names, finds it from the file's timestamp when neither
    /// does, as for a planet file, and checks a sequence given in the options against that timestamp when the header
    /// gives none for the stream.
    pub fn import(
        &self,
        pbf: impl AsRef<Path>,
        replication: Option<&Replication>,
        opts: &ImportOpts,
    ) -> Result<ImportStats, SyncError> {
        import::import(self, pbf.as_ref(), replication, opts)
    }

    /// Applies the diffs of `replication` after the database's sequence, up to the stream's latest or
    /// [`UpdateOpts::until`]. The files are brought up to the database first if a crash left them behind.
    pub fn update(
        &self,
        replication: &Replication,
        opts: &UpdateOpts,
    ) -> Result<UpdateStats, SyncError> {
        update::update(self, replication, opts)
    }

    /// Makes the node file and the reverse index again, up to the database's revision. The database is only read.
    pub fn rebuild(&self, from: &Rebuild<'_>) -> Result<RebuildStats, SyncError> {
        rebuild::rebuild(self, from)
    }

    /// The database's position in its replication stream.
    pub fn state(&self) -> Result<DbState, SyncError> {
        DbState::read(&mut db::connect(&self.url)?)
    }

    /// The database's position, and its files' stamps.
    pub fn status(&self) -> Result<Status, SyncError> {
        let db = self.state()?;
        let nodes = PagedStore::open_read_only(self.node_path())
            .map(|s| FileStamp {
                id: files::node_file_id(&s),
                rev: s.stamp().cast_signed(),
            })
            .map_err(SyncError::from);
        let index = index::ReverseIndex::open(&self.index_path())
            .and_then(|i| i.stamp())
            .map(|(id, rev)| FileStamp { id: Some(id), rev });
        Ok(Status { db, nodes, index })
    }

    /// The node file.
    #[must_use]
    pub fn node_path(&self) -> PathBuf {
        self.dir.join("nodes.cache")
    }

    /// The reverse index.
    #[must_use]
    pub fn index_path(&self) -> PathBuf {
        self.dir.join("ways.redb")
    }
}

/// The connections an import copies on by default: half the cores, from 1 to 8.
fn default_connections() -> usize {
    (thread::available_parallelism().map_or(1, usize::from) / 2).clamp(1, 8)
}
