#![allow(clippy::needless_doctest_main)]
#![cfg_attr(feature = "paged", doc = include_str!("../README.md"))]

use std::path::PathBuf;

use thiserror::Error;

pub use crate::codec::Location;
#[cfg(feature = "dense")]
pub use crate::dense::{DenseLoader, DenseOpts, DenseReader, DenseStore};
#[cfg(feature = "redb")]
pub use crate::kv::redb::{RedbBackend, RedbOpts, RedbStore};
pub use crate::kv::{KvBackend, KvLoader, KvOpts, KvReader, KvStore};
#[cfg(feature = "memory")]
pub use crate::memory::{MemoryLoader, MemoryReader, MemoryStore};
#[cfg(feature = "paged")]
pub use crate::paged::{PagedLoader, PagedOpts, PagedReader, PagedStats, PagedStore};
pub use crate::store::{
    BulkLoad, FileStore, FixReport, LoadState, MAX_METADATA_BYTES, Metadata, Mode, NodeRead,
    NodeReader, NodeStore, ReadOnly, ReadWrite,
};

// the key-value stores use only part of it
#[cfg_attr(not(feature = "paged"), allow(dead_code))]
mod codec;
#[cfg(feature = "dense")]
mod dense;
// the database stores use only part of it
#[cfg_attr(not(feature = "paged"), allow(dead_code))]
mod file;
mod kv;
#[cfg(feature = "memory")]
mod memory;
#[cfg(feature = "paged")]
mod paged;
mod store;

/// The traits to import to use any store.
pub mod prelude {
    pub use crate::store::{BulkLoad, FileStore, NodeRead, NodeReader, NodeStore};
}

/// The highest node ID the stores take, about 68.7 billion: five times the highest OSM node ID of
/// 2026. Higher IDs are rejected with [`Error::IdTooLarge`].
pub const MAX_NODE_ID: u64 = (1 << 36) - 1;

#[derive(Error, Debug)]
pub enum Error {
    #[error("{path}: {source}", path = .path.display())]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("{path} does not exist", path = .0.display())]
    NotFound(PathBuf),

    #[error("{path} is not a node store", path = .0.display())]
    NotAStore(PathBuf),

    #[error("{path} has format version {found}, but this library reads version {expected}", path = .path.display())]
    UnsupportedVersion {
        path: PathBuf,
        found: u32,
        expected: u32,
    },

    /// The store is damaged in a way that opening it with fixing cannot repair.
    #[error("{path} is damaged: {reason}", path = .path.display())]
    Corrupt { path: PathBuf, reason: String },

    /// The store is damaged, and opening it with fixing can repair it.
    #[error("{path} needs fixing: {reason}", path = .path.display())]
    NeedsFix { path: PathBuf, reason: String },

    /// A commit was interrupted; opening the store for writing once rolls it back.
    #[error("{path} has an interrupted commit; open it for writing once to roll it back", path = .0.display())]
    NeedsRecovery(PathBuf),

    #[error("{path} is open in another store, or in another process", path = .0.display())]
    Locked(PathBuf),

    #[error("node ID {0} is above the highest supported ID {max}", max = crate::MAX_NODE_ID)]
    IdTooLarge(u64),

    #[error("node location {0:?} is outside ±90° latitude or ±180° longitude")]
    InvalidLocation(Location),

    #[error("node IDs must be loaded in ascending order: {id} after {last}")]
    NotAscending { id: u64, last: u64 },

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

    /// An error of a key-value database backend.
    #[error(transparent)]
    Backend(Box<dyn std::error::Error + Send + Sync>),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
