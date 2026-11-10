//! [`PagedStore`]: node locations in 4 KiB pages of one file, read through a memory map.
//!
//! Blocks of 64 IDs, encoded as in [`super::codec`], are kept in ID order in pages. A bulk load
//! fills pages to `fill`, so most later changes fit in place; a page that overflows moves its upper
//! blocks to a new page at the end of the file.
//!
//! The store has up to three files:
//! - `<path>`: a header page holding two copies of the header, written in turn so that a crash
//!   while writing one leaves the other, then the data pages, each with a CRC-32.
//! - `<path>.dir`: the directory, the first block of each page, in the order pages were added. It is
//!   only appended to, and the header says how many of its entries are committed.
//! - `<path>.undo`: the old contents of the pages a commit changes, written before it changes them,
//!   so that opening the store after a crash during the commit rolls it back.
//!
//! Opening the store checks the header copies, the directory and the undo file, but not the pages,
//! which would read the whole file. [`super::FileStore::open_and_fix`] checks every page's CRC-32.
//!
//! The file is read through a read-only memory map and written with positioned writes. Changes to
//! committed pages are held in memory until a commit. New pages go to the file at once, as nothing
//! committed refers to them until the header does.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Write as _};
use std::marker::PhantomData;
use std::mem;
use std::ops::{Range, RangeInclusive};
use std::path::{Path, PathBuf};
use std::thread::{self, JoinHandle};

use memmap2::Mmap;
use rayon::prelude::*;

use super::codec::{
    BLOCK_LEN, BLOCK_SHIFT, Block, ENTRY, Head, PAGE_HEADER, Slots, block_of, decode_page,
    encode_block, encode_detached, encode_page, find_in_page, pack_block, packed_len, page_crcs,
    page_header, rank, read_block, set_entry, set_page_crc, set_page_header, u32_at, u64_at,
};
use super::file::{
    At as _, crc, header_copies, map_file, open_ro, open_rw, read_at, sidecar, sync_dir, write_at,
};
use super::store::{check, check_order, check_resumed};
use super::{
    BulkLoad, FileStore, FixReport, LoadState, MAX_NODE_ID, Metadata, Mode, NodeRead, NodeReader,
    NodeStore, NodeStoreError, ReadOnly, ReadWrite, Result,
};
use crate::Location;

const PAGE_SIZE: usize = 4096;
const MAGIC: &[u8; 8] = b"ONCPAGE3";
/// What every version's magic starts with: a file that has it is a paged store, of a version this
/// library may not read.
const MAGIC_PREFIX: &[u8; 7] = b"ONCPAGE";
const VERSION: u32 = 3;
/// Each copy of the header takes half of the header page.
const SLOT: usize = PAGE_SIZE / 2;
/// Where a header copy's metadata starts; its CRC-32 is in the copy's last 4 bytes.
const METADATA_AT: usize = 68;
const DIR_MAGIC: &[u8; 8] = b"ONCDIR03";
/// A directory entry: first block (u64), page (u32)
const DIR_ENTRY: usize = 12;
const UNDO_MAGIC: &[u8; 8] = b"ONCUNDO3";
/// The data file grows by at least this much at a time.
const GROW_MIN: u64 = if cfg!(test) { 64 << 10 } else { 1 << 20 };
/// The directory index has an entry per 2^8 blocks (16,384 IDs).
const BUCKET_SHIFT: u32 = 8;
/// Pages a bulk load writes between checkpoints: 1 GiB.
const CHECKPOINT_PAGES: u64 = if cfg!(test) { 8 } else { 1 << 18 };
/// Pages a bulk load writes to the file at once.
const WRITE_PAGES: usize = 256;
/// Blocks a bulk load encodes per task when encoding in parallel.
const PARALLEL_BLOCKS: usize = 256;
/// Nodes a batch needs for a bulk load to encode it in parallel.
const PARALLEL_NODES: usize = 8192;

/// Options for [`PagedStore`].
#[derive(Clone, Debug)]
pub struct PagedOpts {
    fill: f64,
}

impl Default for PagedOpts {
    fn default() -> Self {
        // room in each page for the changes of the days after a load, so few pages split
        Self { fill: 0.97 }
    }
}

impl PagedOpts {
    /// How full a bulk load, or new IDs above all others, fill each page; the rest is room for
    /// later changes.
    ///
    /// # Panics
    /// If `fill` is not between 0.25 and 1.
    #[must_use]
    pub fn fill(mut self, fill: f64) -> Self {
        assert!(
            (0.25..=1.0).contains(&fill),
            "fill must be between 0.25 and 1, not {fill}"
        );
        self.fill = fill;
        self
    }

    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    fn fill_limit(&self) -> usize {
        (self.fill * PAGE_SIZE as f64) as usize
    }
}

/// Counts over all pages, from [`PagedStore::stats`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PagedStats {
    /// Nodes stored, the uncommitted changes included.
    pub nodes: u64,
    /// Blocks with at least one node; a block covers 64 consecutive node IDs.
    pub blocks: u64,
    /// Pages of blocks, 4 KiB each.
    pub pages: u64,
    /// bytes used in the pages, headers included
    pub used_bytes: u64,
    /// size of the data file, which grows by an eighth at a time, so it can be that much past the last page in use
    pub file_bytes: u64,
    /// blocks with all 64 IDs present, a single run of them, or any other mix
    pub masks: [u64; 3],
    /// blocks with plain integers, varints, or packed steps
    pub codecs: [u64; 3],
}

/// The committed state of the store, as its header says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Header {
    generation: u64,
    /// pages in use, the header page included
    pages: u64,
    dir_len: u64,
    stamp: u64,
    state: LoadState,
}

impl Header {
    /// A copy of the header: magic, version, page size and block IDs (u32 each), then at 24 the
    /// generation, pages, directory entries, stamp and last loaded ID (u64 each), at 64 whether a
    /// bulk load is unfinished (1) and has kept nodes (2), at 68 the metadata, and in the last 4
    /// bytes the CRC-32 of the bytes before them.
    fn encode(&self, metadata: &Metadata) -> Vec<u8> {
        let mut h = vec![0_u8; METADATA_AT];
        h[..8].copy_from_slice(MAGIC);
        h[8..12].copy_from_slice(&VERSION.to_le_bytes());
        h[12..16].copy_from_slice(&u32::try_from(PAGE_SIZE).unwrap().to_le_bytes());
        h[16..20].copy_from_slice(&u32::try_from(BLOCK_LEN).unwrap().to_le_bytes());
        h[24..32].copy_from_slice(&self.generation.to_le_bytes());
        h[32..40].copy_from_slice(&self.pages.to_le_bytes());
        h[40..48].copy_from_slice(&self.dir_len.to_le_bytes());
        h[48..56].copy_from_slice(&self.stamp.to_le_bytes());
        if let LoadState::Loading { last_id } = self.state {
            h[56..64].copy_from_slice(&last_id.unwrap_or(0).to_le_bytes());
            h[64] = 1 + u8::from(last_id.is_some());
        }
        metadata.encode(&mut h);
        h.resize(SLOT - 4, 0);
        let crc = crc(&h);
        h.extend_from_slice(&crc.to_le_bytes());
        h
    }

    /// The header and metadata from a copy, if it is intact.
    fn decode(h: &[u8]) -> Option<(Self, Metadata)> {
        let intact = &h[..8] == MAGIC
            && crc(&h[..SLOT - 4]) == u32_at(h, SLOT - 4)
            && u32_at(h, 12) as usize == PAGE_SIZE
            && u32_at(h, 16) as usize == BLOCK_LEN;
        let metadata = Metadata::decode(&h[METADATA_AT..SLOT - 4]).filter(|_| intact)?;
        let header = Self {
            generation: u64_at(h, 24),
            pages: u64_at(h, 32),
            dir_len: u64_at(h, 40),
            stamp: u64_at(h, 48),
            state: match h[64] {
                0 => LoadState::Complete,
                1 => LoadState::Loading { last_id: None },
                _ => LoadState::Loading {
                    last_id: Some(u64_at(h, 56)),
                },
            },
        };
        Some((header, metadata))
    }

    /// The newer intact copy in a header page of a file of `file_len` bytes, and its metadata.
    fn read(page: &[u8], file_len: u64, path: &Path) -> Result<(Self, Metadata)> {
        let corrupt = |reason: &str| NodeStoreError::Corrupt {
            path: path.to_path_buf(),
            reason: reason.to_owned(),
        };
        let copies = header_copies(page, path, *MAGIC, VERSION)?;
        let (header, metadata) = copies
            .into_iter()
            .filter_map(Self::decode)
            .max_by_key(|(h, _)| h.generation)
            .ok_or_else(|| corrupt("both copies of the header are damaged"))?;
        if header.pages == 0 || header.pages > file_len / PAGE_SIZE as u64 {
            return Err(corrupt("the file is shorter than its header says"));
        }
        if header.dir_len >= header.pages {
            return Err(corrupt("the header has more directory entries than pages"));
        }
        Ok((header, metadata))
    }
}

