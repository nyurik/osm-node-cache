//! Reading a file's blobs: the 4-byte length, the blob header, and the blob, all of it or only its start; and loading
//! a data blob into a block, skipping what the read's filters do not want.

use std::borrow::Cow;
use std::cell::RefCell;
use std::fs::File;
use std::io::{self, ErrorKind, Read as _, Seek as _, SeekFrom};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicI8, Ordering};
use std::{mem, result};

use buffa::MessageView as _;
use flate2::{Decompress, FlushDecompress, Status};

use crate::block::{Block, Fields};
use crate::error::{BlobPosition, PbfError, PbfFormatError, Result};
use crate::filter::Filters;
use crate::generated::raw::osmflow_raw::__buffa::view::oneof::blob::Data;
use crate::generated::raw::osmflow_raw::{BlobHeaderView, BlobView};
use crate::packed::varint;
use crate::peek::{Kind, Peek, peek};

type FResult<T> = result::Result<T, PbfFormatError>;

/// The largest blob header the format allows.
const MAX_HEADER: u32 = 64 * 1024;
/// The largest blob, compressed or not, the format allows.
const MAX_BLOB: usize = 32 * 1024 * 1024;
/// Bytes read from the file at once.
const READ_BUFFER: usize = 8 << 20;
/// The start of a blob a worker reads when the rest may not be needed: the string table of most blocks fits in it, and
/// more of a blob is read when it does not.
const PEEK_READ: usize = 128 << 10;
/// The bytes of a header and its blob's start read in one small read while only blob starts are wanted: enough to
/// find the next header, so the reader walks the file quickly while the workers read the starts they peek at.
const HEADER_ROOM: usize = 4 << 10;
/// Blobs this small are always read whole: skipping their rest saves less than the extra read costs.
const SMALL_BLOB: usize = 512 << 10;

/// How a read with filters reads the blobs it may skip. Reads without filters always read whole blobs, one after
/// another.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ReadMode {
    /// Reads only the start of a large blob while most recent blocks are skipped, and whole blobs otherwise: skipped
    /// blocks cost little reading, without many small reads when few are skipped. Blobs under 512 KiB are always read
    /// whole.
    #[default]
    Auto,
    /// Always reads whole blobs, in large reads one after another: fewest reads, for storage where each read is slow
    /// to start, such as a network volume or a spinning disk. Filters still skip decoding.
    Sequential,
    /// Reads only the start of every blob, and the rest only if its block is kept: least data, for a fast local disk
    /// when the filters skip most of a file with large blobs.
    Selective,
}

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

/// One blob: all of its `Blob` message, or the start of it when the rest may not be needed.
#[derive(Debug)]
pub(crate) struct RawBlob {
    pub(crate) position: BlobPosition,
    pub(crate) kind: BlobKind,
    span: Span,
    /// The uncompressed size the blob declares, when its start shows it.
    raw_size: Option<usize>,
    /// The next data blob, for telling whether this block holds a single object type.
    next: Option<Span>,
}

/// A `Blob` message: where it is in the file, its length, and its bytes read so far (all of them, or its start).
#[derive(Debug, Clone)]
#[allow(
    clippy::rc_buffer,
    reason = "an `Arc<[u8]>` made from the read `Vec` would copy every blob once more"
)]
struct Span {
    start: u64,
    len: usize,
    data: Arc<Vec<u8>>,
}

impl RawBlob {
    /// The bytes the blob stands for while it is read and decoded: the larger of its own size and its declared
    /// uncompressed size.
    pub(crate) fn content_len(&self) -> usize {
        let len = self.span.len;
        self.raw_size.map_or(len, |size| size.max(len))
    }

    /// Calls `f` with the content of a blob read whole, decompressed into this thread's buffer.
    pub(crate) fn with_content<T>(&self, f: impl FnOnce(&[u8]) -> Result<T>) -> Result<T> {
        with_content(&self.span.data, |data, _| f(data))
            .map_err(|kind| PbfError::format(self.position, kind))?
    }
}

// A decompressor and its output buffer, kept per thread: both are reused from blob to blob instead of being
// allocated for each. The buffer grows to the largest blob a thread has decompressed, at most 32 MiB. The second pair
// peeks at the next blob while the first holds a block half inflated.
thread_local! {
    static ZLIB: RefCell<(Decompress, Vec<u8>)> = RefCell::new((Decompress::new(true), Vec::new()));
    static NEXT: RefCell<(Decompress, Vec<u8>)> = RefCell::new((Decompress::new(true), Vec::new()));
}

