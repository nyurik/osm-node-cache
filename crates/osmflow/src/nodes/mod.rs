#![doc = include_str!("../../docs/nodes.md")]

use std::path::PathBuf;
use std::{io, result};

pub use self::dense::{DenseLoader, DenseOpts, DenseReader, DenseStore};
pub use self::memory::{MemoryLoader, MemoryReader, MemoryStore};
pub use self::paged::{PagedLoader, PagedOpts, PagedReader, PagedStats, PagedStore};
pub use self::store::{
    BulkLoad, FileStore, FixReport, LoadState, MAX_METADATA_BYTES, Metadata, Mode, NodeRead,
    NodeReader, NodeStore, ReadOnly, ReadWrite,
};
use crate::Location;

mod codec;
mod dense;
mod file;
mod memory;
mod paged;
mod store;

/// The traits to import to use any store.
pub mod prelude {
    pub use super::store::{BulkLoad, FileStore, NodeRead, NodeReader, NodeStore};
}

/// The highest node ID the stores take, about 68.7 billion: five times the highest OSM node ID of
/// 2026. Higher IDs are rejected with [`NodeStoreError::IdTooLarge`].
pub const MAX_NODE_ID: u64 = (1 << 36) - 1;

/// An error of a node location store.
#[derive(Debug, thiserror::Error)]
pub enum NodeStoreError {
    /// Reading or writing a file failed.
    #[error("{path}: {source}", path = .path.display())]
    Io {
        /// The store's file, or one of the files kept next to it.
        path: PathBuf,
        /// What the operating system reported.
        source: io::Error,
    },

    /// A store opened to read only does not exist; opening it to write creates it.
    #[error("{path} does not exist", path = .0.display())]
    NotFound(PathBuf),

    /// The file does not start with this store type's header: it is another kind of file, or a store
    /// of the other type.
    #[error("{path} is not a node store", path = .0.display())]
    NotAStore(PathBuf),

    /// The store was written by a version of osmflow with another file format.
    #[error("{path} has format version {found}, but this library reads version {expected}", path = .path.display())]
    UnsupportedVersion {
        /// The store's main file.
        path: PathBuf,
        /// The version in the file's header.
        found: u32,
        /// The only version this osmflow reads and writes.
        expected: u32,
    },

    /// The store is damaged in a way that opening it with fixing cannot repair.
    #[error("{path} is damaged: {reason}", path = .path.display())]
    Corrupt {
        /// The store's main file.
        path: PathBuf,
        /// What is damaged, for a person to read.
        reason: String,
    },

    /// The store is damaged, and opening it with fixing can repair it.
    #[error("{path} needs fixing: {reason}", path = .path.display())]
    NeedsFix {
        /// The store's main file.
        path: PathBuf,
        /// What is damaged, for a person to read.
        reason: String,
    },

    /// A commit was interrupted; opening the store for writing once rolls it back.
    #[error("{path} has an interrupted commit; open it for writing once to roll it back", path = .0.display())]
    NeedsRecovery(PathBuf),

    /// Another store, in this process or another, has the file open in a way that excludes this one: a
    /// store open to write excludes every other, and one open to read excludes writers.
    #[error("{path} is open in another store, or in another process", path = .0.display())]
    Locked(PathBuf),

    /// Stores size their data by node ID, so they take IDs only up to [`MAX_NODE_ID`].
    #[error("node ID {0} is above the highest supported ID {max}", max = MAX_NODE_ID)]
    IdTooLarge(u64),

    /// A location [`Location::is_valid`] rejects, as one in nanodegrees rather than 1e-7 degrees would be.
    #[error("node location {0:?} is outside ±90° latitude or ±180° longitude")]
    InvalidLocation(Location),

    /// A bulk load was given a node ID not above the one before. PBF files sorted by type and ID, as
    /// planet files and extracts are, give them in order.
    #[error("node IDs must be loaded in ascending order: {id} after {last}")]
    NotAscending {
        /// The node ID out of order.
        id: u64,
        /// The ID given before it.
        last: u64,
    },

    /// A resumed bulk load was given a node that differs from the one stored before.
    #[error("node {0} differs from the one the interrupted load stored; is it the same input?")]
    ResumeMismatch(u64),

    /// A bulk load needs an empty store, or one whose bulk load was interrupted.
    #[error("bulk loading needs an empty store, or one whose bulk load was interrupted")]
    NotEmpty,

    /// The store's bulk load was not finished; resume and finish it before changing nodes.
    #[error("the store's bulk load is not finished; resume it first")]
    LoadUnfinished,

    /// A metadata key is empty or longer than 255 bytes, or the metadata would take more than
    /// [`MAX_METADATA_BYTES`] encoded.
    #[error(
        "metadata keys take 1 to 255 bytes, and all metadata at most {MAX_METADATA_BYTES} bytes"
    )]
    MetadataTooLarge,
}

/// The result of a node location store's operation.
pub(crate) type Result<T, E = NodeStoreError> = result::Result<T, E>;