/// The first block of each data page, in block order.
#[derive(Default)]
struct Directory {
    starts: Vec<u64>,
    pages: Vec<u32>,
    /// for each bucket of blocks, the index of the first page starting in it or later
    index: Vec<u32>,
}

impl Directory {
    /// The directory of pages listed in any order.
    fn new(mut entries: Vec<(u64, u32)>) -> Self {
        entries.sort_unstable();
        let (starts, pages) = entries.into_iter().unzip();
        let mut dir = Self {
            starts,
            pages,
            index: Vec::new(),
        };
        dir.extend_index();
        dir
    }

    fn len(&self) -> u64 {
        self.starts.len() as u64
    }

    /// The index of the page that would hold block `num`, if any page could
    #[inline]
    fn find(&self, num: u64) -> Option<usize> {
        let bucket = usize::try_from(num >> BUCKET_SHIFT).unwrap_or(usize::MAX);
        let len = self.starts.len();
        let at = |b: usize| self.index.get(b).map_or(len, |&i| i as usize);
        // the last of the bucket's pages starting at or below `num`, or else the page before them
        let (lo, hi) = (at(bucket), at(bucket.saturating_add(1)));
        (lo + self.starts[lo..hi].partition_point(|&s| s <= num)).checked_sub(1)
    }

    fn insert(&mut self, i: usize, start: u64, page: u32) {
        self.starts.insert(i, start);
        self.pages.insert(i, page);
        // pages starting in later buckets have moved up by one
        let later = usize::try_from(start >> BUCKET_SHIFT).expect("node IDs are limited") + 1;
        for i in self.index.iter_mut().skip(later) {
            *i += 1;
        }
        self.extend_index();
    }

    fn push(&mut self, start: u64, page: u32) {
        self.insert(self.starts.len(), start, page);
    }

    /// Drops the pages above the first `len`, which must start above all others.
    fn truncate(&mut self, len: usize) {
        self.starts.truncate(len);
        self.pages.truncate(len);
        let buckets = self
            .starts
            .last()
            .map_or(0, |&s| usize::try_from(s >> BUCKET_SHIFT).unwrap() + 2);
        self.index.truncate(buckets);
        for i in &mut self.index {
            *i = (*i).min(u32::try_from(len).unwrap());
        }
    }

    /// Adds index entries up to the bucket after the last page's.
    fn extend_index(&mut self) {
        let Some(&last) = self.starts.last() else {
            return;
        };
        let buckets = usize::try_from(last >> BUCKET_SHIFT).expect("node IDs are limited") + 2;
        let mut i = self.index.last().map_or(0, |&i| i as usize);
        for b in self.index.len()..buckets {
            let start = (b as u64) << BUCKET_SHIFT;
            while i < self.starts.len() && self.starts[i] < start {
                i += 1;
            }
            self.index
                .push(u32::try_from(i).expect("more than 2^32 pages"));
        }
    }
}

/// Node locations in 4 KiB pages of one file; the module documentation has the format.
///
/// The fastest and smallest store for large data, as long as the file fits in memory. Opened to
/// write, it keeps commits through a crash or a power loss: an interrupted commit is rolled back
/// when the store is next opened.
pub struct PagedStore<M: Mode = ReadWrite> {
    path: PathBuf,
    file: File,
    file_len: u64,
    map: Mmap,
    /// the directory file, open to append in a store open to write
    dir_file: File,
    fill_limit: usize,
    committed: Header,
    /// with the changes not yet committed
    metadata: Metadata,
    /// pages in use, the header page included; above the committed ones, pages not yet committed
    allocated: u64,
    dir: Directory,
    /// directory entries of the pages added since the last commit
    new_entries: Vec<(u64, u32)>,
    /// pages changed since the last commit, decoded: their first block, and their blocks
    changed: HashMap<u32, (u64, Vec<Block>)>,
    /// a commit or a growth of the file failed: `changed`, `dir` and `allocated` no longer match the file, so only
    /// opening it again, which rolls the commit back, makes it safe to use
    poisoned: bool,
    mode: PhantomData<M>,
}

/// Reads locations of a [`PagedStore`] for one thread.
pub struct PagedReader<'a, M: Mode = ReadWrite> {
    store: &'a PagedStore<M>,
    slots: Slots,
}

impl<M: Mode> PagedStore<M> {
    fn open_impl(
        path: &Path,
        opts: &PagedOpts,
        write: bool,
        fix: bool,
    ) -> Result<(Self, FixReport)> {
        let file = if write {
            let (file, empty) = open_rw(path)?;
            if empty {
                init(&file, path)?;
            }
            file
        } else {
            open_ro(path)?
        };
        let file_len = file.metadata().at(path)?.len();
        let mut page = vec![0_u8; PAGE_SIZE];
        if file_len < PAGE_SIZE as u64 {
            let n = usize::try_from(file_len).unwrap();
            read_at(&file, &mut page[..n], 0).at(path)?;
            return Err(if page.starts_with(MAGIC_PREFIX) {
                NodeStoreError::Corrupt {
                    path: path.to_path_buf(),
                    reason: "the file is shorter than its header".to_owned(),
                }
            } else {
                NodeStoreError::NotAStore(path.to_path_buf())
            });
        }
        read_at(&file, &mut page, 0).at(path)?;
        let (mut header, metadata) = Header::read(&page, file_len, path)?;

        let undo_path = sidecar(path, "undo");
        if let Some(records) = read_undo(&undo_path, header.generation + 1, header.pages) {
            if !write {
                return Err(NodeStoreError::NeedsRecovery(path.to_path_buf()));
            }
            for (p, old) in records {
                write_at(&file, &old, u64::from(p) * PAGE_SIZE as u64).at(path)?;
            }
            file.sync_data().at(path)?;
            // the rolled back commit's undo is now stale
            write_at(
                &OpenOptions::new()
                    .write(true)
                    .open(&undo_path)
                    .at(&undo_path)?,
                &[0; 8],
                0,
            )
            .at(&undo_path)?;
        }

        let map = map_file(&file).at(path)?;
        let dir_path = sidecar(path, "dir");
        let mut report = FixReport::default();
        let entries = match read_dir(&dir_path, &header) {
            Ok(entries) if fix => {
                let (entries, lost) = check_pages(&map, entries);
                report.lost = lost;
                entries
            }
            Ok(entries) => entries,
            Err(_) if fix && write => {
                report.rebuilt_index = true;
                let (entries, lost) = scan_pages(&map, &header);
                report.lost = lost;
                entries
            }
            Err(reason) => {
                return Err(NodeStoreError::NeedsFix {
                    path: path.to_path_buf(),
                    reason,
                });
            }
        };
        if !report.is_clean() {
            // a directory of the pages kept, then a header committing it
            write_dir(&dir_path, &entries)?;
            header.generation += 1;
            header.dir_len = entries.len() as u64;
            write_header(&file, &header, &metadata, path)?;
        }
        let dir_file = if write {
            OpenOptions::new().read(true).write(true).open(&dir_path)
        } else {
            File::open(&dir_path)
        }
        .at(&dir_path)?;

        let store = Self {
            path: path.to_path_buf(),
            file,
            file_len,
            map,
            dir_file,
            fill_limit: opts.fill_limit(),
            committed: header,
            metadata,
            allocated: header.pages,
            dir: Directory::new(entries),
            new_entries: Vec::new(),
            changed: HashMap::new(),
            poisoned: false,
            mode: PhantomData,
        };
        store.advise(false);
        Ok((store, report))
    }