/// Inflates `input`, the rest of the zlib stream `d` reads, into `out`, which must then hold `expected` bytes.
fn inflate(d: &mut Decompress, input: &[u8], out: &mut Vec<u8>, expected: usize) -> FResult<()> {
    // Room for one byte more than declared, to detect data longer than its declared size. `decompress_vec` writes only
    // into the buffer's capacity and never grows it, so a stream that inflates past its declared size stops there and
    // fails the length check below.
    out.reserve((expected + 1).saturating_sub(out.len()));
    let status = d
        .decompress_vec(input, out, FlushDecompress::Finish)
        .map_err(|e| PbfFormatError::Zlib(e.to_string()))?;
    if status != Status::StreamEnd || out.len() != expected {
        return Err(PbfFormatError::RawSizeMismatch {
            expected,
            actual: out.len(),
        });
    }
    Ok(())
}

/// Calls `f` with a `Blob` message's content: its raw data as it is, or its zlib data decompressed into this thread's
/// buffer. `f`'s own result is returned inside, so its errors keep their own position.
fn with_content<T>(blob: &[u8], f: impl FnOnce(&[u8], bool) -> Result<T>) -> FResult<Result<T>> {
    let view = BlobView::decode_view(blob).map_err(PbfFormatError::protobuf("blob"))?;
    let raw_size = view
        .raw_size
        .map(|size| {
            usize::try_from(size)
                .ok()
                .filter(|&s| s <= MAX_BLOB)
                .ok_or(PbfFormatError::RawSize(size))
        })
        .transpose()?;
    match view.data.ok_or(PbfFormatError::MissingData)? {
        Data::Raw(data) => match raw_size {
            Some(expected) if expected != data.len() => Err(PbfFormatError::RawSizeMismatch {
                expected,
                actual: data.len(),
            }),
            _ => Ok(f(data, false)),
        },
        Data::ZlibData(data) => {
            let expected = raw_size.ok_or(PbfFormatError::MissingRawSize)?;
            ZLIB.with_borrow_mut(|(zlib, out)| {
                zlib.reset(true);
                out.clear();
                inflate(zlib, data, out, expected)?;
                Ok(f(out, true))
            })
        }
        Data::LzmaData(_) => Err(PbfFormatError::UnsupportedCompression("lzma")),
        Data::OBSOLETEBzip2Data(_) => Err(PbfFormatError::UnsupportedCompression("bzip2")),
        Data::Lz4Data(_) => Err(PbfFormatError::UnsupportedCompression("lz4")),
        Data::ZstdData(_) => Err(PbfFormatError::UnsupportedCompression("zstd")),
    }
}

/// How a [`BlobReader`] reads the next blob: the bytes of its message to read first (all of it if larger), and how
/// many bytes of the file to read at once.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Want {
    pub(crate) start: usize,
    pub(crate) chunk: usize,
}

impl Want {
    pub(crate) const WHOLE: Self = Self {
        start: usize::MAX,
        chunk: READ_BUFFER,
    };
}

/// Reads the blobs of a file in order, each whole or only its start, and stops after the first error. It reads the
/// file in chunks of the size the next blob asks for: large ones while blobs are read whole, and one small read per
/// blob while only their starts are, so the rest of each is never read.
pub(crate) struct BlobReader {
    file: File,
    file_len: u64,
    /// The bytes of the file from `buf_start`.
    buf: Vec<u8>,
    buf_start: u64,
    /// How much of the file to read at once for the next header: as for the blob before, which blobs near it are like.
    chunk: usize,
    position: BlobPosition,
    done: bool,
    /// For a file too large to stay cached: how far from the start it is dropped from the page cache.
    dropped: Option<u64>,
}

/// A file larger than this share of the machine's memory is dropped from the page cache behind the reader.
const CACHED_SHARE: u64 = 2;
/// How far behind the reader its file is dropped: past the blobs the pool may still be reading.
const DROP_LAG: u64 = if cfg!(test) { 64 << 10 } else { 1 << 30 };
/// How much of the file is dropped at once.
const DROP_STEP: u64 = if cfg!(test) { 16 << 10 } else { 64 << 20 };

