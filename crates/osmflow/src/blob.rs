//! Reading a file's blobs: the 4-byte length, the blob header, and the blob, without decoding it.

use std::cell::RefCell;
use std::io::Read;

use buffa::MessageView as _;

use crate::block::{Block, Select};
use crate::error::{Error, FormatError, Position, Result};
use crate::generated::raw::osmflow_raw::__buffa::view::oneof::blob::Data;
use crate::generated::raw::osmflow_raw::{BlobHeaderView, BlobView};

/// The largest blob header the format allows.
const MAX_HEADER: u32 = 64 * 1024;
/// The largest blob, compressed or not, the format allows.
const MAX_BLOB: usize = 32 * 1024 * 1024;

/// What a blob holds, from its header's type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BlobKind {
    /// `OSMHeader`: the file header.
    Header,
    /// `OSMData`: a block of objects.
    Data,
    /// Any other type, which readers skip.
    Other(String),
}

/// One blob, read but not decompressed.
#[derive(Debug)]
pub(crate) struct RawBlob {
    pub(crate) position: Position,
    pub(crate) kind: BlobKind,
    /// The encoded `Blob` message.
    data: Vec<u8>,
}

impl RawBlob {
    /// The bytes this blob takes in the file, without its length and header.
    pub(crate) fn len(&self) -> usize {
        self.data.len()
    }

    /// Calls `f` with the blob's content, decompressed into this thread's buffer.
    pub(crate) fn with_content<T>(&self, f: impl FnOnce(&[u8]) -> Result<T>) -> Result<T> {
        with_content(&self.data, f).map_err(|kind| Error::format(self.position, kind))?
    }

    /// Decompresses and decodes a data blob.
    pub(crate) fn decode(&self, select: Select) -> Result<Block> {
        self.with_content(|data| Block::decode(self.position, data, select))
    }
}

// A decompressor and its output buffer, kept per thread: both are reused from blob to blob instead of being
// allocated for each. The buffer grows to the largest blob a thread has decompressed, at most 32 MiB.
thread_local! {
    static ZLIB: RefCell<(flate2::Decompress, Vec<u8>)> = RefCell::new((flate2::Decompress::new(true), Vec::new()));
}

/// Calls `f` with a `Blob` message's content: its raw data as it is, or its zlib data decompressed into this thread's
/// buffer. `f`'s own result is returned inside, so its errors keep their own position.
fn with_content<T>(
    blob: &[u8],
    f: impl FnOnce(&[u8]) -> Result<T>,
) -> std::result::Result<Result<T>, FormatError> {
    let view = BlobView::decode_view(blob).map_err(FormatError::protobuf("blob"))?;
    let raw_size = view
        .raw_size
        .map(|size| {
            usize::try_from(size)
                .ok()
                .filter(|&s| s <= MAX_BLOB)
                .ok_or(FormatError::RawSize(size))
        })
        .transpose()?;
    match view.data.ok_or(FormatError::MissingData)? {
        Data::Raw(data) => match raw_size {
            Some(expected) if expected != data.len() => Err(FormatError::RawSizeMismatch {
                expected,
                actual: data.len(),
            })?,
            _ => Ok(f(data)),
        },
        Data::ZlibData(data) => {
            let expected = raw_size.ok_or(FormatError::MissingRawSize)?;
            ZLIB.with_borrow_mut(|(zlib, out)| {
                zlib.reset(true);
                out.clear();
                // Room for one byte more than declared, to detect data longer than its declared size.
                out.reserve(expected + 1);
                let status = zlib
                    .decompress_vec(data, out, flate2::FlushDecompress::Finish)
                    .map_err(|e| FormatError::Zlib(e.to_string()))?;
                if status != flate2::Status::StreamEnd || out.len() != expected {
                    Err(FormatError::RawSizeMismatch {
                        expected,
                        actual: out.len(),
                    })?;
                }
                Ok(f(out))
            })
        }
        Data::LzmaData(_) => Err(FormatError::UnsupportedCompression("lzma"))?,
        Data::OBSOLETEBzip2Data(_) => Err(FormatError::UnsupportedCompression("bzip2"))?,
        Data::Lz4Data(_) => Err(FormatError::UnsupportedCompression("lz4"))?,
        Data::ZstdData(_) => Err(FormatError::UnsupportedCompression("zstd"))?,
    }
}

/// Reads the blobs of a file in order, and stops after the first error.
pub(crate) struct BlobReader<R> {
    reader: R,
    position: Position,
    done: bool,
}

impl<R: Read> BlobReader<R> {
    pub(crate) fn new(reader: R) -> Self {
        Self {
            reader,
            position: Position { blob: 0, offset: 0 },
            done: false,
        }
    }

    /// Reads up to `len` bytes, fewer only at the end of the file, without zeroing a buffer first.
    fn read(&mut self, len: usize) -> Result<Vec<u8>> {
        let mut buf = Vec::with_capacity(len);
        (&mut self.reader).take(len as u64).read_to_end(&mut buf)?;
        Ok(buf)
    }

    /// Reads exactly `len` bytes: the file ending first is an error.
    fn read_exact(&mut self, len: usize) -> Result<Vec<u8>> {
        let buf = self.read(len)?;
        if buf.len() == len {
            Ok(buf)
        } else {
            Err(self.error(FormatError::Truncated))
        }
    }

    fn error(&self, kind: FormatError) -> Error {
        Error::format(self.position, kind)
    }

    fn read_blob(&mut self) -> Result<Option<RawBlob>> {
        let len = self.read(4)?;
        if len.is_empty() {
            return Ok(None);
        }
        let len: [u8; 4] = len
            .try_into()
            .map_err(|_| self.error(FormatError::Truncated))?;
        let header_len = u32::from_be_bytes(len);
        if header_len > MAX_HEADER {
            return Err(self.error(FormatError::BlobHeaderTooLarge(header_len)));
        }
        let header = self.read_exact(header_len as usize)?;
        let view = BlobHeaderView::decode_view(&header).map_err(|source| {
            self.error(FormatError::Protobuf {
                what: "blob header",
                source,
            })
        })?;
        let size = view.datasize.unwrap_or(0);
        let Some(len) = usize::try_from(size)
            .ok()
            .filter(|s| (1..=MAX_BLOB).contains(s))
        else {
            return Err(self.error(FormatError::BlobSize(size)));
        };
        let kind = match view.r#type.unwrap_or_default() {
            b"OSMHeader" => BlobKind::Header,
            b"OSMData" => BlobKind::Data,
            other => BlobKind::Other(String::from_utf8_lossy(other).into_owned()),
        };
        let data = self.read_exact(len)?;
        let position = self.position;
        self.position = Position {
            blob: position.blob + 1,
            offset: position.offset + 4 + u64::from(header_len) + data.len() as u64,
        };
        Ok(Some(RawBlob {
            position,
            kind,
            data,
        }))
    }
}

impl<R: Read> Iterator for BlobReader<R> {
    type Item = Result<RawBlob>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        let blob = self.read_blob().transpose();
        self.done = !matches!(blob, Some(Ok(_)));
        blob
    }
}
