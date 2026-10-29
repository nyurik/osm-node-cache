#![allow(clippy::needless_doctest_main)]
#![cfg_attr(feature = "default", doc = include_str!("../README.md"))]

use std::path::PathBuf;

use thiserror::Error;

pub use crate::codec::Location;
#[cfg(unix)]
pub use crate::dense_file::Advice;
pub use crate::dense_file::{DenseFileCache, DenseFileCacheOpts};
pub use crate::hashmap::HashMapCache;
#[cfg(feature = "redb")]
pub use crate::kv::redb::{RedbBackend, RedbOpts};
pub use crate::kv::{KvBackend, KvNodeStore, KvOpts, KvReader};
#[cfg(feature = "native")]
pub use crate::paged::{PagedBuilder, PagedFileCache, PagedFileCacheOpts, PagedReader, PagedStats};
pub use crate::store::{NodeBuilder, NodeReader, NodeStore};

// the key-value store uses only part of it
#[cfg_attr(not(feature = "native"), allow(dead_code))]
mod codec;
mod dense_file;
mod hashmap;
mod kv;
#[cfg(feature = "native")]
mod paged;
mod store;
mod traits;

pub use traits::{Cache, CacheStore};

#[derive(Error, Debug)]
pub enum OsmNodeCacheError {
    #[error("Invalid cache file {path}: {1}", path = .0.to_string_lossy())]
    InvalidCacheFile(PathBuf, std::io::Error),

    #[error("Invalid cache page size: page_size={page_size} is not a multiple of {element_size}.")]
    InvalidPageSize {
        page_size: usize,
        element_size: usize,
    },

    #[error("Invalid cache file {path}: {1}", path = .0.to_string_lossy())]
    InvalidFormat(PathBuf, String),

    #[error("Node IDs must be added in ascending order: {id} after {last}")]
    NotAscending { id: u64, last: u64 },

    #[error(transparent)]
    Io(#[from] std::io::Error),

    /// An error of a key-value store backend
    #[error(transparent)]
    Store(Box<dyn std::error::Error + Send + Sync>),

    #[error(transparent)]
    Serde(#[from] serde_json::Error),

    #[error(transparent)]
    BincodeEncode(#[from] bincode_next::error::EncodeError),

    #[error(transparent)]
    BincodeDecode(#[from] bincode_next::error::DecodeError),
}

pub type OsmNodeCacheResult<T> = Result<T, OsmNodeCacheError>;
