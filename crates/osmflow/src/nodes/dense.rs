//! [`DenseStore`]: each node's location in 8 bytes at its ID's position in one memory-mapped file.
//!
//! The file is a header page holding two copies of the header, written in turn so that a crash
//! while writing one leaves the other, then 8 bytes per node ID: the latitude with its sign bit
//! flipped, then the longitude (little-endian `u32` each). Zero latitude bytes are an ID with no
//! node, as no valid latitude has them. IDs above the highest stored one take no space, as the
//! file ends there, and unwritten parts of it are sparse on most file systems.

use std::fs::File;
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::thread::{self, JoinHandle};

use memmap2::{Mmap, MmapMut, MmapOptions};

use super::codec::{u32_at, u64_at};
use super::file::{At as _, crc, map_file, open_ro, open_rw, read_at, sync_dir, write_at};
use super::store::{check, check_order, check_resumed};
use super::{
    BulkLoad, FileStore, FixReport, LoadState, MAX_NODE_ID, Metadata, Mode, NodeRead, NodeReader,
    NodeStore, NodeStoreError, ReadOnly, ReadWrite, Result,
};
use crate::Location;

const PAGE_SIZE: usize = 4096;
const SLOT: usize = PAGE_SIZE / 2;
/// Where a header copy's metadata starts; its CRC-32 is in the copy's last 4 bytes.
const METADATA_AT: usize = 44;
const MAGIC: &[u8; 8] = b"ONCDENS1";
const MAGIC_PREFIX: &[u8; 7] = b"ONCDENS";
const VERSION: u32 = 1;
/// The file grows by at least this much at a time.
const GROW_MIN: u64 = if cfg!(test) { 64 << 10 } else { 64 << 20 };
/// Nodes a bulk load writes between checkpoints: 1 GiB of the file.
const CHECKPOINT: u64 = if cfg!(test) { 1000 } else { 1 << 27 };
/// The node IDs whose values a bulk load writes at once: 8 MiB of the file.
const LOAD_IDS: u64 = if cfg!(test) { 1 << 12 } else { 1 << 20 };
/// The node IDs of a page of the file: a bulk load writes no gap between nodes this long, so that
/// the file system keeps no blocks for it
const PAGE_IDS: u64 = PAGE_SIZE as u64 / 8;

/// Options for [`DenseStore`]; it has none yet.
#[derive(Clone, Debug, Default)]
pub struct DenseOpts;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Header {
    generation: u64,
    stamp: u64,
    state: LoadState,
}

impl Header {
    /// A copy of the header: magic, version (u32), then at 16 the generation, stamp and last
    /// loaded ID (u64 each), at 40 whether a bulk load is unfinished (1) and has kept nodes (2), at
    /// 44 the metadata, and in the last 4 bytes the CRC-32 of the bytes before them.
    fn encode(&self, metadata: &Metadata) -> Vec<u8> {
        let mut h = vec![0_u8; METADATA_AT];
        h[..8].copy_from_slice(MAGIC);
        h[8..12].copy_from_slice(&VERSION.to_le_bytes());
        h[16..24].copy_from_slice(&self.generation.to_le_bytes());
        h[24..32].copy_from_slice(&self.stamp.to_le_bytes());
        if let LoadState::Loading { last_id } = self.state {
            h[32..40].copy_from_slice(&last_id.unwrap_or(0).to_le_bytes());
            h[40] = 1 + u8::from(last_id.is_some());
        }
        metadata.encode(&mut h);
        h.resize(SLOT - 4, 0);
        let crc = crc(&h);
        h.extend_from_slice(&crc.to_le_bytes());
        h
    }

    fn decode(h: &[u8]) -> Option<(Self, Metadata)> {
        let intact = &h[..8] == MAGIC && crc(&h[..SLOT - 4]) == u32_at(h, SLOT - 4);
        let metadata = Metadata::decode(&h[METADATA_AT..SLOT - 4]).filter(|_| intact)?;
        let header = Self {
            generation: u64_at(h, 16),
            stamp: u64_at(h, 24),
            state: match h[40] {
                0 => LoadState::Complete,
                1 => LoadState::Loading { last_id: None },
                _ => LoadState::Loading {
                    last_id: Some(u64_at(h, 32)),
                },
            },
        };
        Some((header, metadata))
    }