    /// Tells the kernel how pages are about to be read: `sequential` for a pass over all of them in
    /// file order, otherwise lookups all over the file, where reading ahead of the page that faulted
    /// only evicts pages that are still needed.
    #[cfg_attr(
        not(unix),
        expect(clippy::unused_self, reason = "the hint exists only on Unix")
    )]
    fn advise(&self, sequential: bool) {
        // only a hint, so a kernel that does not take it changes nothing
        #[cfg(unix)]
        let _ = self.map.advise(if sequential {
            memmap2::Advice::Sequential
        } else {
            memmap2::Advice::Random
        });
        #[cfg(not(unix))]
        let _ = sequential;
    }

    /// Counts nodes, blocks and bytes over all pages.
    #[must_use]
    pub fn stats(&self) -> PagedStats {
        let mut s = PagedStats {
            file_bytes: self.file_len,
            ..PagedStats::default()
        };
        let mut tally = |page: &[u8]| {
            let h = page_header(page);
            s.pages += 1;
            s.used_bytes += h.used as u64;
            for i in 0..h.n_blocks {
                let head = read_block(page, &h, i);
                s.blocks += 1;
                s.nodes += u64::from(head.mask.count_ones());
                s.masks[usize::from(head.mask_kind)] += 1;
                s.codecs[usize::from(head.codec)] += 1;
            }
        };
        self.advise(true);
        for &p in &self.dir.pages {
            match self.changed.get(&p) {
                // as the next commit writes it: split into pages that fit, where it grew past one
                Some((first, blocks)) => {
                    for (_, page) in encode_changed(*first, blocks, self.fill_limit) {
                        tally(&page);
                    }
                }
                None => tally(self.page(p)),
            }
        }
        self.advise(false);
        s
    }

    /// Page `p` as the file has it, without the changes not yet committed
    #[inline]
    fn page(&self, p: u32) -> &[u8] {
        let start = p as usize * PAGE_SIZE;
        &self.map[start..start + PAGE_SIZE]
    }

    /// Where block `num` is: in a page changed since the last commit, or in the file, if stored
    #[inline]
    fn find_block(&self, num: u64) -> Option<Found<'_>> {
        let p = self.dir.pages[self.dir.find(num)?];
        if !self.changed.is_empty()
            && let Some((_, blocks)) = self.changed.get(&p)
        {
            let i = blocks.binary_search_by_key(&num, |b| b.num).ok()?;
            return Some(Found::Changed(&blocks[i]));
        }
        let page = self.page(p);
        Some(Found::Page(page, find_in_page(page, num)?))
    }

    /// Decodes block `num` into `locs`, and returns its mask, 0 if it is not stored.
    #[inline]
    fn load_block(&self, num: u64, locs: &mut [Location; BLOCK_LEN]) -> u64 {
        match self.find_block(num) {
            Some(Found::Page(page, head)) => {
                head.decode_into(page, locs);
                head.mask
            }
            Some(Found::Changed(b)) => {
                locs[..b.locs.len()].copy_from_slice(&b.locs);
                b.mask
            }
            None => 0,
        }
    }
}

impl<M: Mode> NodeRead for PagedStore<M> {
    type Reader<'a> = PagedReader<'a, M>;

    #[inline]
    fn reader(&self) -> PagedReader<'_, M> {
        PagedReader {
            store: self,
            slots: Slots::default(),
        }
    }

    #[inline]
    fn get(&self, id: u64) -> Option<Location> {
        let (num, bit) = block_of(id);
        match self.find_block(num)? {
            Found::Page(page, head) => {
                (head.mask & bit != 0).then(|| head.location(page, rank(head.mask, bit)))
            }
            Found::Changed(b) => (b.mask & bit != 0).then(|| b.locs[rank(b.mask, bit)]),
        }
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
        self.advise(true);
        for &p in &self.dir.pages {
            let decoded;
            let blocks = if let Some((_, blocks)) = self.changed.get(&p) {
                blocks
            } else {
                decoded = decode_page(self.page(p)).1;
                &decoded
            };
            for b in blocks {
                b.nodes().for_each(|(id, loc)| f(id, loc));
            }
        }
        self.advise(false);
        Ok(())
    }
}

impl<M: Mode> NodeReader for PagedReader<'_, M> {
    #[inline]
    fn get(&mut self, id: u64) -> Option<Location> {
        let store = self.store;
        self.slots.get(id, |num, locs| store.load_block(num, locs))
    }
}

impl FileStore for PagedStore<ReadWrite> {
    type Opts = PagedOpts;
    type ReadOnly = PagedStore<ReadOnly>;

    fn open_with(path: impl AsRef<Path>, opts: &PagedOpts) -> Result<Self> {
        Ok(Self::open_impl(path.as_ref(), opts, true, false)?.0)
    }

    fn open_and_fix_with(path: impl AsRef<Path>, opts: &PagedOpts) -> Result<(Self, FixReport)> {
        Self::open_impl(path.as_ref(), opts, true, true)
    }

    fn open_read_only_with(
        path: impl AsRef<Path>,
        opts: &PagedOpts,
    ) -> Result<PagedStore<ReadOnly>> {
        Ok(PagedStore::<ReadOnly>::open_impl(path.as_ref(), opts, false, false)?.0)
    }
}

impl NodeStore for PagedStore<ReadWrite> {
    type Loader<'a> = PagedLoader<'a>;

    fn set(&mut self, id: u64, loc: Location) -> Result<()> {
        check(id, loc)?;
        self.modify(id, Some(loc))
    }

    fn remove(&mut self, id: u64) -> Result<()> {
        check(id, Location::default())?;
        self.modify(id, None)
    }

    fn set_metadata(&mut self, key: &str, value: &[u8]) -> Result<()> {
        self.require_healthy()?;
        self.metadata.set(key, value)
    }

    fn remove_metadata(&mut self, key: &str) {
        self.metadata.remove(key);
    }

    fn commit(&mut self, stamp: u64) -> Result<()> {
        self.require_complete()?;
        let result = self.write_commit(stamp);
        // Writing the changes empties `changed` and extends the directory before any page is written, so a retry
        // would commit pages that lost their edits.
        self.poisoned |= result.is_err();
        result
    }

    fn bulk_load(&mut self) -> Result<PagedLoader<'_>> {
        let changed = !self.changed.is_empty() || !self.new_entries.is_empty();
        let resume = match self.committed.state {
            LoadState::Loading { last_id } => last_id,
            LoadState::Complete if changed || self.dir.len() > 0 => {
                return Err(NodeStoreError::NotEmpty);
            }
            LoadState::Complete => {
                // a load dropped or cut short before its first checkpoint must not look complete
                let header = Header {
                    generation: self.committed.generation + 1,
                    state: LoadState::Loading { last_id: None },
                    ..self.committed
                };
                self.write_committed(header)?;
                None
            }
        };
        // blocks up to the last loaded one are complete
        let floor_block = resume.map(|id| id >> BLOCK_SHIFT);
        Ok(PagedLoader {
            store: self,
            resume,
            floor_block,
            last_id: None,
            slots: Slots::default(),
            block: Block::default(),
            page: PageFill::default(),
            pages_since_checkpoint: 0,
            checkpoint: None,
            pending: Vec::new(),
            pending_start: 0,
            tmp: Vec::new(),
            page_buf: Vec::with_capacity(PAGE_SIZE),
            finished: false,
        })
    }
}

impl PagedStore<ReadWrite> {
    fn write_commit(&mut self, stamp: u64) -> Result<()> {
        let generation = self.committed.generation + 1;
        let undo = self.write_changes(generation)?;
        let header = Header {
            generation,
            pages: self.allocated,
            dir_len: self.dir.len(),
            stamp,
            state: LoadState::Complete,
        };
        self.write_committed(header)?;
        if let Some(undo) = undo {
            // the commit is kept, so the old pages are not needed
            undo.set_len(0).at(&sidecar(&self.path, "undo"))?;
        }
        Ok(())
    }

    /// Fails once a commit or a growth of the file failed, until the store is opened again.
    fn require_healthy(&self) -> Result<()> {
        if self.poisoned {
            return Err(NodeStoreError::Poisoned(self.path.clone()));
        }
        Ok(())
    }

    fn require_complete(&self) -> Result<()> {
        self.require_healthy()?;
        match self.committed.state {
            LoadState::Complete => Ok(()),
            LoadState::Loading { .. } => Err(NodeStoreError::LoadUnfinished),
        }
    }

    fn modify(&mut self, id: u64, loc: Option<Location>) -> Result<()> {
        self.require_complete()?;
        let (num, bit) = block_of(id);
        let p = if let Some(di) = self.dir.find(num) {
            self.dir.pages[di]
        } else {
            // below all pages, or there are none yet: a page of its own
            if loc.is_none() {
                return Ok(());
            }
            let p = self.alloc_page()?;
            self.dir.insert(0, num, p);
            self.new_entries.push((num, p));
            self.changed.insert(p, (num, Vec::new()));
            p
        };
        if !self.changed.contains_key(&p) {
            let decoded = decode_page(self.page(p));
            self.changed.insert(p, decoded);
        }
        let blocks = &mut self.changed.get_mut(&p).expect("just decoded").1;
        let pos = blocks.partition_point(|b| b.num < num);
        match (blocks.get_mut(pos).filter(|b| b.num == num), loc) {
            (Some(b), Some(loc)) => {
                let r = rank(b.mask, bit);
                if b.mask & bit == 0 {
                    b.mask |= bit;
                    b.locs.insert(r, loc);
                } else {
                    b.locs[r] = loc;
                }
            }
            (Some(b), None) if b.mask & bit != 0 => {
                b.mask &= !bit;
                b.locs.remove(rank(b.mask, bit));
                if b.mask == 0 {
                    blocks.remove(pos);
                }
            }
            (None, Some(loc)) => blocks.insert(pos, Block::single(num, bit, loc)),
            _ => {}
        }
        Ok(())
    }