/// Opens the PBF file at `path` to read each part of it once. On Linux 6.3 and later, its pages are then the first the
/// page cache gives up when memory runs short, so a large file read through does not push out what other code keeps
/// in memory, such as a node store; while memory is free, they stay cached. Elsewhere it is a plain open.
pub(crate) fn open_once(path: &Path) -> io::Result<File> {
    let file = File::open(path)?;
    #[cfg(target_os = "linux")]
    {
        use rustix::fs::{Advice, fadvise};

        // only a hint: older kernels accept it and do nothing
        let _ = fadvise(&file, 0, None, Advice::NoReuse);
    }
    Ok(file)
}

/// The machine's memory in bytes, where the OS tells it.
#[cfg(target_os = "linux")]
#[allow(
    clippy::useless_conversion,
    reason = "`totalram` is a C `unsigned long`, 32 bits on 32-bit systems"
)]
fn memory() -> Option<u64> {
    use rustix::system::sysinfo;

    let info = sysinfo();
    u64::from(info.totalram).checked_mul(u64::from(info.mem_unit))
}

#[cfg(not(target_os = "linux"))]
fn memory() -> Option<u64> {
    None
}

/// Drops `len` bytes of the file at `offset` from the page cache, where the OS allows it.
#[cfg(target_os = "linux")]
fn drop_cached(file: &File, offset: u64, len: u64) {
    use std::num::NonZeroU64;

    use rustix::fs::{Advice, fadvise};

    // only advice: the pages are read from the disk again if they are needed
    let _ = fadvise(file, offset, NonZeroU64::new(len), Advice::DontNeed);
}

#[cfg(not(target_os = "linux"))]
fn drop_cached(_file: &File, _offset: u64, _len: u64) {}

impl BlobReader {
    pub(crate) fn open(path: &Path) -> Result<Self> {
        let file = open_once(path)?;
        let file_len = file.metadata()?.len();
        Ok(Self {
            file_len,
            file,
            buf: Vec::new(),
            buf_start: 0,
            chunk: READ_BUFFER,
            position: BlobPosition { blob: 0, offset: 0 },
            done: false,
            dropped: memory()
                .is_some_and(|m| file_len > m / CACHED_SHARE)
                .then_some(0),
        })
    }

    /// Drops the file from the page cache up to `DROP_LAG` behind the reader, once that is `DROP_STEP` past where it
    /// was dropped to: a file larger than half the memory would otherwise push out of it what other code keeps there,
    /// such as a node store, to keep pages that are not read again.
    fn drop_read(&mut self) {
        let Some(dropped) = self.dropped else {
            return;
        };
        let until = self.position.offset.saturating_sub(DROP_LAG);
        if until >= dropped + DROP_STEP {
            drop_cached(&self.file, dropped, until - dropped);
            self.dropped = Some(until);
        }
    }

    fn error(&self, kind: PbfFormatError) -> PbfError {
        PbfError::format(self.position, kind)
    }

    /// Appends up to `len` bytes of the file at `offset` to `out`, without zeroing them first.
    fn read_into(&mut self, offset: u64, len: usize, out: &mut Vec<u8>) -> Result<()> {
        self.file.seek(SeekFrom::Start(offset))?;
        (&self.file).take(len as u64).read_to_end(out)?;
        Ok(())
    }

    /// The bytes of the buffer from `offset` on, if the buffer holds `offset`.
    fn buffered(&self, offset: u64) -> Option<&[u8]> {
        let start = usize::try_from(offset.checked_sub(self.buf_start)?).ok()?;
        self.buf.get(start..)
    }

    /// The `len` bytes of the file at `offset`, reading `chunk` bytes from there (at least `len`) if they are not
    /// buffered.
    fn bytes(&mut self, offset: u64, len: usize, chunk: usize) -> Result<&[u8]> {
        if self.buffered(offset).is_none_or(|b| b.len() < len) {
            let mut buf = mem::take(&mut self.buf);
            buf.clear();
            self.read_into(offset, chunk.max(len), &mut buf)?;
            (self.buf, self.buf_start) = (buf, offset);
        }
        match self.buffered(offset).and_then(|b| b.get(..len)) {
            Some(bytes) => Ok(bytes),
            None => Err(self.error(PbfFormatError::Truncated)),
        }
    }

    /// The `len` bytes of the file at `offset`, as a new buffer: the part buffered copied, and the rest read straight
    /// into it.
    fn take(&mut self, offset: u64, len: usize) -> Result<Vec<u8>> {
        let mut out = Vec::with_capacity(len);
        out.extend_from_slice(
            self.buffered(offset)
                .map_or(&[][..], |b| &b[..b.len().min(len)]),
        );
        if out.len() < len {
            self.read_into(offset + out.len() as u64, len - out.len(), &mut out)?;
        }
        if out.len() == len {
            Ok(out)
        } else {
            Err(self.error(PbfFormatError::Truncated))
        }
    }