    fn read(page: &[u8], path: &Path) -> Result<(Self, Metadata)> {
        if !page.starts_with(MAGIC_PREFIX) && !page[SLOT..].starts_with(MAGIC_PREFIX) {
            return Err(NodeStoreError::NotAStore(path.to_path_buf()));
        }
        for h in [&page[..SLOT], &page[SLOT..]] {
            if h.starts_with(MAGIC_PREFIX) && &h[..8] != MAGIC {
                return Err(NodeStoreError::UnsupportedVersion {
                    path: path.to_path_buf(),
                    found: u32_at(h, 8),
                    expected: VERSION,
                });
            }
        }
        [&page[..SLOT], &page[SLOT..]]
            .into_iter()
            .filter_map(Self::decode)
            .max_by_key(|(h, _)| h.generation)
            .ok_or_else(|| NodeStoreError::Corrupt {
                path: path.to_path_buf(),
                reason: "both copies of the header are damaged".to_owned(),
            })
    }

    /// Writes the header and `metadata` over the older copy, and syncs it.
    fn write(&self, metadata: &Metadata, file: &File, path: &Path) -> Result<()> {
        let slot = (self.generation % 2) as usize * SLOT;
        write_at(file, &self.encode(metadata), slot as u64).at(path)?;
        file.sync_data().at(path)
    }
}

/// Node locations in 8 bytes per node ID in one file; the module documentation has the format.
///
/// The fastest store to build, read and change, when the host has memory for the whole file: 8
/// bytes times the highest node ID, over 100 GB for the planet. Too big for an extract, as its size
/// follows the highest ID, not the number of nodes.
///
/// Changes are written in place, so after a crash the store may have some of the changes since
/// the last commit: apply the changes since [`NodeRead::stamp`] again.
pub struct DenseStore<M: Mode = ReadWrite> {
    path: PathBuf,
    file: File,
    /// read only: changes are written to the file, so that the OS writes only the blocks they
    /// change, where a write to a mapping that is cached in large pages may have it write all of
    /// a large page
    map: Mmap,
    committed: Header,
    /// with the changes not yet committed
    metadata: Metadata,
    mode: PhantomData<M>,
}

/// Reads locations of a [`DenseStore`].
pub struct DenseReader<'a, M: Mode = ReadWrite>(&'a DenseStore<M>);

#[inline]
fn encode(loc: Location) -> u64 {
    u64::from(loc.lat.cast_unsigned() ^ 0x8000_0000) | (u64::from(loc.lon.cast_unsigned()) << 32)
}

#[inline]
#[expect(clippy::cast_possible_truncation)]
fn decode(v: u64) -> Option<Location> {
    let lat = v as u32;
    (lat != 0).then(|| {
        Location::new(
            (lat ^ 0x8000_0000).cast_signed(),
            ((v >> 32) as u32).cast_signed(),
        )
    })
}

impl<M: Mode> DenseStore<M> {
    fn open_impl(path: &Path, write: bool) -> Result<Self> {
        let file = if write {
            let (file, empty) = open_rw(path)?;
            if empty {
                file.set_len(PAGE_SIZE as u64).at(path)?;
                let header = Header {
                    generation: 1,
                    stamp: 0,
                    state: LoadState::Complete,
                };
                header.write(&Metadata::default(), &file, path)?;
                sync_dir(path)?;
            }
            file
        } else {
            open_ro(path)?
        };
        let len = file.metadata().at(path)?.len();
        if len < PAGE_SIZE as u64 || !(len - PAGE_SIZE as u64).is_multiple_of(8) {
            return Err(NodeStoreError::NotAStore(path.to_path_buf()));
        }
        let mut page = vec![0_u8; PAGE_SIZE];
        read_at(&file, &mut page, 0).at(path)?;
        let (committed, metadata) = Header::read(&page, path)?;
        let map = map_file(&file).at(path)?;
        let store = Self {
            path: path.to_path_buf(),
            file,
            map,
            committed,
            metadata,
            mode: PhantomData,
        };
        store.advise();
        Ok(store)
    }

