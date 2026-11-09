use std::{fmt, io, result};

#[cfg(feature = "nodes")]
use crate::nodes;

/// The result of every fallible `osmflow` call.
pub(crate) type Result<T, E = PbfError> = result::Result<T, E>;

/// Where in a file an error was found: the byte offset of the blob it is in, and the blob's index (0 is the file
/// header). The offset of a blob is that of the 4-byte length before its header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlobPosition {
    /// The blob's index in the file; 0 is the header blob.
    pub blob: u64,
    /// The byte offset of the 4-byte length before the blob's header.
    pub offset: u64,
}

impl fmt::Display for BlobPosition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "blob {} at byte {}", self.blob, self.offset)
    }
}

/// An error reading an OSM PBF file.
#[derive(Debug, thiserror::Error)]
pub enum PbfError {
    /// Reading the file failed. Data that breaks the format is [`Format`](Self::Format) instead.
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    /// The file's data breaks the PBF format, or one of the limits osmflow checks it against.
    #[error("{position}: {kind}")]
    Format {
        /// The blob the error is in.
        position: BlobPosition,
        /// What is wrong.
        kind: PbfFormatError,
    },
    /// The file has no bytes. A file that ends inside its first blob is a [`Format`](Self::Format) error instead.
    #[error("the file is empty")]
    EmptyFile,
    /// The first blob is not the `OSMHeader` blob every OSM PBF file starts with, so the file is probably not one. The
    /// string is the first blob's type.
    #[error("the file starts with a {0:?} blob, not OSMHeader")]
    MissingHeader(String),
    /// The header lists a required feature osmflow does not read. It reads `OsmSchema-V0.6`, `DenseNodes`,
    /// `HistoricalInformation` and `LocationsOnWays`.
    #[error("the file requires the feature {0:?}, which osmflow does not support")]
    UnsupportedFeature(String),
    /// The shared pool of decoding threads could not be started, as when the OS refuses new threads.
    /// [`PbfReader::with_pool`](crate::PbfReader::with_pool) gives a reader a pool of the caller's instead.
    #[error("the thread pool could not be started: {0}")]
    ThreadPool(String),
    /// An error of a node location store.
    #[cfg(feature = "nodes")]
    #[error(transparent)]
    Nodes(#[from] nodes::NodeStoreError),
}

impl PbfError {
    pub(crate) fn format(position: BlobPosition, kind: PbfFormatError) -> Self {
        Self::Format { position, kind }
    }
}

/// What is wrong with a file's data.
#[derive(Debug, thiserror::Error)]
pub enum PbfFormatError {
    /// The file ends inside a blob, as a file still being written or a cut download does.
    #[error("the file ends inside a blob")]
    Truncated,
    /// A blob header is longer than the format's 64 KiB limit, so the length before it is probably not one.
    #[error("blob header of {0} bytes is larger than the 64 KiB limit")]
    BlobHeaderTooLarge(u32),
    /// A blob's size is 0, negative, or over the format's 32 MiB limit.
    #[error("blob of {0} bytes is outside the allowed 1 byte to 32 MiB")]
    BlobSize(i32),
    /// A blob declares an uncompressed size that is negative or over the format's 32 MiB limit.
    #[error("blob declares an uncompressed size of {0} bytes, outside the allowed 0 to 32 MiB")]
    RawSize(i32),
    /// A compressed blob does not declare its uncompressed size, which the format requires.
    #[error("compressed blob does not declare its uncompressed size")]
    MissingRawSize,
    /// A blob has neither uncompressed nor compressed data.
    #[error("blob has no data")]
    MissingData,
    /// A blob is compressed with lzma, bzip2, lz4 or zstd. The format allows them, but osmflow reads only
    /// uncompressed and zlib blobs.
    #[error("blob is compressed with {0}, which osmflow does not support")]
    UnsupportedCompression(&'static str),
    /// A blob inflated to another size than it declares, so it is damaged.
    #[error("blob decompressed to {actual} bytes, but declares {expected}")]
    RawSizeMismatch {
        /// The uncompressed size the blob declares.
        expected: usize,
        /// The size it inflated to.
        actual: usize,
    },
    /// Inflating a zlib blob failed, so it is damaged. The string is the inflater's message.
    #[error("zlib error: {0}")]
    Zlib(String),
    /// A message is not valid protobuf.
    #[error("invalid protobuf in {what}: {source}")]
    Protobuf {
        /// The message being decoded, such as `PrimitiveBlock`.
        what: &'static str,
        /// What the protobuf decoder found.
        source: buffa::DecodeError,
    },
    /// A packed array ends inside a value, or holds a value too large for its field. The string names the array.
    #[error("invalid packed array in {0}")]
    Packed(&'static str),
    /// A field has a wire type its message does not allow, or a message ends inside a field. The string names the
    /// message.
    #[error("malformed protobuf message: {0}")]
    Message(&'static str),
    /// Arrays that must have one value per object or per tag, such as a way's tag keys and values, have different
    /// lengths. The string names them.
    #[error("{0} have different lengths")]
    LengthMismatch(&'static str),
    /// An object refers to a string past the end of its block's string table.
    #[error("string index {0} is outside the block's string table")]
    StringIndex(u32),
    /// A string of the block's string table is not UTF-8, which OSM strings must be. The value is its index.
    #[error("string table entry {0} is not valid UTF-8")]
    InvalidUtf8(u32),
    /// A relation member's type is not one of the three the format numbers.
    #[error("relation member type {0} is not 0 (node), 1 (way) or 2 (relation)")]
    MemberType(u64),
    /// The dense nodes' tag array does not split into one list per node, each of key and value pairs.
    #[error("dense node tags do not match the nodes: {0}")]
    DenseTags(&'static str),
    /// A plain node lacks its latitude or longitude, which the format requires. The string names which.
    #[error("a plain node has no {0}")]
    NodeLocation(&'static str),
    /// An object has no ID, which the format requires.
    #[error("an object has no id")]
    MissingId,
    /// The block's coordinate or date granularity is 0 or negative, which would put every location or time at one
    /// value.
    #[error("{what} {value} is not positive")]
    Granularity {
        /// Which granularity: coordinates or dates.
        what: &'static str,
        /// The value the block gives.
        value: i32,
    },
}

impl PbfFormatError {
    /// For `map_err`: a protobuf error in the message `what`.
    pub(crate) fn protobuf(what: &'static str) -> impl FnOnce(buffa::DecodeError) -> Self {
        move |source| Self::Protobuf { what, source }
    }
}