    /// Encodes the changed pages, moving blocks that do not fit to new pages filled to `fill`,
    /// as a bulk load fills them. Saves the old contents of the committed ones to the undo file,
    /// then writes them all. Returns the undo file, if there is one, to empty after the commit.
    fn write_changes(&mut self, generation: u64) -> Result<Option<File>> {
        let mut changed: Vec<(u32, (u64, Vec<Block>))> = self.changed.drain().collect();
        changed.sort_unstable_by_key(|&(p, _)| p);
        // each page on its own thread; the pages that did not fit, as runs of pages
        let fill_limit = self.fill_limit;
        let encoded: Vec<(u32, u64, Vec<EncodedPage>)> = changed
            .into_par_iter()
            .map(|(p, (first, blocks))| (p, first, encode_changed(first, &blocks, fill_limit)))
            .collect();
        let mut pages: Vec<(u32, Vec<u8>)> = Vec::with_capacity(encoded.len());
        for (p, first, runs) in encoded {
            let di = self
                .dir
                .find(first)
                .expect("a changed page is in the directory");
            for (k, (start, bytes)) in runs.into_iter().enumerate() {
                let q = if k == 0 {
                    p
                } else {
                    let q = self.alloc_page()?;
                    self.dir.insert(di + k, start, q);
                    self.new_entries.push((start, q));
                    q
                };
                pages.push((q, bytes));
            }
        }
        let old: Vec<u32> = pages
            .iter()
            .map(|&(p, _)| p)
            .filter(|&p| u64::from(p) < self.committed.pages)
            .collect();
        let undo = if old.is_empty() {
            None
        } else {
            Some(self.write_undo(generation, &old)?)
        };
        let mut page = vec![0_u8; PAGE_SIZE];
        for (p, bytes) in pages {
            page[..bytes.len()].copy_from_slice(&bytes);
            page[bytes.len()..].fill(0);
            write_at(&self.file, &page, u64::from(p) * PAGE_SIZE as u64).at(&self.path)?;
        }
        Ok(undo)
    }

    fn alloc_page(&mut self) -> Result<u32> {
        let p = u32::try_from(self.allocated).expect("more than 2^32 pages");
        self.allocated += 1;
        let grown = self.ensure_len(self.allocated);
        self.poisoned |= grown.is_err();
        grown.map(|()| p)
    }

    /// Grows the file to hold `pages` pages, and maps all of it.
    fn ensure_len(&mut self, pages: u64) -> Result<()> {
        let needed = pages * PAGE_SIZE as u64;
        if needed > self.file_len {
            let len = needed
                .max(self.file_len + GROW_MIN.max(self.file_len / 8))
                .next_multiple_of(PAGE_SIZE as u64);
            self.file.set_len(len).at(&self.path)?;
            self.file_len = len;
            self.map = map_file(&self.file).at(&self.path)?;
            self.advise(false);
        }
        Ok(())
    }

    /// Saves the old contents of committed `pages` before a commit changes them, and returns the
    /// file, to empty once the commit is kept.
    fn write_undo(&self, generation: u64, pages: &[u32]) -> Result<File> {
        let path = sidecar(&self.path, "undo");
        let existed = path.exists();
        let mut buf = Vec::with_capacity(24 + pages.len() * (PAGE_SIZE + 8));
        buf.extend_from_slice(UNDO_MAGIC);
        buf.extend_from_slice(&generation.to_le_bytes());
        buf.extend_from_slice(&(pages.len() as u64).to_le_bytes());
        for &p in pages {
            let start = p as usize * PAGE_SIZE;
            let old = &self.map[start..start + PAGE_SIZE];
            let at = buf.len();
            buf.extend_from_slice(&p.to_le_bytes());
            buf.extend_from_slice(old);
            let crc = crc(&buf[at..]);
            buf.extend_from_slice(&crc.to_le_bytes());
        }
        let mut f = File::create(&path).at(&path)?;
        f.write_all(&buf).and_then(|()| f.sync_all()).at(&path)?;
        if !existed {
            sync_dir(&path)?;
        }
        Ok(f)
    }

    /// Makes the pages written since the last commit durable, then `header`, which commits them.
    fn write_committed(&mut self, header: Header) -> Result<()> {
        let files = CommitFiles {
            file: &self.file,
            dir_file: &self.dir_file,
            path: &self.path,
        };
        files.commit(
            &self.new_entries,
            self.committed.dir_len,
            &header,
            &self.metadata,
        )?;
        self.new_entries.clear();
        self.committed = header;
        Ok(())
    }
}

/// The files a commit writes.
struct CommitFiles<'a> {
    file: &'a File,
    dir_file: &'a File,
    path: &'a Path,
}

impl CommitFiles<'_> {
    /// Makes the pages written since the last commit durable, then appends `entries` to the
    /// directory after its first `dir_len`, and writes `header`, which commits them.
    fn commit(
        &self,
        entries: &[(u64, u32)],
        dir_len: u64,
        header: &Header,
        metadata: &Metadata,
    ) -> Result<()> {
        self.file.sync_data().at(self.path)?;
        if !entries.is_empty() {
            let mut buf = Vec::with_capacity(DIR_ENTRY * entries.len());
            for &(start, p) in entries {
                buf.extend_from_slice(&start.to_le_bytes());
                buf.extend_from_slice(&p.to_le_bytes());
            }
            let at = (DIR_MAGIC.len() + DIR_ENTRY * usize::try_from(dir_len).unwrap()) as u64;
            let dir_path = sidecar(self.path, "dir");
            write_at(self.dir_file, &buf, at).at(&dir_path)?;
            self.dir_file.sync_data().at(&dir_path)?;
        }
        write_header(self.file, header, metadata, self.path)
    }
}

/// A bulk load checkpoint being made durable on another thread, and its header.
type Checkpoint = (JoinHandle<Result<()>>, Header);

