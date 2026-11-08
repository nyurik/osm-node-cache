//! Reading the nodes and ways of a sorted OSM PBF file in order: one thread reads the blobs, and
//! the others decompress and decode them at the same time, with a decoder of just the fields a node
//! store needs: node IDs and locations, and the node IDs of ways.

use std::cell::RefCell;
use std::fs::File;
use std::io::{self, BufReader, Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, SyncSender};
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, ensure, Context as _};
use rayon::prelude::*;

/// Blobs decoded together; each holds about 8000 nodes or ways.
const BATCH: usize = 256;
/// Bytes read from the file at once
const READ_BUFFER: usize = 8 << 20;
/// How often to report how much of the file has been read
const PROGRESS: Duration = Duration::from_secs(30);

/// Nodes above this ID, and way refs to them, are left out, to measure a store that fits in memory.
static MAX_ID: OnceLock<u64> = OnceLock::new();
/// Whether to read PBF files with direct I/O
static DIRECT: AtomicBool = AtomicBool::new(false);
/// Whether to decompress with zlib-rs, which decompressed North America's node blobs 12% faster
/// than zlib-ng on Graviton4
static ZLIB_RS: AtomicBool = AtomicBool::new(true);

/// Decompresses PBF blobs with zlib-ng from here on, rather than with zlib-rs.
pub fn set_zlib_ng() {
    ZLIB_RS.store(false, Ordering::Relaxed);
}

/// Leaves out nodes above `id` from here on.
pub fn set_max_id(id: u64) {
    MAX_ID.set(id).expect("the node ID limit is set once");
}

fn max_id() -> u64 {
    MAX_ID.get().copied().unwrap_or(u64::MAX)
}

/// Reads PBF files with direct I/O from here on, bypassing the page cache, on Linux: a file read
/// once does not push the pages of a store out of memory, and on a network disk, big direct reads
/// are 3 to 4 times faster than reads through the page cache with the OS's usual 128 KiB of
/// read-ahead.
pub fn set_direct() {
    DIRECT.store(true, Ordering::Relaxed);
}

/// A PBF file to read in order.
trait Input: Read + Seek + Send {}
impl<T: Read + Seek + Send> Input for T {}

/// Opens a PBF file to read, with direct I/O if [`set_direct`] was called.
fn open(path: &Path) -> anyhow::Result<Box<dyn Input>> {
    #[cfg(target_os = "linux")]
    if DIRECT.load(Ordering::Relaxed) {
        return Ok(Box::new(Direct::open(path)?));
    }
    let file = File::open(path).with_context(|| path.display().to_string())?;
    Ok(Box::new(BufReader::with_capacity(READ_BUFFER, file)))
}

#[derive(Clone, Copy)]
pub struct Node {
    pub id: u64,
    pub lat: i32,
    pub lon: i32,
}

/// The node lists of a blob's ways, in one buffer
#[derive(Default)]
pub struct Ways {
    refs: Vec<i64>,
    /// where each way's refs end
    ends: Vec<usize>,
}

impl Ways {
    /// The node refs of each way.
    pub fn iter(&self) -> impl Iterator<Item = &[i64]> {
        let mut start = 0;
        self.ends.iter().map(move |&end| {
            let refs = &self.refs[start..end];
            start = end;
            refs
        })
    }

    pub fn len(&self) -> usize {
        self.ends.len()
    }

    pub fn refs(&self) -> usize {
        self.refs.len()
    }
}

/// Calls `f` with the nodes of each blob, in file order (by ID in a sorted file). Returns where
/// the blobs after the nodes start, if the file has any.
pub fn for_each_node(
    path: &Path,
    mut f: impl FnMut(&[Node]) -> anyhow::Result<()>,
) -> anyhow::Result<Option<u64>> {
    let max = max_id();
    let mut past_max = false;
    let res = for_each_blob(path, 0, decode_nodes, true, |nodes| {
        let n = nodes.partition_point(|n| n.id <= max);
        if n > 0 {
            f(&nodes[..n])?;
        }
        // an error stops reading the file
        past_max = n < nodes.len();
        ensure!(!past_max, "past the node ID limit");
        Ok(())
    });
    match res {
        Err(_) if past_max => Ok(None),
        res => res,
    }
}

