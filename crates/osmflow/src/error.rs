use std::fmt;

/// The result of every fallible `osmflow` call.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Where in a file an error was found: the byte offset of the blob it is in, and the blob's index (0 is the file
/// header). The offset of a blob is that of the 4-byte length before its header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Position {
    pub blob: u64,
    pub offset: u64,
}

impl fmt::Display for Position {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "blob {} at byte {}", self.blob, self.offset)
    }
}

/// An error reading an OSM PBF file.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("{position}: {kind}")]
    Format {
        position: Position,
        kind: FormatError,
    },
    #[error("the file is empty")]
    EmptyFile,
    #[error("the file starts with a {0:?} blob, not OSMHeader")]
    MissingHeader(String),
    #[error("the file requires the feature {0:?}, which osmflow does not support")]
    UnsupportedFeature(String),
    #[error("the thread pool could not be started: {0}")]
    ThreadPool(String),
}

impl Error {
    pub(crate) fn format(position: Position, kind: FormatError) -> Self {
        Self::Format { position, kind }
    }
}

/// What is wrong with a file's data.
#[derive(Debug, thiserror::Error)]
pub enum FormatError {
    #[error("the file ends inside a blob")]
    Truncated,
    #[error("blob header of {0} bytes is larger than the 64 KiB limit")]
    BlobHeaderTooLarge(u32),
    #[error("blob of {0} bytes is outside the allowed 1 byte to 32 MiB")]
    BlobSize(i32),
    #[error("blob declares an uncompressed size of {0} bytes, outside the allowed 0 to 32 MiB")]
    RawSize(i32),
    #[error("compressed blob does not declare its uncompressed size")]
    MissingRawSize,
    #[error("blob has no data")]
    MissingData,
    #[error("blob is compressed with {0}, which osmflow does not support")]
    UnsupportedCompression(&'static str),
    #[error("blob decompressed to {actual} bytes, but declares {expected}")]
    RawSizeMismatch { expected: usize, actual: usize },
    #[error("zlib error: {0}")]
    Zlib(String),
    #[error("invalid protobuf in {what}: {source}")]
    Protobuf {
        what: &'static str,
        source: buffa::DecodeError,
    },
    #[error("invalid packed array in {0}")]
    Packed(&'static str),
    #[error("malformed protobuf message: {0}")]
    Message(&'static str),
    #[error("{0} have different lengths")]
    LengthMismatch(&'static str),
    #[error("string index {0} is outside the block's string table")]
    StringIndex(u32),
    #[error("string table entry {0} is not valid UTF-8")]
    InvalidUtf8(u32),
    #[error("relation member type {0} is not 0 (node), 1 (way) or 2 (relation)")]
    MemberType(u64),
    #[error("dense node tags do not match the nodes: {0}")]
    DenseTags(&'static str),
    #[error("a plain node has no {0}")]
    NodeLocation(&'static str),
    #[error("an object has no id")]
    MissingId,
    #[error("{what} {value} is not positive")]
    Granularity { what: &'static str, value: i32 },
}

impl FormatError {
    /// For `map_err`: a protobuf error in the message `what`.
    pub(crate) fn protobuf(what: &'static str) -> impl FnOnce(buffa::DecodeError) -> Self {
        move |source| Self::Protobuf { what, source }
    }
}