/// Where a block is: decoded in a page changed since the last commit, or encoded in a page
enum Found<'a> {
    Changed(&'a Block),
    Page(&'a [u8], Head),
}

/// A page's first block, and its bytes
type EncodedPage = (u64, Vec<u8>);

/// Encodes a changed page whose first block is `first`, as one page if it fits, else as pages
/// filled to `fill_limit`. Returns each page's first block and bytes, with its CRC.
fn encode_changed(first: u64, blocks: &[Block], fill_limit: usize) -> Vec<EncodedPage> {
    let mut buf = Vec::new();
    // an upper bound of the encoded size, as a block's offsets must fit in u16
    let bound = PAGE_HEADER
        + blocks
            .iter()
            .map(|b| ENTRY + 19 + 8 * b.locs.len())
            .sum::<usize>();
    if u16::try_from(bound).is_ok() {
        encode_page(first, blocks, &mut buf);
        if buf.len() <= PAGE_SIZE {
            set_page_crc(&mut buf);
            return vec![(first, buf)];
        }
    }
    fill_pages(blocks, fill_limit)
        .into_iter()
        .enumerate()
        .map(|(k, run)| {
            let start = if k == 0 { first } else { blocks[run.start].num };
            encode_page(start, &blocks[run], &mut buf);
            set_page_crc(&mut buf);
            (start, buf.clone())
        })
        .collect()
}

/// Splits blocks that do not fit a page into runs that fill pages to `fill_limit`, as a bulk load
/// fills them; each run has at least one block.
fn fill_pages(blocks: &[Block], fill_limit: usize) -> Vec<Range<usize>> {
    let mut runs = Vec::new();
    let (mut start, mut size, mut tmp) = (0, PAGE_HEADER, Vec::new());
    for (i, b) in blocks.iter().enumerate() {
        tmp.clear();
        encode_block(&mut tmp, b, blocks[start].locs[0]);
        if i > start && size + ENTRY + tmp.len() > fill_limit {
            runs.push(start..i);
            start = i;
            tmp.clear();
            encode_block(&mut tmp, b, b.locs[0]);
            size = PAGE_HEADER;
        }
        size += ENTRY + tmp.len();
    }
    runs.push(start..blocks.len());
    runs
}

/// Starts a new store in an empty file: a header, and an empty directory file.
fn init(file: &File, path: &Path) -> Result<()> {
    // an undo file left from a deleted store must not apply to this one
    let undo = sidecar(path, "undo");
    match fs::remove_file(&undo) {
        Err(e) if e.kind() != ErrorKind::NotFound => return Err(e).at(&undo),
        _ => {}
    }
    write_dir(&sidecar(path, "dir"), &[])?;
    file.set_len(PAGE_SIZE as u64).at(path)?;
    let header = Header {
        generation: 1,
        pages: 1,
        dir_len: 0,
        stamp: 0,
        state: LoadState::Complete,
    };
    write_header(file, &header, &Metadata::default(), path)?;
    sync_dir(path)
}

/// Writes `header` and `metadata` over the older copy, and syncs it.
fn write_header(file: &File, header: &Header, metadata: &Metadata, path: &Path) -> Result<()> {
    let slot = (header.generation % 2) as usize * SLOT;
    write_at(file, &header.encode(metadata), slot as u64).at(path)?;
    file.sync_data().at(path)
}

/// Writes a whole directory file, replacing any, through a temporary file.
fn write_dir(dir_path: &Path, entries: &[(u64, u32)]) -> Result<()> {
    let tmp = sidecar(dir_path, "tmp");
    let mut buf = Vec::with_capacity(DIR_MAGIC.len() + DIR_ENTRY * entries.len());
    buf.extend_from_slice(DIR_MAGIC);
    for &(start, p) in entries {
        buf.extend_from_slice(&start.to_le_bytes());
        buf.extend_from_slice(&p.to_le_bytes());
    }
    let mut f = File::create(&tmp).at(&tmp)?;
    f.write_all(&buf).and_then(|()| f.sync_all()).at(&tmp)?;
    fs::rename(&tmp, dir_path).at(dir_path)?;
    sync_dir(dir_path)
}

/// The committed directory entries, or why they cannot be used.
fn read_dir(dir_path: &Path, header: &Header) -> Result<Vec<(u64, u32)>, String> {
    let bytes = fs::read(dir_path).map_err(|e| format!("cannot read the directory file: {e}"))?;
    let n = usize::try_from(header.dir_len).unwrap();
    if !bytes.starts_with(DIR_MAGIC) || bytes.len() < DIR_MAGIC.len() + DIR_ENTRY * n {
        return Err("the directory file is damaged or shorter than its header says".to_owned());
    }
    let entries: Vec<(u64, u32)> = bytes[DIR_MAGIC.len()..][..DIR_ENTRY * n]
        .as_chunks::<DIR_ENTRY>()
        .0
        .iter()
        .map(|e| (u64_at(e, 0), u32_at(e, 8)))
        .collect();
    // each page in the directory once, and each first block; fixing may have left out damaged pages
    let mut pages: Vec<u32> = entries.iter().map(|&(_, p)| p).collect();
    pages.sort_unstable();
    pages.dedup();
    let mut starts: Vec<u64> = entries.iter().map(|&(s, _)| s).collect();
    starts.sort_unstable();
    starts.dedup();
    let in_file = pages.first().is_none_or(|&p| p > 0)
        && pages.last().is_none_or(|&p| u64::from(p) < header.pages);
    if pages.len() != n
        || starts.len() != n
        || !in_file
        || starts
            .last()
            .is_some_and(|&s| s > MAX_NODE_ID >> BLOCK_SHIFT)
    {
        return Err("the directory file does not match the pages".to_owned());
    }
    Ok(entries)
}

/// The directory entries of pages whose CRC is right, and the IDs of the others.
fn check_pages(
    map: &[u8],
    entries: Vec<(u64, u32)>,
) -> (Vec<(u64, u32)>, Vec<RangeInclusive<u64>>) {
    let page = |p: u32| &map[p as usize * PAGE_SIZE..(p as usize + 1) * PAGE_SIZE];
    let (good, bad): (Vec<_>, Vec<_>) = entries.into_iter().partition(|&(start, p)| {
        let (stored, actual) = page_crcs(page(p));
        stored == actual && page_header(page(p)).first_block == start
    });
    let lost = lost_ranges(&good, bad.iter().map(|&(start, _)| Some(start)));
    (good, lost)
}

/// The directory entries of the committed pages whose CRC is right, from their headers, and the
/// IDs of the others.
fn scan_pages(map: &[u8], header: &Header) -> (Vec<(u64, u32)>, Vec<RangeInclusive<u64>>) {
    let (mut good, mut bad) = (Vec::new(), Vec::new());
    for p in 1..u32::try_from(header.pages).unwrap() {
        let page = &map[p as usize * PAGE_SIZE..(p as usize + 1) * PAGE_SIZE];
        let (stored, actual) = page_crcs(page);
        let start = page_header(page).first_block;
        if stored == actual && start <= MAX_NODE_ID >> BLOCK_SHIFT {
            good.push((start, p));
        } else {
            bad.push((start <= MAX_NODE_ID >> BLOCK_SHIFT).then_some(start));
        }
    }
    let lost = lost_ranges(&good, bad.into_iter());
    (good, lost)
}

/// The node IDs a damaged page may have held: from its first block, if known, to the next good
/// page's first block.
fn lost_ranges(
    good: &[(u64, u32)],
    bad: impl Iterator<Item = Option<u64>>,
) -> Vec<RangeInclusive<u64>> {
    let mut starts: Vec<u64> = good.iter().map(|&(s, _)| s).collect();
    starts.sort_unstable();
    let mut lost: Vec<_> = bad
        .map(|start| {
            let Some(start) = start else {
                return 0..=MAX_NODE_ID;
            };
            let next = starts[starts.partition_point(|&s| s <= start)..]
                .first()
                .map_or(MAX_NODE_ID, |&s| (s << BLOCK_SHIFT) - 1);
            (start << BLOCK_SHIFT)..=next
        })
        .collect();
    lost.sort_unstable_by_key(|r| (*r.start(), *r.end()));
    lost
}

/// The old page contents an interrupted commit of `generation` saved, if it saved all of them.
fn read_undo(path: &Path, generation: u64, pages: u64) -> Option<Vec<(u32, Vec<u8>)>> {
    let bytes = fs::read(path).ok()?;
    if !bytes.starts_with(UNDO_MAGIC) || bytes.len() < 24 || u64_at(&bytes, 8) != generation {
        return None;
    }
    let n = usize::try_from(u64_at(&bytes, 16)).ok()?;
    let record = 4 + PAGE_SIZE + 4;
    if bytes.len() != 24 + n * record {
        return None;
    }
    bytes[24..]
        .chunks_exact(record)
        .map(|r| {
            let p = u32_at(r, 0);
            let intact = crc(&r[..4 + PAGE_SIZE]) == u32_at(r, 4 + PAGE_SIZE);
            (intact && p > 0 && u64::from(p) < pages).then(|| (p, r[4..4 + PAGE_SIZE].to_vec()))
        })
        .collect()
}

/// Bulk loads a [`PagedStore`]; see [`BulkLoad`].
pub struct PagedLoader<'a> {
    store: &'a mut PagedStore<ReadWrite>,
    resume: Option<u64>,
    /// blocks up to this one are stored already
    floor_block: Option<u64>,
    last_id: Option<u64>,
    /// for checking the nodes of a resumed load against the store
    slots: Slots,
    /// the block being filled
    block: Block,
    /// the page being filled
    page: PageFill,
    pages_since_checkpoint: u64,
    /// the last checkpoint, if it may not be durable yet
    checkpoint: Option<Checkpoint>,
    /// pages written but not yet in the file, from page `pending_start`
    pending: Vec<u8>,
    pending_start: u32,
    /// for encoding a block, and a page
    tmp: Vec<u8>,
    page_buf: Vec<u8>,
    finished: bool,
}

/// A page being filled by a bulk load.
#[derive(Default)]
struct PageFill {
    first: u64,
    base: Location,
    blocks: Vec<u8>,
    table: Vec<(u32, usize)>,
    /// the last node ID in the page
    last_id: u64,
}

/// Blocks a bulk load encoded on another thread, with [`encode_detached`].
struct Encoded {
    bytes: Vec<u8>,
    /// per block: its number, first location, where its bytes end and its steps start, last ID
    blocks: Vec<(u64, Location, usize, usize, u64)>,
}