    /// Lookups go all over the file, so reading ahead of the page that faulted only evicts pages
    /// that are still needed.
    #[cfg_attr(
        not(unix),
        expect(clippy::unused_self, reason = "the hint exists only on Unix")
    )]
    fn advise(&self) {
        #[cfg(unix)]
        let _ = self.map.advise(memmap2::Advice::Random);
    }

    #[inline]
    fn value(&self, id: u64) -> Option<Location> {
        if id > MAX_NODE_ID {
            return None;
        }
        // Below the highest ID, the offset fits a u64; it may not fit a 32-bit usize.
        let at = usize::try_from(PAGE_SIZE as u64 + id * 8).ok()?;
        let bytes = self.map.get(at..)?.get(..8)?;
        decode(u64::from_le_bytes(bytes.try_into().unwrap()))
    }

    /// Node IDs the file has room for
    fn capacity(&self) -> u64 {
        (self.map.len() - PAGE_SIZE) as u64 / 8
    }
}

impl<M: Mode> NodeRead for DenseStore<M> {
    type Reader<'a> = DenseReader<'a, M>;

    fn reader(&self) -> DenseReader<'_, M> {
        DenseReader(self)
    }

    #[inline]
    fn get(&self, id: u64) -> Option<Location> {
        self.value(id)
    }

    fn stamp(&self) -> u64 {
        self.committed.stamp
    }

    fn load_state(&self) -> LoadState {
        self.committed.state
    }

    fn metadata(&self) -> &Metadata {
        &self.metadata
    }

    fn preload(&self) -> Result<()> {
        super::file::preload(&self.file, &self.path)
    }

    fn for_each(&self, mut f: impl FnMut(u64, Location)) -> Result<()> {
        let values = self.map[PAGE_SIZE..].as_chunks::<8>().0;
        for (id, v) in values.iter().enumerate() {
            if let Some(loc) = decode(u64::from_le_bytes(*v)) {
                f(id as u64, loc);
            }
        }
        Ok(())
    }
}

impl<M: Mode> NodeReader for DenseReader<'_, M> {
    #[inline]
    fn get(&mut self, id: u64) -> Option<Location> {
        self.0.value(id)
    }
}

impl FileStore for DenseStore<ReadWrite> {
    type Opts = DenseOpts;
    type ReadOnly = DenseStore<ReadOnly>;

    fn open_with(path: impl AsRef<Path>, _opts: &DenseOpts) -> Result<Self> {
        Self::open_impl(path.as_ref(), true)
    }

    /// The store keeps nothing to fix but its header, which has two copies.
    fn open_and_fix_with(path: impl AsRef<Path>, opts: &DenseOpts) -> Result<(Self, FixReport)> {
        Ok((Self::open_with(path, opts)?, FixReport::default()))
    }

    fn open_read_only_with(
        path: impl AsRef<Path>,
        _opts: &DenseOpts,
    ) -> Result<DenseStore<ReadOnly>> {
        DenseStore::open_impl(path.as_ref(), false)
    }
}

impl NodeStore for DenseStore<ReadWrite> {
    type Loader<'a> = DenseLoader<'a>;

    fn set(&mut self, id: u64, loc: Location) -> Result<()> {
        check(id, loc)?;
        self.require_complete()?;
        self.write(id, encode(loc))
    }

    fn remove(&mut self, id: u64) -> Result<()> {
        check(id, Location::default())?;
        self.require_complete()?;
        if id < self.capacity() {
            self.write(id, 0)?;
        }
        Ok(())
    }

    fn set_metadata(&mut self, key: &str, value: &[u8]) -> Result<()> {
        self.metadata.set(key, value)
    }

    fn remove_metadata(&mut self, key: &str) {
        self.metadata.remove(key);
    }

    fn commit(&mut self, stamp: u64) -> Result<()> {
        self.require_complete()?;
        self.write_header(Header {
            generation: self.committed.generation + 1,
            stamp,
            state: LoadState::Complete,
        })
    }

    fn bulk_load(&mut self) -> Result<DenseLoader<'_>> {
        let resume = match self.committed.state {
            LoadState::Loading { last_id: None } => {
                // The load was cut short before its first checkpoint: it starts over, so nothing it
                // wrote may stay for the nodes the new input lacks.
                self.discard_values()?;
                None
            }
            LoadState::Loading { last_id } => last_id,
            LoadState::Complete if self.capacity() > 0 => return Err(NodeStoreError::NotEmpty),
            LoadState::Complete => {
                // a load cut short before its first checkpoint must not look complete
                self.write_header(Header {
                    generation: self.committed.generation + 1,
                    state: LoadState::Loading { last_id: None },
                    ..self.committed
                })?;
                None
            }
        };
        let file_len = self.file.metadata().at(&self.path)?.len();
        Ok(DenseLoader {
            store: self,
            resume,
            last_id: None,
            since_checkpoint: 0,
            buf: Vec::new(),
            file_len,
            buf_start: 0,
            checkpoint: None,
            finished: false,
        })
    }
}