/// Calls `f` with the ways of each blob from offset `start` on, in file order.
pub fn for_each_way(
    path: &Path,
    start: u64,
    f: impl FnMut(Ways) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    for_each_blob(path, start, decode_ways, false, f).map(drop)
}

/// Calls `f` with what `map` returns for the ways of each blob from offset `start` on, in file
/// order. `map` runs on the threads that decode the blobs.
pub fn map_ways<T: Send>(
    path: &Path,
    start: u64,
    map: impl Fn(Ways) -> T + Sync,
    f: impl FnMut(T) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    for_each_blob(path, start, |blob| decode_ways(blob).map(&map), false, f).map(drop)
}

/// A blob of OSM data as read from the file, still compressed
struct RawBlob {
    offset: u64,
    data: Vec<u8>,
}

thread_local! {
    /// Each thread's buffer for decompressed blobs
    static INFLATED: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

/// Decompresses a blob, and decodes its primitive block with `decode`.
fn decode_blob<T>(blob: &RawBlob, decode: impl Fn(&[u8]) -> T) -> T {
    let mut buf = INFLATED.take();
    inflate(&blob.data, &mut buf)
        .with_context(|| format!("the blob at {}", blob.offset))
        .expect("invalid PBF blob");
    let res = decode(&buf);
    INFLATED.set(buf);
    res
}

/// Calls `f` with each blob from offset `start` that `decode` returns something for. A sorted
/// file has all nodes first, so with `nodes_only` reading stops at the first blob without nodes,
/// and returns its offset.
///
/// One thread reads the file, another has the blobs it read decoded on all threads, and this one
/// calls `f`, all at the same time.
fn for_each_blob<T: Send>(
    path: &Path,
    start: u64,
    decode: impl Fn(&[u8]) -> Option<T> + Sync,
    nodes_only: bool,
    f: impl FnMut(T) -> anyhow::Result<()>,
) -> anyhow::Result<Option<u64>> {
    let stop = AtomicBool::new(false);
    let (raw_tx, raw_rx) = sync_channel::<Vec<RawBlob>>(2);
    let (tx, rx) = sync_channel::<Vec<Option<T>>>(4);
    let (decode, stop) = (&decode, &stop);
    thread::scope(|s| {
        let reader = s.spawn(move || read_blobs(path, start, &raw_tx, stop));
        // owns `tx`, so the channel closes when it is done
        let decoder = s.spawn(move || -> Option<u64> {
            for batch in raw_rx {
                let decoded: Vec<Option<T>> =
                    batch.par_iter().map(|b| decode_blob(b, decode)).collect();
                // the first blob without nodes
                let end = nodes_only
                    .then(|| decoded.iter().position(Option::is_none))
                    .flatten()
                    .map(|i| batch[i].offset);
                if tx.send(decoded).is_err() || end.is_some() {
                    stop.store(true, Ordering::Relaxed);
                    return end;
                }
            }
            None
        });
        // if `f` fails, dropping `rx` stops the others too
        let res = rx.into_iter().flatten().flatten().try_for_each(f);
        let end = decoder.join().expect("PBF decoder thread panicked");
        reader.join().expect("PBF reader thread panicked")?;
        res.map(|()| end)
    })
}

/// Reads the file's blobs of OSM data from offset `start` on, sending them in batches, until the
/// file ends, the receiver hangs up, or `stop` is set.
fn read_blobs(
    path: &Path,
    start: u64,
    tx: &SyncSender<Vec<RawBlob>>,
    stop: &AtomicBool,
) -> anyhow::Result<()> {
    let size = std::fs::metadata(path)?.len();
    let mut r = open(path)?;
    r.seek(SeekFrom::Start(start))?;
    let (mut offset, begin, mut reported) = (start, Instant::now(), Instant::now());
    let mut batch = Vec::with_capacity(BATCH);
    while !stop.load(Ordering::Relaxed) {
        let mut len = [0; 4];
        match r.read_exact(&mut len) {
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
            res => res?,
        }
        let mut header = vec![0; u32::from_be_bytes(len) as usize];
        r.read_exact(&mut header)?;
        let (kind, data_len) = blob_header(&header)?;
        let mut data = vec![0; data_len];
        r.read_exact(&mut data)?;
        if kind == b"OSMData" {
            batch.push(RawBlob { offset, data });
        }
        offset += 4 + header.len() as u64 + data_len as u64;
        if batch.len() == BATCH {
            if reported.elapsed() >= PROGRESS {
                reported = Instant::now();
                eprintln!(
                    "  {:.1}% of the file read in {:.0} s",
                    100.0 * offset as f64 / size as f64,
                    begin.elapsed().as_secs_f64()
                );
            }
            if tx.send(std::mem::take(&mut batch)).is_err() {
                return Ok(());
            }
            batch.reserve(BATCH);
        }
    }
    let _ = tx.send(batch);
    Ok(())
}

/// Reads the file's blobs of OSM data from offset `start` to `end`, or to the end of the file,
/// without decompressing them. Returns the bytes of blob data read.
pub fn read_only(path: &Path, start: u64, end: Option<u64>) -> anyhow::Result<u64> {
    let stop = &AtomicBool::new(false);
    let (tx, rx) = sync_channel::<Vec<RawBlob>>(2);
    let mut bytes = 0;
    thread::scope(|s| {
        // owns `tx`, so the channel closes when it is done
        let reader = s.spawn(move || read_blobs(path, start, &tx, stop));
        for batch in rx {
            bytes += batch.iter().map(|b| b.data.len() as u64).sum::<u64>();
            if end.is_some_and(|end| batch.last().is_some_and(|b| b.offset >= end)) {
                stop.store(true, Ordering::Relaxed);
                break;
            }
        }
        reader.join().expect("PBF reader thread panicked")
    })?;
    Ok(bytes)
}

/// Decompresses the file's blobs of nodes on all threads, without decoding them. Returns the bytes
/// decompressed.
pub fn inflate_nodes(path: &Path) -> anyhow::Result<u64> {
    let mut bytes = 0;
    // a blob of nodes has dense nodes, the second field of a group, near its start
    for_each_blob(
        path,
        0,
        |b| {
            let first_group = Block::new(b).ok()?.groups.first().copied()?;
            (first_group.first() == Some(&0x12)).then_some(b.len() as u64)
        },
        true,
        |n| {
            bytes += n;
            Ok(())
        },
    )?;
    Ok(bytes)
}

/// A file read in order with direct I/O, in big reads of [`READ_BUFFER`] bytes at aligned offsets
#[cfg(target_os = "linux")]
struct Direct {
    file: File,
    /// a buffer with room for an aligned one of [`READ_BUFFER`] bytes
    buf: Vec<u8>,
    /// where the aligned buffer starts in `buf`
    base: usize,
    /// the unread bytes of the aligned buffer
    start: usize,
    end: usize,
    /// the file offset of the aligned buffer's end
    offset: u64,
}

#[cfg(target_os = "linux")]
impl Direct {
    /// Direct I/O reads from aligned offsets into aligned memory, in multiples of this
    const ALIGN: usize = 4096;

    fn open(path: &Path) -> anyhow::Result<Self> {
        use std::os::unix::fs::OpenOptionsExt as _;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECT)
            .open(path)
            .with_context(|| format!("{} with direct I/O", path.display()))?;
        let buf = vec![0; READ_BUFFER + Self::ALIGN];
        let base = buf.as_ptr().align_offset(Self::ALIGN);
        Ok(Self {
            file,
            buf,
            base,
            start: 0,
            end: 0,
            offset: 0,
        })
    }

    /// Reads the next bytes of the file into the buffer; none at its end.
    fn fill(&mut self) -> io::Result<()> {
        use std::os::unix::fs::FileExt as _;
        let buf = &mut self.buf[self.base..self.base + READ_BUFFER];
        let mut n = 0;
        // Only the end of the file gives a short read. Reading on after it would read at an
        // unaligned offset, which direct I/O rejects, so a short read ends the buffer.
        while n < buf.len() {
            let want = buf.len() - n;
            let read = self.file.read_at(&mut buf[n..], self.offset + n as u64)?;
            n += read;
            if read < want {
                break;
            }
        }
        (self.start, self.end) = (0, n);
        self.offset += n as u64;
        Ok(())
    }
}