impl PagedLoader<'_> {
    /// Encodes `self.block` and adds it to the page being filled.
    fn add_block(&mut self) -> Result<()> {
        let b = &self.block;
        let last_id = (b.num << BLOCK_SHIFT) | u64::from(b.mask.ilog2());
        let (num, first) = (b.num, b.locs[0]);
        let mut tmp = mem::take(&mut self.tmp);
        tmp.clear();
        let steps = encode_detached(&mut tmp, &self.block);
        let res = self.add_encoded(num, first, &tmp, steps, last_id);
        self.tmp = tmp;
        res
    }

    /// Adds a block from [`encode_detached`] to the page being filled, or to a new page if it
    /// does not fit.
    fn add_encoded(
        &mut self,
        num: u64,
        first: Location,
        bytes: &[u8],
        steps: usize,
        last_id: u64,
    ) -> Result<()> {
        let page = &mut self.page;
        if !page.table.is_empty() {
            let size = PAGE_HEADER
                + ENTRY * (page.table.len() + 1)
                + page.blocks.len()
                + packed_len(bytes.len(), first, page.base);
            if size <= self.store.fill_limit {
                pack_block(&mut page.blocks, bytes, steps, first, page.base);
                let rel = u32::try_from(num - page.first).expect("node IDs are limited");
                page.table.push((rel, page.blocks.len()));
                page.last_id = last_id;
                return Ok(());
            }
            self.write_page()?;
        }
        let page = &mut self.page;
        (page.first, page.base, page.last_id) = (num, first, last_id);
        page.blocks.clear();
        pack_block(&mut page.blocks, bytes, steps, first, first);
        page.table.push((0, page.blocks.len()));
        Ok(())
    }

    /// Writes the page being filled, and keeps what was loaded if it is time for a checkpoint.
    fn write_page(&mut self) -> Result<()> {
        if self.page.table.is_empty() {
            return Ok(());
        }
        let page = &mut self.page;
        let start = PAGE_HEADER + ENTRY * page.table.len();
        let buf = &mut self.page_buf;
        buf.clear();
        buf.resize(start, 0);
        for (i, &(rel, end)) in page.table.iter().enumerate() {
            set_entry(buf, i, rel, start + end);
        }
        buf.extend_from_slice(&page.blocks);
        set_page_header(buf, page.first, page.table.len(), page.base);
        set_page_crc(buf);
        page.table.clear();

        let store = &mut *self.store;
        let p = u32::try_from(store.allocated).expect("more than 2^32 pages");
        store.allocated += 1;
        if self.pending.is_empty() {
            self.pending_start = p;
        }
        self.pending.extend_from_slice(buf);
        self.pending
            .resize(self.pending.len() + PAGE_SIZE - buf.len(), 0);
        store.dir.push(page.first, p);
        store.new_entries.push((page.first, p));
        if self.pending.len() >= WRITE_PAGES * PAGE_SIZE {
            self.write_pending()?;
        }
        self.pages_since_checkpoint += 1;
        if self.pages_since_checkpoint >= CHECKPOINT_PAGES {
            self.checkpoint(LoadState::Loading {
                last_id: Some(self.page.last_id),
            })?;
        }
        Ok(())
    }

    fn write_pending(&mut self) -> Result<()> {
        if !self.pending.is_empty() {
            let store = &mut *self.store;
            store.ensure_len(store.allocated)?;
            let offset = u64::from(self.pending_start) * PAGE_SIZE as u64;
            write_at(&store.file, &self.pending, offset).at(&store.path)?;
            self.pending.clear();
        }
        Ok(())
    }

    /// Commits the pages written so far, with `state`, on another thread, so that loading goes on
    /// while the disk catches up; the checkpoint before is waited for first.
    fn checkpoint(&mut self, state: LoadState) -> Result<()> {
        self.write_pending()?;
        self.wait_for_checkpoint()?;
        let store = &mut *self.store;
        let header = Header {
            generation: store.committed.generation + 1,
            pages: store.allocated,
            dir_len: store.dir.len(),
            stamp: store.committed.stamp,
            state,
        };
        let file = store.file.try_clone().at(&store.path)?;
        let dir_file = store.dir_file.try_clone().at(&store.path)?;
        let path = store.path.clone();
        let entries = mem::take(&mut store.new_entries);
        let (dir_len, metadata) = (store.committed.dir_len, store.metadata.clone());
        let thread = thread::spawn(move || {
            let files = CommitFiles {
                file: &file,
                dir_file: &dir_file,
                path: &path,
            };
            files.commit(&entries, dir_len, &header, &metadata)
        });
        self.checkpoint = Some((thread, header));
        self.pages_since_checkpoint = 0;
        Ok(())
    }

    /// Waits for the last checkpoint to be durable.
    fn wait_for_checkpoint(&mut self) -> Result<()> {
        if let Some((thread, header)) = self.checkpoint.take() {
            thread.join().expect("checkpoint thread panicked")?;
            self.store.committed = header;
        }
        Ok(())
    }

    /// Checks a node at or below the last one an interrupted load kept.
    fn check_resumed(&mut self, id: u64, loc: Location) -> Result<()> {
        let store = &*self.store;
        let stored = self.slots.get(id, |num, locs| store.load_block(num, locs));
        check_resumed(stored, id, loc)
    }

    /// Encodes `nodes`, whole blocks of them in ascending ID order, on many threads, and adds them.
    fn add_parallel(&mut self, nodes: &[(u64, Location)]) -> Result<()> {
        // the index of each block's first node, found on all threads
        let mut starts = vec![0];
        starts.par_extend(
            (1..nodes.len())
                .into_par_iter()
                .filter(|&i| nodes[i].0 >> BLOCK_SHIFT != nodes[i - 1].0 >> BLOCK_SHIFT),
        );
        starts.push(nodes.len());
        let blocks = starts.len() - 1;
        let chunks: Vec<&[usize]> = (0..blocks)
            .step_by(PARALLEL_BLOCKS)
            .map(|i| &starts[i..=(i + PARALLEL_BLOCKS).min(blocks)])
            .collect();
        let encoded: Vec<Encoded> = chunks
            .into_par_iter()
            .map(|starts| encode_blocks(nodes, starts))
            .collect();
        for chunk in encoded {
            let mut at = 0;
            for &(num, first, end, steps, last_id) in &chunk.blocks {
                self.add_encoded(num, first, &chunk.bytes[at..end], steps, last_id)?;
                at = end;
            }
        }
        Ok(())
    }
}

/// Encodes the blocks of `nodes` that start at the indexes in `starts`, the last one an end.
fn encode_blocks(nodes: &[(u64, Location)], starts: &[usize]) -> Encoded {
    let mut out = Encoded {
        bytes: Vec::with_capacity(starts.len() * 64),
        blocks: Vec::with_capacity(starts.len()),
    };
    let mut b = Block::default();
    for w in starts.windows(2) {
        let block = &nodes[w[0]..w[1]];
        b.num = block[0].0 >> BLOCK_SHIFT;
        b.mask = 0;
        b.locs.clear();
        for &(id, loc) in block {
            b.mask |= block_of(id).1;
            b.locs.push(loc);
        }
        let steps = encode_detached(&mut out.bytes, &b);
        let last_id = block[block.len() - 1].0;
        out.blocks
            .push((b.num, b.locs[0], out.bytes.len(), steps, last_id));
    }
    out
}

