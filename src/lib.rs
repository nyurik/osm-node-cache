#![allow(clippy::needless_doctest_main)]
#![cfg_attr(feature = "default", doc = include_str!("../README.md"))]

use std::path::PathBuf;

use thiserror::Error;

#[cfg(unix)]
pub use crate::dense_file::Advice;
pub use crate::dense_file::{DenseFileCache, DenseFileCacheOpts};
pub use crate::hashmap::HashMapCache;

mod dense_file;
mod hashmap;
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

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Serde(#[from] serde_json::Error),

    #[error(transparent)]
    BincodeEncode(#[from] bincode::error::EncodeError),

    #[error(transparent)]
    BincodeDecode(#[from] bincode::error::DecodeError),
}

pub type OsmNodeCacheResult<T> = Result<T, OsmNodeCacheError>;