#[cfg(target_os = "linux")]
impl Read for Direct {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.start == self.end {
            self.fill()?;
        }
        let buf = &self.buf[self.base + self.start..self.base + self.end];
        let n = buf.len().min(out.len());
        out[..n].copy_from_slice(&buf[..n]);
        self.start += n;
        Ok(n)
    }
}

#[cfg(target_os = "linux")]
impl Seek for Direct {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let SeekFrom::Start(pos) = pos else {
            return Err(io::Error::other("only seeking from the start is supported"));
        };
        self.offset = pos - pos % Self::ALIGN as u64;
        self.fill()?;
        self.start = usize::try_from(pos % Self::ALIGN as u64)
            .unwrap()
            .min(self.end);
        Ok(pos)
    }
}

/// A protocol buffer message being read
struct Pb<'a> {
    b: &'a [u8],
    at: usize,
}

impl<'a> Pb<'a> {
    fn new(b: &'a [u8]) -> Self {
        Self { b, at: 0 }
    }

    #[inline]
    fn varint(&mut self) -> anyhow::Result<u64> {
        let mut v = 0_u64;
        for shift in (0..64).step_by(7) {
            let Some(&byte) = self.b.get(self.at) else {
                bail!("a varint past the end of a message");
            };
            self.at += 1;
            v |= u64::from(byte & 0x7f) << shift;
            if byte < 0x80 {
                return Ok(v);
            }
        }
        bail!("a varint longer than 10 bytes")
    }