impl DenseStore<ReadWrite> {
    fn require_complete(&self) -> Result<()> {
        match self.committed.state {
            LoadState::Complete => Ok(()),
            LoadState::Loading { .. } => Err(NodeStoreError::LoadUnfinished),
        }
    }

    fn write(&mut self, id: u64, value: u64) -> Result<()> {
        if id >= self.capacity() {
            self.grow(id)?;
        }
        let at = PAGE_SIZE as u64 + id * 8;
        write_at(&self.file, &value.to_le_bytes(), at).at(&self.path)
    }

    /// Maps all of the file, after a bulk load wrote past the end of the mapping.
    fn remap(&mut self) -> Result<()> {
        if self.file.metadata().at(&self.path)?.len() != self.map.len() as u64 {
            self.map = map_file(&self.file).at(&self.path)?;
            self.advise();
        }
        Ok(())
    }

    /// Truncates the file to its header page, dropping every value in it.
    fn discard_values(&mut self) -> Result<()> {
        if self.capacity() == 0 {
            return Ok(());
        }
        // Windows does not shrink a mapped file, so the file is unmapped first.
        self.map = MmapOptions::new()
            .len(PAGE_SIZE)
            .map_anon()
            .and_then(MmapMut::make_read_only)
            .at(&self.path)?;
        self.file.set_len(PAGE_SIZE as u64).at(&self.path)?;
        self.map = map_file(&self.file).at(&self.path)?;
        Ok(())
    }

    /// Grows the file to hold node `id`, and maps all of it.
    fn grow(&mut self, id: u64) -> Result<()> {
        let len = self.map.len() as u64;
        let needed = PAGE_SIZE as u64 + (id + 1) * 8;
        let new_len = needed
            .max(len + GROW_MIN.max(len / 8))
            .next_multiple_of(PAGE_SIZE as u64);
        self.file.set_len(new_len).at(&self.path)?;
        self.map = map_file(&self.file).at(&self.path)?;
        self.advise();
        Ok(())
    }

    /// Makes the changes durable, then `header`, which commits them.
    fn write_header(&mut self, header: Header) -> Result<()> {
        self.file.sync_data().at(&self.path)?;
        header.write(&self.metadata, &self.file, &self.path)?;
        self.committed = header;
        Ok(())
    }
}

/// Bulk loads a [`DenseStore`]; see [`BulkLoad`]. It writes the values of a million node IDs
/// at a time to the file, not through the memory map, and makes checkpoints durable on another
/// thread while it goes on loading.
pub struct DenseLoader<'a> {
    store: &'a mut DenseStore<ReadWrite>,
    resume: Option<u64>,
    last_id: Option<u64>,
    since_checkpoint: u64,
    /// the values of the node IDs from `buf_start` on, not yet written to the file
    buf: Vec<u8>,
    /// the file's length
    file_len: u64,
    buf_start: u64,
    /// the last checkpoint, if it may not be durable yet
    checkpoint: Option<(JoinHandle<Result<()>>, Header)>,
    finished: bool,
}