impl BulkLoad for PagedLoader<'_> {
    fn resume_after(&self) -> Option<u64> {
        self.resume
    }

    fn push(&mut self, id: u64, loc: Location) -> Result<()> {
        check(id, loc)?;
        check_order(self.last_id, id)?;
        self.last_id = Some(id);
        if self.resume.is_some_and(|r| id <= r) {
            return self.check_resumed(id, loc);
        }
        let (num, bit) = block_of(id);
        if self.floor_block.is_some_and(|f| num <= f) {
            // the interrupted load had finished this block
            return Err(NodeStoreError::ResumeMismatch(id));
        }
        if num != self.block.num && self.block.mask != 0 {
            self.add_block()?;
            self.block.mask = 0;
            self.block.locs.clear();
        }
        self.block.num = num;
        self.block.mask |= bit;
        self.block.locs.push(loc);
        Ok(())
    }

    fn push_batch(&mut self, nodes: &[(u64, Location)]) -> Result<()> {
        // the nodes of the first block, and of a block an interrupted load kept, one by one
        let first_new = nodes.partition_point(|&(id, _)| self.resume.is_some_and(|r| id <= r));
        let one_by_one = first_new
            + nodes[first_new..]
                .iter()
                .take_while(|&&(id, _)| {
                    nodes
                        .get(first_new)
                        .is_some_and(|&(f, _)| f >> BLOCK_SHIFT == id >> BLOCK_SHIFT)
                })
                .count();
        for &(id, loc) in &nodes[..one_by_one] {
            self.push(id, loc)?;
        }
        let rest = &nodes[one_by_one..];
        // the last block may go on in the next batch
        let last_block = rest.last().map(|&(id, _)| id >> BLOCK_SHIFT);
        let whole = rest.partition_point(|&(id, _)| Some(id >> BLOCK_SHIFT) != last_block);
        if whole < PARALLEL_NODES {
            for &(id, loc) in rest {
                self.push(id, loc)?;
            }
            return Ok(());
        }
        // on all threads, each from the node before its chunk; the first error in node order
        let first_error =
            rest.par_chunks(PARALLEL_NODES)
                .enumerate()
                .find_map_first(|(i, chunk)| {
                    let mut last = if i == 0 {
                        self.last_id
                    } else {
                        Some(rest[i * PARALLEL_NODES - 1].0)
                    };
                    chunk.iter().find_map(|&(id, loc)| {
                        let res = check(id, loc).and_then(|()| check_order(last, id));
                        last = Some(id);
                        res.err()
                    })
                });
        if let Some(e) = first_error {
            return Err(e);
        }
        if self
            .floor_block
            .is_some_and(|f| rest[0].0 >> BLOCK_SHIFT <= f)
        {
            return Err(NodeStoreError::ResumeMismatch(rest[0].0));
        }
        if self.block.mask != 0 {
            self.add_block()?;
            self.block.mask = 0;
            self.block.locs.clear();
        }
        self.add_parallel(&rest[..whole])?;
        self.last_id = Some(rest[whole - 1].0);
        for &(id, loc) in &rest[whole..] {
            self.push(id, loc)?;
        }
        Ok(())
    }

    fn finish(mut self, stamp: u64) -> Result<()> {
        if self.block.mask != 0 {
            self.add_block()?;
        }
        self.write_page()?;
        self.write_pending()?;
        self.wait_for_checkpoint()?;
        let store = &mut *self.store;
        let header = Header {
            generation: store.committed.generation + 1,
            pages: store.allocated,
            dir_len: store.dir.len(),
            stamp,
            state: LoadState::Complete,
        };
        store.write_committed(header)?;
        self.finished = true;
        Ok(())
    }
}