    /// The next field's number and wire type, if the message has more.
    fn field(&mut self) -> anyhow::Result<Option<(u64, u64)>> {
        if self.at >= self.b.len() {
            return Ok(None);
        }
        let key = self.varint()?;
        Ok(Some((key >> 3, key & 7)))
    }

    fn bytes(&mut self) -> anyhow::Result<&'a [u8]> {
        let len = usize::try_from(self.varint()?)?;
        let b = self
            .b
            .get(self.at..self.at + len)
            .context("a field past the end of a message")?;
        self.at += len;
        Ok(b)
    }

    fn skip(&mut self, wire: u64) -> anyhow::Result<()> {
        match wire {
            0 => drop(self.varint()?),
            1 => self.at += 8,
            2 => drop(self.bytes()?),
            5 => self.at += 4,
            _ => bail!("unsupported wire type {wire}"),
        }
        Ok(())
    }
}

#[inline]
fn zigzag(v: u64) -> i64 {
    (v >> 1).cast_signed() ^ -(v & 1).cast_signed()
}

/// Calls `f` with each value of a packed field of delta-coded `sint64`, decoded.
#[inline]
fn deltas(b: &[u8], mut f: impl FnMut(i64)) -> anyhow::Result<()> {
    let (mut pb, mut v) = (Pb::new(b), 0_i64);
    while pb.at < b.len() {
        v += zigzag(pb.varint()?);
        f(v);
    }
    Ok(())
}

/// The type and data length of a blob header.
fn blob_header(b: &[u8]) -> anyhow::Result<(Vec<u8>, usize)> {
    let (mut pb, mut kind, mut len) = (Pb::new(b), Vec::new(), None);
    while let Some((field, wire)) = pb.field()? {
        match (field, wire) {
            (1, 2) => kind = pb.bytes()?.to_vec(),
            (3, 0) => len = Some(usize::try_from(pb.varint()?)?),
            _ => pb.skip(wire)?,
        }
    }
    Ok((kind, len.context("a blob header without a data size")?))
}

/// Decompresses a blob's data into `out`.
fn inflate(blob: &[u8], out: &mut Vec<u8>) -> anyhow::Result<()> {
    let (mut pb, mut raw, mut zlib, mut size) = (Pb::new(blob), None, None, 0);
    while let Some((field, wire)) = pb.field()? {
        match (field, wire) {
            (1, 2) => raw = Some(pb.bytes()?),
            (2, 0) => size = usize::try_from(pb.varint()?)?,
            (3, 2) => zlib = Some(pb.bytes()?),
            (4..=7, 2) => bail!("only zlib-compressed and uncompressed blobs are supported"),
            _ => pb.skip(wire)?,
        }
    }
    out.clear();
    if let Some(raw) = raw {
        out.extend_from_slice(raw);
        return Ok(());
    }
    let zlib = zlib.context("a blob without data")?;
    out.reserve(size);
    if ZLIB_RS.load(Ordering::Relaxed) {
        out.resize(size, 0);
        let (done, rc) = zlib_rs::decompress_slice(out, zlib, zlib_rs::InflateConfig::default());
        ensure!(
            rc == zlib_rs::ReturnCode::Ok,
            "zlib-rs cannot decompress a blob: {rc:?}"
        );
        let n = done.len();
        out.truncate(n);
    } else {
        let mut z = flate2::Decompress::new(true);
        z.decompress_vec(zlib, out, flate2::FlushDecompress::Finish)?;
    }
    ensure!(
        out.len() == size,
        "a blob of {} bytes, not {size}",
        out.len()
    );
    Ok(())
}

