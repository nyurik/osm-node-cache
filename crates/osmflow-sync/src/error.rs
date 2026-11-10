use std::io;
use std::path::{Path, PathBuf};

use osmflow::PbfError;
use osmflow::nodes::NodeStoreError;
use osmflow::osc::OscError;

/// An error of an import, an update or a rebuild.
#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    /// A query failed, or the database could not be reached.
    #[error("database: {0}")]
    Db(#[from] postgres::Error),
    /// Reading the PBF file failed.
    #[error(transparent)]
    Pbf(#[from] PbfError),
    /// Reading a replication diff or state failed.
    #[error(transparent)]
    Osc(#[from] OscError),
    /// The node file failed.
    #[error(transparent)]
    Nodes(#[from] NodeStoreError),
    /// The reverse index failed.
    #[error("{path}: {source}", path = .path.display())]
    Index {
        /// The index file.
        path: PathBuf,
        /// What failed.
        source: redb::Error,
    },
    /// A file is missing: the node file or the reverse index, which `rebuild` makes again.
    #[error("{path} does not exist; `rebuild` makes it from the database", path = .0.display())]
    Missing(PathBuf),
    /// Another I/O error.
    #[error(transparent)]
    Io(#[from] io::Error),
    /// The database, its files and the input do not fit together, as the message says.
    #[error("{0}")]
    State(String),
}

impl SyncError {
    pub(crate) fn index(path: &Path, source: redb::Error) -> Self {
        Self::Index {
            path: path.to_path_buf(),
            source,
        }
    }

    pub(crate) fn state(msg: impl Into<String>) -> Self {
        Self::State(msg.into())
    }
}