    /// The next blob, with as much of its message read as `want` asks for its kind and size, the rest skipped.
    fn read_blob(
        &mut self,
        want: impl FnOnce(&BlobKind, usize) -> Want,
    ) -> Result<Option<RawBlob>> {
        let offset = self.position.offset;
        if offset >= self.file_len {
            return Ok(None);
        }
        let chunk = self.chunk;
        let len: [u8; 4] = self.bytes(offset, 4, chunk)?.try_into().unwrap_or_default();
        let header_len = u32::from_be_bytes(len);
        if header_len > MAX_HEADER {
            return Err(self.error(PbfFormatError::BlobHeaderTooLarge(header_len)));
        }
        let header = self.bytes(offset + 4, header_len as usize, chunk)?;
        let (kind, len) = parse_header(header).map_err(|kind| self.error(kind))?;
        let start = offset + 4 + u64::from(header_len);
        let end = start + len as u64;
        if end > self.file_len {
            return Err(self.error(PbfFormatError::Truncated));
        }
        let want = want(&kind, len);
        self.chunk = want.chunk;
        let data = self.take(start, want.start.min(len))?;
        let position = self.position;
        self.position = BlobPosition {
            blob: position.blob + 1,
            offset: end,
        };
        self.drop_read();
        Ok(Some(RawBlob {
            position,
            kind,
            raw_size: layout(&data).map(|l| l.raw_size),
            span: Span {
                start,
                len,
                data: Arc::new(data),
            },
            next: None,
        }))
    }

    /// The next blob, read as `want` asks.
    pub(crate) fn next_with(
        &mut self,
        want: impl FnOnce(&BlobKind, usize) -> Want,
    ) -> Option<Result<RawBlob>> {
        if self.done {
            return None;
        }
        let blob = self.read_blob(want).transpose();
        self.done = !matches!(blob, Some(Ok(_)));
        blob
    }
}

/// A blob header's kind and the size of the blob after it.
fn parse_header(header: &[u8]) -> result::Result<(BlobKind, usize), PbfFormatError> {
    let view =
        BlobHeaderView::decode_view(header).map_err(PbfFormatError::protobuf("blob header"))?;
    let size = view.datasize.unwrap_or(0);
    let len = usize::try_from(size)
        .ok()
        .filter(|s| (1..=MAX_BLOB).contains(s))
        .ok_or(PbfFormatError::BlobSize(size))?;
    let kind = match view.r#type.unwrap_or_default() {
        b"OSMHeader" => BlobKind::Header,
        b"OSMData" => BlobKind::Data,
        other => BlobKind::Other(String::from_utf8_lossy(other).into_owned()),
    };
    Ok((kind, len))
}

/// The data blobs of a read, each read as its [`Loader`] wants, and each with the start of the next.
pub(crate) struct DataBlobs {
    reader: BlobReader,
    loader: Arc<Loader>,
    ahead: Option<Result<RawBlob>>,
}

impl DataBlobs {
    pub(crate) fn new(path: &Path, loader: Arc<Loader>) -> Result<Self> {
        let mut blobs = Self {
            reader: BlobReader::open(path)?,
            loader,
            ahead: None,
        };
        blobs.ahead = blobs.read();
        Ok(blobs)
    }

    /// The next data blob read from the file, skipping the others unread.
    fn read(&mut self) -> Option<Result<RawBlob>> {
        loop {
            let loader = &self.loader;
            let blob = self.reader.next_with(|kind, len| match kind {
                BlobKind::Data => loader.want(len),
                BlobKind::Header | BlobKind::Other(_) => Want {
                    start: 0,
                    ..loader.want(len)
                },
            });
            if !matches!(&blob, Some(Ok(b)) if b.kind != BlobKind::Data) {
                return blob;
            }
        }
    }
}

impl Iterator for DataBlobs {
    type Item = Result<RawBlob>;

    fn next(&mut self) -> Option<Self::Item> {
        let mut current = match self.ahead.take()? {
            Ok(blob) => blob,
            Err(e) => return Some(Err(e)),
        };
        self.ahead = self.read();
        if let Some(Ok(next)) = &self.ahead {
            current.next = Some(next.span.clone());
        }
        Some(Ok(current))
    }
}