/// The groups of a primitive block, and how to turn its coordinates into 1e-7 degrees.
struct Block<'a> {
    groups: Vec<&'a [u8]>,
    granularity: i64,
    lat_offset: i64,
    lon_offset: i64,
}

impl<'a> Block<'a> {
    fn new(b: &'a [u8]) -> anyhow::Result<Self> {
        let mut block = Self {
            groups: Vec::new(),
            granularity: 100,
            lat_offset: 0,
            lon_offset: 0,
        };
        let mut pb = Pb::new(b);
        while let Some((field, wire)) = pb.field()? {
            match (field, wire) {
                (2, 2) => block.groups.push(pb.bytes()?),
                (17, 0) => block.granularity = pb.varint()?.cast_signed(),
                (19, 0) => block.lat_offset = pb.varint()?.cast_signed(),
                (20, 0) => block.lon_offset = pb.varint()?.cast_signed(),
                _ => pb.skip(wire)?,
            }
        }
        Ok(block)
    }

    #[inline]
    #[expect(clippy::cast_possible_truncation)]
    fn lat(&self, v: i64) -> i32 {
        ((self.lat_offset + self.granularity * v) / 100) as i32
    }

    #[inline]
    #[expect(clippy::cast_possible_truncation)]
    fn lon(&self, v: i64) -> i32 {
        ((self.lon_offset + self.granularity * v) / 100) as i32
    }
}

/// The nodes of a block, leaving out negative IDs, as in files an editor wrote.
fn block_nodes(b: &[u8]) -> anyhow::Result<Vec<Node>> {
    let block = Block::new(b)?;
    let mut nodes = Vec::new();
    for group in &block.groups {
        let mut pb = Pb::new(group);
        while let Some((field, wire)) = pb.field()? {
            match (field, wire) {
                (1, 2) => {
                    let (mut node, mut id, mut lat, mut lon) = (Pb::new(pb.bytes()?), 0, 0, 0);
                    while let Some((field, wire)) = node.field()? {
                        match (field, wire) {
                            (1, 0) => id = zigzag(node.varint()?),
                            (8, 0) => lat = zigzag(node.varint()?),
                            (9, 0) => lon = zigzag(node.varint()?),
                            _ => node.skip(wire)?,
                        }
                    }
                    if id >= 0 {
                        nodes.push(Node {
                            id: id.cast_unsigned(),
                            lat: block.lat(lat),
                            lon: block.lon(lon),
                        });
                    }
                }
                (2, 2) => dense_nodes(&block, pb.bytes()?, &mut nodes)?,
                _ => pb.skip(wire)?,
            }
        }
    }
    Ok(nodes)
}

fn dense_nodes(block: &Block, b: &[u8], nodes: &mut Vec<Node>) -> anyhow::Result<()> {
    let (mut pb, mut ids, mut lats, mut lons) = (Pb::new(b), &[][..], &[][..], &[][..]);
    while let Some((field, wire)) = pb.field()? {
        match (field, wire) {
            (1, 2) => ids = pb.bytes()?,
            (8, 2) => lats = pb.bytes()?,
            (9, 2) => lons = pb.bytes()?,
            _ => pb.skip(wire)?,
        }
    }
    let start = nodes.len();
    deltas(ids, |id| {
        nodes.push(Node {
            id: id.cast_unsigned(),
            lat: 0,
            lon: 0,
        });
    })?;
    let dense = &mut nodes[start..];
    coordinates(lats, dense, |n, v| n.lat = block.lat(v))?;
    coordinates(lons, dense, |n, v| n.lon = block.lon(v))?;
    // negative IDs, as in files an editor wrote, are left out
    if dense.iter().any(|n| n.id.cast_signed() < 0) {
        let kept: Vec<Node> = dense
            .iter()
            .filter(|n| n.id.cast_signed() >= 0)
            .copied()
            .collect();
        nodes.truncate(start);
        nodes.extend(kept);
    }
    Ok(())
}