impl DenseLoader<'_> {
    /// Writes the buffered values to the file. The file is grown first, as [`DenseStore::grow`]
    /// grows it: writing past the end of a file has some file systems (XFS) keep blocks after it,
    /// which take disk space within the file once it grows past them.
    fn flush(&mut self) -> Result<()> {
        if !self.buf.is_empty() {
            let store = &*self.store;
            let at = PAGE_SIZE as u64 + self.buf_start * 8;
            let end = at + self.buf.len() as u64;
            if end > self.file_len {
                self.file_len = end
                    .max(self.file_len + GROW_MIN.max(self.file_len / 8))
                    .next_multiple_of(PAGE_SIZE as u64);
                store.file.set_len(self.file_len).at(&store.path)?;
            }
            write_at(&store.file, &self.buf, at).at(&store.path)?;
            self.buf.clear();
        }
        Ok(())
    }

    /// Keeps the nodes up to `last_id`, on another thread; the checkpoint before is waited for
    /// first, so that headers are written in order.
    fn start_checkpoint(&mut self, last_id: u64) -> Result<()> {
        self.flush()?;
        self.wait_for_checkpoint()?;
        let store = &*self.store;
        let header = Header {
            generation: store.committed.generation + 1,
            state: LoadState::Loading {
                last_id: Some(last_id),
            },
            ..store.committed
        };
        let file = store.file.try_clone().at(&store.path)?;
        let (path, metadata) = (store.path.clone(), store.metadata.clone());
        let thread = thread::spawn(move || {
            file.sync_data().at(&path)?;
            header.write(&metadata, &file, &path)
        });
        self.checkpoint = Some((thread, header));
        Ok(())
    }

    fn wait_for_checkpoint(&mut self) -> Result<()> {
        if let Some((thread, header)) = self.checkpoint.take() {
            thread.join().expect("checkpoint thread panicked")?;
            self.store.committed = header;
        }
        Ok(())
    }
}

impl BulkLoad for DenseLoader<'_> {
    fn resume_after(&self) -> Option<u64> {
        self.resume
    }

    fn push(&mut self, id: u64, loc: Location) -> Result<()> {
        check(id, loc)?;
        check_order(self.last_id, id)?;
        self.last_id = Some(id);
        if self.resume.is_some_and(|r| id <= r) {
            return check_resumed(self.store.value(id), id, loc);
        }
        let buf_end = self.buf_start + self.buf.len() as u64 / 8;
        if !self.buf.is_empty() && (id - self.buf_start >= LOAD_IDS || id >= buf_end + PAGE_IDS) {
            self.flush()?;
        }
        if self.buf.is_empty() {
            self.buf_start = id;
        }
        // IDs in between have no node: their values stay zero
        self.buf
            .resize(usize::try_from(id - self.buf_start).unwrap() * 8, 0);
        self.buf.extend_from_slice(&encode(loc).to_le_bytes());
        self.since_checkpoint += 1;
        if self.since_checkpoint >= CHECKPOINT {
            self.since_checkpoint = 0;
            self.start_checkpoint(id)?;
        }
        Ok(())
    }

    fn finish(mut self, stamp: u64) -> Result<()> {
        self.flush()?;
        self.wait_for_checkpoint()?;
        self.store.file.sync_data().at(&self.store.path)?;
        self.store.remap()?;
        let header = Header {
            generation: self.store.committed.generation + 1,
            stamp,
            state: LoadState::Complete,
        };
        self.store.write_header(header)?;
        self.finished = true;
        Ok(())
    }
}

impl Drop for DenseLoader<'_> {
    /// Keeps the nodes up to the last checkpoint, and maps what the load wrote, so that resuming
    /// it can check them.
    fn drop(&mut self) {
        if !self.finished {
            let _ = self.wait_for_checkpoint();
            let _ = self.store.remap();
        }
    }
}

// Holes depend on the file system: ext4 and XFS keep them, while APFS, on macOS, used the whole file's size in CI.
#[cfg(all(test, target_os = "linux"))]
mod tests {
    use std::fs;

    use crate::Location;
    use crate::nodes::file::tests::TestFile;
    use crate::nodes::{BulkLoad as _, DenseStore, FileStore as _, NodeRead as _, NodeStore as _};

    /// A load of nodes far apart takes disk space only for the pages they are in.
    #[test]
    fn keeps_gaps_as_holes() {
        use std::os::unix::fs::MetadataExt as _;
        let f = TestFile::new("dense_holes");
        let mut store = DenseStore::open(&f.0).unwrap();
        let mut load = store.bulk_load().unwrap();
        // more than a page apart, but several to a write
        let nodes: Vec<(u64, Location)> = (0..200)
            .map(|i| (i * 2000, Location::new(1, i32::try_from(i).unwrap())))
            .collect();
        load.push_batch(&nodes).unwrap();
        load.finish(1).unwrap();
        for &(id, loc) in &nodes {
            assert_eq!(store.get(id), Some(loc));
        }
        let meta = fs::metadata(&f.0).unwrap();
        assert!(meta.len() > 3_000_000);
        assert!(
            meta.blocks() * 512 < 1_600_000,
            "{} bytes on disk",
            meta.blocks() * 512
        );
    }
}