impl Drop for PagedLoader<'_> {
    /// Forgets the pages written since the last checkpoint.
    fn drop(&mut self) {
        if !self.finished {
            // an error leaves the checkpoint before it
            let _ = self.wait_for_checkpoint();
            let store = &mut *self.store;
            store.allocated = store.committed.pages;
            store
                .dir
                .truncate(usize::try_from(store.committed.dir_len).unwrap());
            store.new_entries.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;

    use rand::rngs::StdRng;
    use rand::{RngExt as _, SeedableRng as _};

    use super::*;
    use crate::nodes::file::tests::TestFile;
    use crate::nodes::store::tests::{build, check, nodes, random_location};

    /// Checks the directory: sorted, its index agrees with a search, and each page's blocks are in
    /// its range.
    fn check_dir(store: &PagedStore) {
        let dir = &store.dir;
        assert!(dir.starts.is_sorted());
        for &s in &dir.starts {
            for num in [s.saturating_sub(1), s, s + 1] {
                let found = dir.starts.partition_point(|&x| x <= num).checked_sub(1);
                assert_eq!(dir.find(num), found, "block {num}");
            }
        }
        for (i, &p) in dir.pages.iter().enumerate() {
            let (first, blocks) = store
                .changed
                .get(&p)
                .cloned()
                .unwrap_or_else(|| decode_page(store.page(p)));
            assert_eq!(first, dir.starts[i]);
            let next = dir.starts.get(i + 1).copied().unwrap_or(u64::MAX);
            assert!(blocks.iter().all(|b| b.num >= first && b.num < next));
        }
    }

    fn open(path: &Path, fill: f64) -> PagedStore {
        PagedStore::open_with(path, &PagedOpts::default().fill(fill)).unwrap()
    }

    #[test]
    fn uses_every_codec_and_mask() {
        let f = TestFile::new("codecs");
        let mut expected = nodes(&mut StdRng::seed_from_u64(1), 50_000);
        // full blocks too
        let top = *expected.keys().last().unwrap();
        expected.extend((1..=1000).map(|i| (top + i, Location::new(i32::try_from(i).unwrap(), 0))));
        let store = build::<PagedStore>(&f.0, &expected, 0, 50_000);
        let stats = store.stats();
        assert!(
            stats.codecs.iter().all(|&c| c > 0),
            "all codecs used: {stats:?}"
        );
        assert!(
            stats.masks.iter().all(|&c| c > 0),
            "all mask kinds used: {stats:?}"
        );
        assert_eq!(stats.nodes, expected.len() as u64);
        check(&store, &expected);
        check_dir(&store);
    }

    #[test]
    fn parallel_load_writes_the_same_pages() {
        let expected = nodes(&mut StdRng::seed_from_u64(9), 100_000);
        let (a, b) = (TestFile::new("par_one"), TestFile::new("par_many"));
        let one = build::<PagedStore>(&a.0, &expected, 0, 1);
        let many = build::<PagedStore>(&b.0, &expected, 0, 40_000);
        assert_eq!(one.stats(), many.stats());
        for (&p, &q) in one.dir.pages.iter().zip(&many.dir.pages) {
            assert_eq!(one.page(p), many.page(q), "page {p}");
        }
    }

    #[test]
    fn extreme_locations() {
        let f = TestFile::new("extreme");
        let corners = [
            (-900_000_000, -1_800_000_000),
            (900_000_000, 1_800_000_000),
            (0, 0),
            (-900_000_000, 1_800_000_000),
        ];
        let expected: BTreeMap<u64, Location> = (0..640_u64)
            .zip(corners.iter().cycle())
            .map(|(i, &c)| (i * 3 + (MAX_NODE_ID - 2000), Location::from(c)))
            .collect();
        let store = build::<PagedStore>(&f.0, &expected, 0, 100);
        check(&store, &expected);
        check_dir(&store);
    }

    #[test]
    fn splits_pages_under_changes() {
        let f = TestFile::new("splits");
        let mut rng = StdRng::seed_from_u64(2);
        let mut expected = nodes(&mut rng, 20_000);
        drop(build::<PagedStore>(&f.0, &expected, 0, 1000));
        let mut store = open(&f.0, 0.5);
        let max = *expected.keys().last().unwrap();
        for i in 0..60_000 {
            let id = rng.random_range(1..max + 5000);
            let loc = match rng.random_range(0..10) {
                0..=1 => None,
                // a jump far away grows the block, so pages fill and split
                2 => Some(random_location(&mut rng)),
                _ => {
                    let old = expected.get(&id).copied().unwrap_or_default();
                    Some(Location::new(
                        old.lat + rng.random_range(-50..50),
                        old.lon + rng.random_range(-50..50),
                    ))
                }
            };
            if let Some(loc) = loc {
                store.set(id, loc).unwrap();
                expected.insert(id, loc);
            } else {
                store.remove(id).unwrap();
                expected.remove(&id);
            }
            if i % 20_000 == 0 {
                check(&store, &expected);
                check_dir(&store);
                store.commit(i).unwrap();
            }
        }
        store.commit(1).unwrap();
        check(&store, &expected);
        check_dir(&store);
        let pages = store.stats().pages;
        drop(store);
        let store = open(&f.0, 0.5);
        assert_eq!(store.stats().pages, pages);
        check(&store, &expected);
        check_dir(&store);
    }

    #[test]
    fn appending_new_ids_fills_pages_as_a_bulk_load() {
        // sparse blocks, and full blocks that overflow a nearly full page as they grow
        for (fill, seed) in [(0.8, 3), (0.97, 4)] {
            let f = TestFile::new(&format!("append_{fill}"));
            let expected = nodes(&mut StdRng::seed_from_u64(seed), 30_000);
            let mut store = open(&f.0, fill);
            for (&id, &loc) in &expected {
                store.set(id, loc).unwrap();
            }
            store.commit(0).unwrap();
            check(&store, &expected);
            let built = TestFile::new(&format!("append_built_{fill}"));
            let mut by_build = open(&built.0, fill);
            let mut load = by_build.bulk_load().unwrap();
            for (&id, &loc) in &expected {
                load.push(id, loc).unwrap();
            }
            load.finish(0).unwrap();
            let (by_set, by_build) = (store.stats().pages, by_build.stats().pages);
            assert!(
                by_set <= by_build + 1,
                "fill {fill}: {by_set} pages by set, {by_build} by bulk load"
            );
        }
    }

    #[test]
    fn set_below_first_page_and_far_apart() {
        let f = TestFile::new("below");
        let mut store = open(&f.0, 0.97);
        store.remove(5).unwrap();
        let mut expected = BTreeMap::new();
        for id in [100_000, 5000, 64, 63, 0, 1_000_000, MAX_NODE_ID, 1 << 30] {
            let loc = Location::new(i32::try_from(id % 1000).unwrap(), -1);
            store.set(id, loc).unwrap();
            expected.insert(id, loc);
            check(&store, &expected);
            check_dir(&store);
        }
        for id in [0, 63, 64] {
            store.remove(id).unwrap();
            expected.remove(&id);
        }
        store.commit(0).unwrap();
        check(&store, &expected);
        check_dir(&store);
    }

    /// A store with changes committed over a bulk load, and the nodes it then has
    fn store_with_changes(path: &Path, seed: u64) -> (PagedStore, BTreeMap<u64, Location>) {
        let mut rng = StdRng::seed_from_u64(seed);
        let mut expected = nodes(&mut rng, 20_000);
        let mut store = build::<PagedStore>(path, &expected, 1, 5000);
        for _ in 0..2000 {
            let (id, loc) = (rng.random_range(1..30_000), random_location(&mut rng));
            store.set(id, loc).unwrap();
            expected.insert(id, loc);
        }
        store.commit(2).unwrap();
        (store, expected)
    }

    /// Changes a store, and writes them as a commit does up to the header, as if it then crashed.
    fn crash_during_commit(store: &mut PagedStore, rng: &mut StdRng) {
        for _ in 0..3000 {
            store
                .set(rng.random_range(1..40_000), random_location(rng))
                .unwrap();
        }
        let generation = store.committed.generation + 1;
        store.write_changes(generation).unwrap();
        store.file.sync_data().unwrap();
    }

    #[test]
    fn rolls_back_an_interrupted_commit() {
        let f = TestFile::new("rollback");
        let (mut store, expected) = store_with_changes(&f.0, 5);
        crash_during_commit(&mut store, &mut StdRng::seed_from_u64(6));
        drop(store);
        assert!(matches!(
            PagedStore::open_read_only(&f.0),
            Err(NodeStoreError::NeedsRecovery(_))
        ));
        let store = PagedStore::open(&f.0).unwrap();
        assert_eq!(store.stamp(), 2);
        check(&store, &expected);
        drop(store);
        // the rolled back commit's undo applies no more
        check(&PagedStore::open_read_only(&f.0).unwrap(), &expected);
    }

    #[test]
    fn stats_split_pages_grown_past_their_size() {
        let f = TestFile::new("stats_grown");
        let mut store = PagedStore::open(&f.0).unwrap();
        let mut load = store.bulk_load().unwrap();
        // one node per block, so a page holds many blocks
        for k in 0..4000 {
            load.push(k * 64 + 1, Location::new(1, 1)).unwrap();
        }
        load.finish(1).unwrap();
        let before = store.stats();
        // filling every block with scattered locations, which hardly compress, grows each page far
        // past 64 KiB until the commit splits it
        for id in 1..4000 * 64_u64 {
            let h = id.wrapping_mul(0x9E37_79B9_7F4A_7C15);
            let lat = i32::try_from(h % 1_800_000_001).unwrap() - 900_000_000;
            let lon = i32::try_from((h >> 32) % 1_800_000_001).unwrap() - 900_000_000;
            store.set(id, Location::new(lat, lon)).unwrap();
        }
        let after = store.stats();
        assert_eq!(after.nodes, 4000 * 64 - 1);
        assert!(after.used_bytes > 16 * 65_536, "{before:?} {after:?}");
        // the commit writes the pages as counted; only the file has grown
        store.commit(2).unwrap();
        let committed = store.stats();
        assert_eq!(
            committed,
            PagedStats {
                file_bytes: committed.file_bytes,
                ..after
            }
        );
    }

    #[test]
    fn a_failed_commit_poisons_the_store_until_it_is_reopened() {
        let f = TestFile::new("failed_commit");
        let (mut store, expected) = store_with_changes(&f.0, 9);
        let undo = sidecar(&f.0, "undo");
        // An undo file that cannot be created fails the commit after its changes were taken from the store.
        let _ = fs::remove_file(&undo);
        fs::create_dir_all(&undo).unwrap();
        for id in 1..2000 {
            store.set(id, Location::new(1, 1)).unwrap();
        }
        store.commit(3).unwrap_err();
        assert!(matches!(
            store.set(1, Location::new(2, 2)),
            Err(NodeStoreError::Poisoned(_))
        ));
        assert!(matches!(store.commit(3), Err(NodeStoreError::Poisoned(_))));
        assert!(matches!(
            store.set_metadata("source", b"x"),
            Err(NodeStoreError::Poisoned(_))
        ));
        drop(store);
        fs::remove_dir(&undo).unwrap();
        let store = PagedStore::open(&f.0).unwrap();
        assert_eq!(store.stamp(), 2);
        check(&store, &expected);
    }

    #[test]
    fn a_torn_header_leaves_the_last_commit() {
        let f = TestFile::new("torn_header");
        let (mut store, expected) = store_with_changes(&f.0, 7);
        crash_during_commit(&mut store, &mut StdRng::seed_from_u64(8));
        // the new header copy, half written
        let header = Header {
            generation: store.committed.generation + 1,
            stamp: 3,
            ..store.committed
        };
        let slot = (header.generation % 2) as usize * SLOT;
        write_at(
            &store.file,
            &header.encode(&store.metadata)[..100],
            slot as u64,
        )
        .unwrap();
        drop(store);
        let store = PagedStore::open(&f.0).unwrap();
        assert_eq!(store.stamp(), 2);
        check(&store, &expected);
    }

    #[test]
    fn fixes_a_missing_directory() {
        let f = TestFile::new("no_dir");
        let (store, expected) = store_with_changes(&f.0, 9);
        drop(store);
        fs::remove_file(sidecar(&f.0, "dir")).unwrap();
        assert!(matches!(
            PagedStore::open(&f.0),
            Err(NodeStoreError::NeedsFix { .. })
        ));
        assert!(matches!(
            PagedStore::open_read_only(&f.0),
            Err(NodeStoreError::NeedsFix { .. })
        ));
        let (store, report) = PagedStore::open_and_fix(&f.0).unwrap();
        assert!(report.rebuilt_index && report.lost.is_empty(), "{report:?}");
        check(&store, &expected);
        drop(store);
        let (store, report) = PagedStore::open_and_fix(&f.0).unwrap();
        assert!(report.is_clean());
        check(&store, &expected);
    }

    #[test]
    fn fixing_drops_a_damaged_page() {
        let f = TestFile::new("damaged");
        let (store, mut expected) = store_with_changes(&f.0, 10);
        let p = store.dir.pages[3];
        drop(store);
        let mut bytes = fs::read(&f.0).unwrap();
        bytes[p as usize * PAGE_SIZE + 100] ^= 0x40;
        fs::write(&f.0, &bytes).unwrap();
        let (store, report) = PagedStore::open_and_fix(&f.0).unwrap();
        assert!(!report.rebuilt_index);
        assert_eq!(report.lost.len(), 1, "{report:?}");
        let before = expected.len();
        expected.retain(|id, _| !report.lost[0].contains(id));
        assert!(expected.len() < before, "the damaged page had nodes");
        check(&store, &expected);
        drop(store);
        check(&PagedStore::open(&f.0).unwrap(), &expected);
    }

    #[test]
    fn rejects_other_versions() {
        let f = TestFile::new("version");
        drop(PagedStore::open(&f.0).unwrap());
        let mut bytes = fs::read(&f.0).unwrap();
        for slot in [0, SLOT] {
            if bytes[slot..].starts_with(MAGIC) {
                bytes[slot + 7] = b'2';
            }
        }
        fs::write(&f.0, &bytes).unwrap();
        assert!(matches!(
            PagedStore::open(&f.0),
            Err(NodeStoreError::UnsupportedVersion { .. })
        ));
    }

    #[test]
    fn rejects_a_load_that_resumes_into_a_kept_block() {
        let f = TestFile::new("floor");
        let expected = nodes(&mut StdRng::seed_from_u64(11), 20_000);
        let all: Vec<_> = expected.iter().map(|(&k, &v)| (k, v)).collect();
        let mut store = PagedStore::open(&f.0).unwrap();
        let mut load = store.bulk_load().unwrap();
        load.push_batch(&all).unwrap();
        drop(load);
        let mut load = store.bulk_load().unwrap();
        let last = load.resume_after().unwrap();
        // a node the first load did not have, in the last block it kept
        let id = (last | 63).min(last + 1);
        if !expected.contains_key(&id) && id >> BLOCK_SHIFT == last >> BLOCK_SHIFT {
            for &(i, loc) in all.iter().take_while(|&&(i, _)| i <= last) {
                load.push(i, loc).unwrap();
            }
            assert!(matches!(
                load.push(id, Location::new(1, 1)),
                Err(NodeStoreError::ResumeMismatch(_))
            ));
        }
    }
}