/// Sets a coordinate of each node with `set`, from a packed field with one value per node.
#[inline]
fn coordinates(
    values: &[u8],
    nodes: &mut [Node],
    mut set: impl FnMut(&mut Node, i64),
) -> anyhow::Result<()> {
    let mut i = 0;
    deltas(values, |v| {
        if let Some(n) = nodes.get_mut(i) {
            set(n, v);
        }
        i += 1;
    })?;
    ensure!(
        i == nodes.len(),
        "{} dense nodes with {i} coordinates",
        nodes.len()
    );
    Ok(())
}

/// The ways of a block, leaving out refs above the node ID limit.
fn block_ways(b: &[u8]) -> anyhow::Result<Ways> {
    let block = Block::new(b)?;
    let max = i64::try_from(max_id()).unwrap_or(i64::MAX);
    let mut ways = Ways::default();
    for group in &block.groups {
        let mut pb = Pb::new(group);
        while let Some((field, wire)) = pb.field()? {
            if (field, wire) != (3, 2) {
                pb.skip(wire)?;
                continue;
            }
            let mut way = Pb::new(pb.bytes()?);
            while let Some((field, wire)) = way.field()? {
                if (field, wire) != (8, 2) {
                    way.skip(wire)?;
                    continue;
                }
                let start = ways.refs.len();
                deltas(way.bytes()?, |id| {
                    if id <= max {
                        ways.refs.push(id);
                    }
                })?;
                if ways.refs.len() > start {
                    ways.ends.push(ways.refs.len());
                }
            }
        }
    }
    Ok(ways)
}

/// A number to add up over nodes, to compare decoders.
pub fn checksum(n: &Node) -> u64 {
    n.id.wrapping_mul(0x9e37_79b9_7f4a_7c15)
        ^ u64::from(n.lat.cast_unsigned()) << 32
        ^ u64::from(n.lon.cast_unsigned())
}

fn decode_nodes(block: &[u8]) -> Option<Vec<Node>> {
    let nodes = block_nodes(block).expect("invalid PBF block");
    (!nodes.is_empty()).then_some(nodes)
}

fn decode_ways(block: &[u8]) -> Option<Ways> {
    let ways = block_ways(block).expect("invalid PBF block");
    (!ways.ends.is_empty()).then_some(ways)
}

/// Decodes the file's nodes with the `osmpbf` crate, as this tool did before, for comparison:
/// returns the node count, and a checksum of the nodes, to compare with [`checksum`].
pub fn osmpbf_nodes(path: &Path) -> anyhow::Result<(u64, u64)> {
    use osmpbf::{BlobDecode, BlobReader, BlobType};
    let mut reader = BlobReader::new(open(path)?);
    let (mut count, mut sum, mut batch) = (0, 0_u64, Vec::with_capacity(BATCH));
    loop {
        let blob = reader.next().transpose()?;
        let at_end = blob.is_none();
        if let Some(blob) = blob {
            if matches!(blob.get_type(), BlobType::OsmData) {
                batch.push(blob);
            }
            if batch.len() < BATCH {
                continue;
            }
        }
        let counts: Vec<Option<(u64, u64)>> = batch
            .par_iter()
            .map(|blob| {
                let BlobDecode::OsmData(block) = blob.decode().expect("invalid PBF blob") else {
                    return None;
                };
                let (mut n, mut sum) = (0, 0_u64);
                for g in block.groups() {
                    for (id, lat, lon) in g
                        .dense_nodes()
                        .map(|n| (n.id(), n.decimicro_lat(), n.decimicro_lon()))
                        .chain(
                            g.nodes()
                                .map(|n| (n.id(), n.decimicro_lat(), n.decimicro_lon())),
                        )
                    {
                        // negative IDs, as in files an editor wrote, are left out, as the decoder does
                        if id >= 0 {
                            n += 1;
                            sum = sum.wrapping_add(checksum(&Node {
                                id: id.cast_unsigned(),
                                lat,
                                lon,
                            }));
                        }
                    }
                }
                (n > 0).then_some((n, sum))
            })
            .collect();
        for &(n, s) in counts.iter().map_while(Option::as_ref) {
            count += n;
            sum = sum.wrapping_add(s);
        }
        if counts.iter().any(Option::is_none) || at_end {
            return Ok((count, sum));
        }
        batch.clear();
    }
}