/// How a read loads its blocks: shared by the threads that load them.
#[derive(Debug)]
pub(crate) struct Loader {
    /// The file, for reading the rest of a blob read only in part.
    pub(crate) file: File,
    pub(crate) select: Fields,
    pub(crate) filters: Filters,
    /// The file declares `Sort.Type_then_ID`: no object type comes back after a later one.
    pub(crate) sorted: bool,
    pub(crate) mode: ReadMode,
    /// Whether recent blocks were kept (positive) or skipped (negative), between -8 and 8.
    pub(crate) kept: AtomicI8,
}

/// Reads into `buf` at `offset`, as `pread` does, so threads share one file. Windows also moves the file's position,
/// which no reader of this file uses: the blob reader reads through a file of its own.
fn read_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileExt as _;
        file.read_at(buf, offset)
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileExt as _;
        file.seek_read(buf, offset)
    }
}

/// Reads as much of `buf` as the file has at `offset`, and returns how much: less only at the end of the file.
fn read_full_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    let mut done = 0;
    while done < buf.len() {
        match read_at(file, &mut buf[done..], offset + done as u64) {
            Ok(0) => break,
            Ok(n) => done += n,
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(done)
}

/// Where a `Blob` message's data is, from its first bytes.
struct Layout {
    raw_size: usize,
    /// The data's range in the message.
    data: (usize, usize),
    /// Whether the data is zlib data, or the block itself.
    zlib: bool,
}

/// Reads the start of a `Blob` message: its declared size, then its data's place. `None` when it is anything else
/// (another compression, the size after zlib data, a size that does not match raw data), which is then read whole
/// and checked.
fn layout(blob: &[u8]) -> Option<Layout> {
    let mut p = 0;
    let mut raw_size = None;
    loop {
        let key = varint(blob, &mut p)?;
        match key {
            0x10 => {
                raw_size = usize::try_from(varint(blob, &mut p)?)
                    .ok()
                    .filter(|&s| s <= MAX_BLOB);
            }
            0x0a | 0x1a => {
                let len = usize::try_from(varint(blob, &mut p)?).ok()?;
                let zlib = key == 0x1a;
                let raw_size = if zlib {
                    raw_size?
                } else {
                    raw_size.unwrap_or(len)
                };
                if !zlib && raw_size != len {
                    return None;
                }
                return Some(Layout {
                    raw_size,
                    data: (p, p.checked_add(len)?),
                    zlib,
                });
            }
            _ => return None,
        }
    }
}

/// The bytes `d` has inflated, which fit a blob's limit of 32 MiB.
fn total_out(d: &Decompress) -> usize {
    usize::try_from(d.total_out()).unwrap_or(usize::MAX)
}

/// The bytes `d` has consumed, which fit a blob's limit of 32 MiB.
fn total_in(d: &Decompress) -> usize {
    usize::try_from(d.total_in()).unwrap_or(usize::MAX)
}

/// Inflates the start of the zlib block in `blob`, a `Blob` message or its first bytes laid out as `layout`, into
/// `out` with `d`, until its first group shows or `blob` runs out.
fn peek_start(
    blob: &[u8],
    layout: &Layout,
    d: &mut Decompress,
    out: &mut Vec<u8>,
) -> FResult<Peek> {
    let available = &blob[layout.data.0.min(blob.len())..layout.data.1.min(blob.len())];
    d.reset(true);
    out.clear();
    let mut target = 4096;
    loop {
        let produced = total_out(d);
        let target_now = target.min(layout.raw_size).max(produced);
        out.resize(target_now, 0);
        let status = d
            .decompress(
                &available[total_in(d)..],
                &mut out[produced..],
                FlushDecompress::None,
            )
            .map_err(|e| PbfFormatError::Zlib(e.to_string()))?;
        let produced = total_out(d);
        out.truncate(produced);
        match peek(out, layout.raw_size) {
            Peek::NeedMore(need) if produced == target_now && status != Status::StreamEnd => {
                target = need.max(produced * 2);
            }
            found @ (Peek::Found { .. } | Peek::NeedMore(_) | Peek::Unknown) => return Ok(found),
        }
    }
}

/// The zlib layout of a `Blob` message's start; `None` for raw data or anything unusual, which is read whole.
fn zlib_layout(blob: &[u8]) -> Option<Layout> {
    layout(blob).filter(|l| l.zlib)
}

impl Loader {
    /// How to read a data blob of `len` bytes: whole, or a few bytes of its start while most recent blocks were
    /// skipped and it is large. The worker loading it reads more of its start to peek at, and the rest only if its
    /// block is kept.
    fn want(&self, len: usize) -> Want {
        let start_only = self.skips()
            && match self.mode {
                ReadMode::Sequential => false,
                ReadMode::Selective => true,
                ReadMode::Auto => len > SMALL_BLOB && self.kept.load(Ordering::Relaxed) < 0,
            };
        if start_only {
            Want {
                start: HEADER_ROOM,
                chunk: 2 * HEADER_ROOM,
            }
        } else {
            Want::WHOLE
        }
    }

    /// Whether some blocks may be skipped whole: there are filters, or object types not selected.
    fn skips(&self) -> bool {
        let s = self.select;
        !(self.filters.is_empty() && s.nodes && s.ways && s.relations)
    }

    /// Notes whether a block was kept, for [`want`](Self::want). Threads may overwrite each other's notes, which
    /// only blurs a guess.
    fn note(&self, kept: bool) {
        let step = if kept { 1 } else { -1 };
        let k = self.kept.load(Ordering::Relaxed);
        self.kept.store((k + step).clamp(-8, 8), Ordering::Relaxed);
    }

    /// The first `want` bytes (at most all) of the message of `span`, of which `have` holds the first bytes: `have`
    /// itself if it holds them, otherwise with the rest read.
    fn more<'a>(
        &self,
        position: BlobPosition,
        span: &Span,
        have: Cow<'a, [u8]>,
        want: usize,
    ) -> Result<Cow<'a, [u8]>> {
        let want = want.min(span.len);
        if have.len() >= want {
            return Ok(have);
        }
        let mut all = vec![0; want];
        all[..have.len()].copy_from_slice(&have);
        if read_full_at(
            &self.file,
            &mut all[have.len()..],
            span.start + have.len() as u64,
        )? < want - have.len()
        {
            return Err(PbfError::format(position, PbfFormatError::Truncated));
        }
        Ok(Cow::Owned(all))
    }

    /// Peeks at the block of the zlib `Blob` message of `span`, laid out as `layout`, with `d` inflating into `out`:
    /// from the bytes read, then reading [`PEEK_READ`] bytes, then twice as much each time, while that is too little.
    /// Returns the peek and the bytes read.
    fn peek<'a>(
        &self,
        position: BlobPosition,
        span: &'a Span,
        layout: &Layout,
        d: &mut Decompress,
        out: &mut Vec<u8>,
    ) -> Result<(Peek, Cow<'a, [u8]>)> {
        let mut have = Cow::Borrowed(&span.data[..]);
        loop {
            match peek_start(&have, layout, d, out)
                .map_err(|kind| PbfError::format(position, kind))?
            {
                Peek::NeedMore(_) if have.len() < span.len => {
                    let want = (2 * have.len()).max(PEEK_READ);
                    have = self.more(position, span, have, want)?;
                }
                found @ (Peek::Found { .. } | Peek::NeedMore(_) | Peek::Unknown) => {
                    return Ok((found, have));
                }
            }
        }
    }

    /// Reads the rest of the data blob `blob` as needed, inflates and decodes it; `None` when it is skipped whole.
    pub(crate) fn load(&self, blob: &RawBlob) -> Result<Option<Block>> {
        let err = |kind| PbfError::format(blob.position, kind);
        if self.skips()
            && let Some(layout) = zlib_layout(&blob.span.data)
        {
            let decided = ZLIB.with_borrow_mut(|(d, out)| -> Result<Option<Option<Block>>> {
                let (peek, have) = self.peek(blob.position, &blob.span, &layout, d, out)?;
                let Peek::Found {
                    strings,
                    kind,
                    alone,
                } = peek
                else {
                    return Ok(None);
                };
                let strings: Vec<&[u8]> = strings.iter().map(|r| &out[r.clone()]).collect();
                let select = self.filters.narrow(self.select, &strings);
                let first_kept = match kind {
                    Kind::Nodes => select.nodes,
                    Kind::Ways => select.ways,
                    Kind::Relations => select.relations,
                    Kind::Other => true,
                };
                let none_left = !(select.nodes || select.ways || select.relations);
                if none_left || (!first_kept && (alone || self.next_is_same(blob, kind))) {
                    self.note(false);
                    return Ok(Some(None));
                }
                self.note(true);
                let all = self.more(blob.position, &blob.span, have, blob.span.len)?;
                inflate(
                    d,
                    &all[layout.data.0 + total_in(d)..layout.data.1],
                    out,
                    layout.raw_size,
                )
                .map_err(err)?;
                Block::decode(blob.position, blob.span.len, true, out, select, None)
                    .map(|b| Some(Some(b)))
            })?;
            if let Some(block) = decided {
                return Ok(block);
            }
        }
        // Read whole: no blocks skipped, raw data, or a start not laid out as expected.
        let span = &blob.span;
        let all = self.more(blob.position, span, Cow::Borrowed(&span.data), span.len)?;
        let filters = Some(&self.filters).filter(|f| !f.is_empty());
        let decoded = with_content(&all, |data, zlib| {
            Block::decode(blob.position, span.len, zlib, data, self.select, filters)
        });
        decoded.map_err(err)?.map(Some)
    }

    /// Whether the file is sorted and `blob` holds only objects of `kind`: its kind is the last one, or the next data
    /// blob starts with it.
    fn next_is_same(&self, blob: &RawBlob, kind: Kind) -> bool {
        if !self.sorted || kind == Kind::Other {
            return false;
        }
        if kind == Kind::Relations {
            return true;
        }
        let Some((next, layout)) = blob
            .next
            .as_ref()
            .and_then(|n| Some((n, zlib_layout(&n.data)?)))
        else {
            return false;
        };
        NEXT.with_borrow_mut(|(d, out)| {
            let peek = self.peek(blob.position, next, &layout, d, out);
            matches!(peek, Ok((Peek::Found { kind: k, .. }, _)) if k == kind)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{PbfReader, filter};

    #[test]
    fn a_large_file_is_dropped_behind_the_reader() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("fixtures/osm-io/history-niue-230109.osm.pbf");
        let all = |dropped| {
            let mut reader = BlobReader::open(&path).unwrap();
            reader.dropped = dropped;
            let mut blobs = Vec::new();
            while let Some(blob) = reader.next_with(|_, _| Want::WHOLE) {
                blobs.push(blob.unwrap().span.data);
            }
            (blobs, reader.dropped, reader.file_len)
        };
        let (kept, none, _) = all(None);
        let (dropped, until, len) = all(Some(0));
        assert_eq!(kept, dropped, "the blobs read are the same");
        assert_eq!(none, None);
        let until = until.unwrap();
        assert!(
            until + DROP_LAG + DROP_STEP > len && until + DROP_LAG <= len,
            "dropped to {until} of {len}"
        );
    }

    /// A loader of a generated fixture, rejecting every way (or every node), and its data blobs as read in `mode`.
    fn read(name: &str, mode: ReadMode, reject_nodes: bool) -> (Arc<Loader>, Vec<RawBlob>) {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("fixtures/generated")
            .join(name);
        let reader = PbfReader::open(&path).unwrap();
        let reject = Some(Arc::new(|_: &crate::BlockStrings<'_>| false) as filter::Filter);
        let filters = if reject_nodes {
            Filters {
                nodes: reject,
                ..Filters::default()
            }
        } else {
            Filters {
                ways: reject,
                ..Filters::default()
            }
        };
        let loader = Arc::new(Loader {
            file: File::open(&path).unwrap(),
            select: crate::Select::all().fields(),
            filters,
            sorted: reader.header().is_sorted(),
            mode,
            kept: AtomicI8::new(0),
        });
        let blobs = DataBlobs::new(&path, Arc::clone(&loader))
            .unwrap()
            .collect::<Result<_>>()
            .unwrap();
        (loader, blobs)
    }

    /// What loading each data blob gives: `None` for a blob skipped whole, or its numbers of nodes, ways and
    /// relations.
    fn decisions(
        name: &str,
        mode: ReadMode,
        reject_nodes: bool,
    ) -> Vec<Option<(usize, usize, usize)>> {
        let (loader, blobs) = read(name, mode, reject_nodes);
        blobs
            .iter()
            .map(|blob| {
                let block = loader.load(blob).unwrap();
                block.map(|b| (b.nodes().len(), b.ways().len(), b.relations().len()))
            })
            .collect()
    }

    const MODES: [ReadMode; 3] = [ReadMode::Auto, ReadMode::Sequential, ReadMode::Selective];

    #[test]
    fn a_block_of_one_group_is_skipped() {
        // Nodes, then ways, then relations, one group per block, as osmium writes them.
        for mode in MODES {
            let expected = [Some((3, 0, 0)), None, Some((0, 0, 1))];
            assert_eq!(decisions("packed_zlib.osm.pbf", mode, false), expected);
        }
    }

    #[test]
    fn a_sorted_block_followed_by_its_type_is_skipped() {
        // The first ways block is followed by another, so it holds only ways; the second is followed by relations,
        // so it might end with some, and is read whole without its ways.
        let expected = [Some((4, 0, 0)), None, Some((0, 0, 0)), Some((0, 0, 2))];
        assert_eq!(
            decisions("sorted_groups_zlib.osm.pbf", ReadMode::Auto, false),
            expected
        );
    }

    #[test]
    fn a_sorted_block_changing_type_is_read() {
        let expected = [Some((3, 0, 0)), Some((0, 0, 1)), Some((0, 0, 1))];
        assert_eq!(
            decisions("sorted_boundary_zlib.osm.pbf", ReadMode::Auto, false),
            expected
        );
    }

    #[test]
    fn an_unsorted_block_of_several_groups_is_read() {
        assert_eq!(
            decisions("mixed_groups_zlib.osm.pbf", ReadMode::Auto, false),
            [Some((4, 0, 1))]
        );
    }

    #[test]
    fn a_raw_block_is_read_whole() {
        let expected = [Some((3, 0, 0)), Some((0, 0, 0)), Some((0, 0, 1))];
        assert_eq!(
            decisions("packed.osm.pbf", ReadMode::Selective, false),
            expected
        );
    }

    #[test]
    fn large_blocks_are_skipped_after_reading_their_start() {
        let name = "large_blocks_zlib.osm.pbf";
        for mode in MODES {
            // The small ways block has a single group, so it is skipped too.
            let expected = [
                Some((4, 0, 0)),
                Some((1, 0, 0)),
                None,
                None,
                Some((0, 0, 1)),
            ];
            assert_eq!(decisions(name, mode, false), expected, "{mode:?}");
        }
        // The nodes blocks and the large ways block are larger than the start read first, while starts are read.
        let partial = |mode| {
            read(name, mode, false)
                .1
                .iter()
                .map(|b| b.span.data.len() < b.span.len)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            partial(ReadMode::Selective),
            [true, true, true, false, false]
        );
        assert_eq!(partial(ReadMode::Sequential), [false; 5]);
        // A kept block read in part has the rest of it read from the file.
        let expected = [
            None,
            None,
            Some((0, 4000, 0)),
            Some((0, 1, 0)),
            Some((0, 0, 1)),
        ];
        assert_eq!(decisions(name, ReadMode::Selective, true), expected);
    }

    /// The IDs of a loaded block's objects; none for a skipped one.
    fn ids(block: Option<Block>) -> Vec<i64> {
        block.map_or_else(Vec::new, |b| {
            let nodes = b.nodes().map(|n| n.id());
            let ways = b.ways().map(|w| w.id());
            nodes
                .chain(ways)
                .chain(b.relations().map(|r| r.id()))
                .collect()
        })
    }

    #[test]
    fn a_blob_read_in_part_loads_as_a_whole_one() {
        for name in [
            "packed_zlib.osm.pbf",
            "sorted_groups_zlib.osm.pbf",
            "sorted_boundary_zlib.osm.pbf",
            "mixed_groups_zlib.osm.pbf",
        ] {
            let (loader, blobs) = read(name, ReadMode::Auto, false);
            for blob in &blobs {
                let expected = ids(loader.load(blob).unwrap());
                // From too short to tell the block's type to most of the blob: the rest is read from the file. The
                // next blob's start is cut the same way, which can only keep a block that could be skipped.
                let cut_at = |span: &Span, cut: usize| Span {
                    data: Arc::new(span.data[..cut.min(span.data.len())].to_vec()),
                    ..span.clone()
                };
                for cut in [1, 12, 40, blob.span.len / 2, blob.span.len - 1] {
                    let part = RawBlob {
                        position: blob.position,
                        kind: blob.kind.clone(),
                        span: cut_at(&blob.span, cut),
                        raw_size: blob.raw_size,
                        next: blob.next.as_ref().map(|n| cut_at(n, cut)),
                    };
                    let got = ids(loader.load(&part).unwrap());
                    let rejected: Vec<i64> = expected
                        .iter()
                        .copied()
                        .filter(|&id| (10..20).contains(&id))
                        .collect();
                    assert!(
                        got == expected
                            || (expected.is_empty() && got.iter().all(|id| !rejected.contains(id))),
                        "{name} {:?} cut at {cut}: {got:?}",
                        blob.position
                    );
                }
            }
        }
    }
}
