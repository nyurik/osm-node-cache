//! A compact node location cache in one memory-mapped file, changed in place.
//!
//! Blocks of locations, encoded as in [`crate::codec`], are stored in ID order in 4 KiB pages.
//! A bulk load fills pages only to `fill`, so most later changes
//! fit in place. A page that overflows is split, and its upper blocks move to a new page at the end
//! of the file.
//!
//! The first block of each page is kept in an in-memory directory, which [`PagedFileCache::flush`]
//! saves next to the data file (`<file>.dir`). A cache changed and not flushed has its directory
//! rebuilt from the pages when opened.
//!
//! Reads take `&self` and can run on many threads; changes take `&mut self`.

use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use memmap2::MmapMut;

use crate::codec::{
    block_of, decode_page, encode_block, encode_page, entry, find_in_page, page_header, rank,
    read_block, set_entry, set_page_header, split_point, u32_at, u64_at, Block, Head, Location,
    Slots, BLOCK_LEN, ENTRY, PAGE_HEADER,
};
use crate::traits::open_cache_file;
use crate::{NodeBuilder, NodeReader, NodeStore, OsmNodeCacheError, OsmNodeCacheResult};

const PAGE_SIZE: usize = 4096;
const MAGIC: &[u8; 8] = b"ONCPAGE1";
const VERSION: u32 = 2;
const DIR_MAGIC: &[u8; 8] = b"ONCDIR01";
/// The data file grows by at least this much at a time.
const GROW_MIN: u64 = if cfg!(test) { 1 << 16 } else { 64 << 20 };
/// The directory index has an entry per 2^8 blocks (16,384 IDs).
const BUCKET_SHIFT: u32 = 8;

/// Options for [`PagedFileCache`].
#[derive(Clone, Debug)]
pub struct PagedFileCacheOpts {
    fill: f64,
}

impl Default for PagedFileCacheOpts {
    fn default() -> Self {
        // on Europe, a synthetic day of edits splits about 30 of 3.9 million pages at this fill
        Self { fill: 0.97 }
    }
}

impl PagedFileCacheOpts {
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

/// Counts over all pages, from [`PagedFileCache::stats`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PagedStats {
    pub nodes: u64,
    pub blocks: u64,
    pub pages: u64,
    /// bytes used in the pages, headers included
    pub used_bytes: u64,
    /// size of the data file
    pub file_bytes: u64,
    /// blocks with all 64 IDs present, a single run of them, or any other mix
    pub masks: [u64; 3],
    /// blocks with plain integers, varints, or packed steps
    pub codecs: [u64; 3],
}

/// The first block of each data page, in block order.
#[derive(Default)]
struct Directory {
    starts: Vec<u64>,
    pages: Vec<u32>,
    /// for each bucket of blocks, the index of the first page starting in it or later
    index: Vec<u32>,
    changed: bool,
}

impl Directory {
    fn new(starts: Vec<u64>, pages: Vec<u32>) -> Self {
        let mut dir = Self {
            starts,
            pages,
            ..Self::default()
        };
        dir.extend_index();
        dir
    }

    /// The index of the page that would hold block `num`, if any page could
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
        self.changed = true;
        // pages starting in later buckets have moved up by one
        let later = usize::try_from(start >> BUCKET_SHIFT).expect("too many blocks") + 1;
        for i in self.index.iter_mut().skip(later) {
            *i += 1;
        }
        self.extend_index();
    }

    fn push(&mut self, start: u64, page: u32) {
        self.insert(self.starts.len(), start, page);
    }

    /// Adds index entries up to the bucket after the last page's.
    fn extend_index(&mut self) {
        let Some(&last) = self.starts.last() else {
            return;
        };
        let buckets = usize::try_from(last >> BUCKET_SHIFT).expect("too many blocks") + 2;
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

    /// Reads a saved directory, if there is one for a data file of `allocated` pages.
    fn read(path: &Path, allocated: u64) -> Option<Self> {
        let mut r = BufReader::new(File::open(path).ok()?);
        let mut head = [0_u8; 24];
        r.read_exact(&mut head).ok()?;
        let n = u64_at(&head, 16);
        if &head[..8] != DIR_MAGIC || u64_at(&head, 8) != allocated || n >= allocated {
            return None;
        }
        let mut entry = [0_u8; 12];
        let (mut starts, mut pages) = (Vec::new(), Vec::new());
        for _ in 0..n {
            r.read_exact(&mut entry).ok()?;
            starts.push(u64_at(&entry, 0));
            pages.push(u32_at(&entry, 8));
        }
        Some(Self::new(starts, pages))
    }

    /// Saves the directory through a temporary file, so a crash leaves the old one or the new one.
    fn write(&mut self, path: &Path, allocated: u64) -> io::Result<()> {
        let tmp = path.with_extension("dir.tmp");
        let mut w = BufWriter::new(File::create(&tmp)?);
        w.write_all(DIR_MAGIC)?;
        w.write_all(&allocated.to_le_bytes())?;
        w.write_all(&(self.starts.len() as u64).to_le_bytes())?;
        for (s, p) in self.starts.iter().zip(&self.pages) {
            w.write_all(&s.to_le_bytes())?;
            w.write_all(&p.to_le_bytes())?;
        }
        w.into_inner()?.sync_all()?;
        fs::rename(&tmp, path)?;
        self.changed = false;
        Ok(())
    }
}

/// A writable, compressed file cache of node locations; the module documentation has the format.
pub struct PagedFileCache {
    path: PathBuf,
    file: File,
    mmap: MmapMut,
    /// pages allocated in the file, the header page included
    allocated: u64,
    fill_limit: usize,
    stamp: u64,
    dir: Directory,
    /// changed since the last flush, which the file header then says
    dirty: bool,
    /// for encoding a page
    buf: Vec<u8>,
}

/// Reads locations for one thread, keeping the last few blocks it read decoded, as a way's nodes
/// are mostly in the same few blocks.
pub struct PagedReader<'a> {
    cache: &'a PagedFileCache,
    slots: Slots,
}

/// Bulk loads a new [`PagedFileCache`] from locations in ascending ID order.
pub struct PagedBuilder {
    cache: PagedFileCache,
    last_id: Option<u64>,
    block: Block,
    /// the page being filled: its first block, base location, encoded blocks, and table
    first: u64,
    base: Location,
    blocks: Vec<u8>,
    table: Vec<(u16, usize)>,
    tmp: Vec<u8>,
}

impl PagedFileCache {
    /// Creates an empty cache, replacing any file at `path`.
    pub fn create<P: AsRef<Path>>(path: P, opts: &PagedFileCacheOpts) -> OsmNodeCacheResult<Self> {
        let path = path.as_ref();
        let file = open_cache_file(path, true, true)?;
        file.set_len(GROW_MIN)?;
        let _ = fs::remove_file(dir_path(path));
        let mmap = unsafe { MmapMut::map_mut(&file)? };
        let mut cache = Self::new(path, file, mmap, opts);
        cache.dir.changed = true;
        cache.write_header(false)?;
        Ok(cache)
    }

    /// Creates a cache at `path`, to be loaded in ascending ID order.
    pub fn builder<P: AsRef<Path>>(
        path: P,
        opts: &PagedFileCacheOpts,
    ) -> OsmNodeCacheResult<PagedBuilder> {
        Ok(PagedBuilder {
            cache: Self::create(path, opts)?,
            last_id: None,
            block: Block::default(),
            first: 0,
            base: Location::default(),
            blocks: Vec::with_capacity(PAGE_SIZE),
            table: Vec::new(),
            tmp: Vec::with_capacity(PAGE_SIZE),
        })
    }

    /// Opens an existing cache. Changes and new pages use `opts.fill`.
    pub fn open<P: AsRef<Path>>(path: P, opts: &PagedFileCacheOpts) -> OsmNodeCacheResult<Self> {
        let path = path.as_ref();
        let file = open_cache_file(path, false, false)?;
        let mmap = unsafe { MmapMut::map_mut(&file)? };
        let (allocated, stamp, clean) = read_header(&mmap)
            .map_err(|e| OsmNodeCacheError::InvalidFormat(path.to_path_buf(), e.to_string()))?;
        let dir = if clean {
            Directory::read(&dir_path(path), allocated)
        } else {
            None
        };
        let mut cache = Self::new(path, file, mmap, opts);
        (cache.allocated, cache.stamp, cache.dirty) = (allocated, stamp, !clean);
        match dir {
            Some(dir) => cache.dir = dir,
            None => cache.rebuild_directory()?,
        }
        Ok(cache)
    }

    fn new(path: &Path, file: File, mmap: MmapMut, opts: &PagedFileCacheOpts) -> Self {
        let cache = Self {
            path: path.to_path_buf(),
            file,
            mmap,
            allocated: 1,
            fill_limit: opts.fill_limit(),
            stamp: 0,
            dir: Directory::default(),
            dirty: true,
            buf: Vec::with_capacity(2 * PAGE_SIZE),
        };
        cache.advise(false);
        cache
    }

    /// Tells the kernel how pages are about to be read: `sequential` for a pass over all of them in
    /// file order, otherwise lookups all over the file, where reading ahead of the page that faulted
    /// only evicts pages that are still needed.
    fn advise(&self, sequential: bool) {
        // only a hint, so a kernel that does not take it changes nothing
        #[cfg(unix)]
        let _ = self.mmap.advise(if sequential {
            memmap2::Advice::Sequential
        } else {
            memmap2::Advice::Random
        });
        #[cfg(not(unix))]
        let _ = sequential;
    }

    /// The location of node `id`, if it is stored. A [`PagedReader`] per thread reads many faster.
    #[must_use]
    pub fn get(&self, id: u64) -> Option<Location> {
        let (num, bit) = block_of(id);
        let (page, head) = self.find_block(num)?;
        (head.mask & bit != 0).then(|| head.location(page, rank(head.mask, bit)))
    }

    /// A reader for one thread.
    #[must_use]
    pub fn reader(&self) -> PagedReader<'_> {
        PagedReader {
            cache: self,
            slots: Slots::default(),
        }
    }

    /// Stores the location of node `id`, replacing any earlier one.
    pub fn set(&mut self, id: u64, loc: Location) -> OsmNodeCacheResult<()> {
        self.modify(id, Some(loc))
    }

    /// Removes node `id`, if it is stored.
    pub fn remove(&mut self, id: u64) -> OsmNodeCacheResult<()> {
        self.modify(id, None)
    }

    /// A number saved with the cache, such as the replication sequence its locations reflect.
    #[must_use]
    pub fn stamp(&self) -> u64 {
        self.stamp
    }

    pub fn set_stamp(&mut self, stamp: u64) -> OsmNodeCacheResult<()> {
        self.mark_dirty()?;
        self.stamp = stamp;
        Ok(())
    }

    /// Writes all changes and the directory to disk, and marks the file as consistent.
    pub fn flush(&mut self) -> OsmNodeCacheResult<()> {
        if self.dirty {
            self.mmap.flush()?;
            if self.dir.changed {
                self.dir.write(&dir_path(&self.path), self.allocated)?;
            }
            self.write_header(true)?;
            self.dirty = false;
        }
        Ok(())
    }

    /// Calls `f` with every stored node, in ID order.
    pub fn for_each(&self, mut f: impl FnMut(u64, Location)) {
        self.advise(true);
        for &p in &self.dir.pages {
            for b in decode_page(self.page(p)).1 {
                b.nodes().for_each(|(id, loc)| f(id, loc));
            }
        }
        self.advise(false);
    }

    /// Counts nodes, blocks and bytes over all pages.
    #[must_use]
    pub fn stats(&self) -> PagedStats {
        let mut s = PagedStats {
            pages: self.dir.pages.len() as u64,
            file_bytes: self.mmap.len() as u64,
            ..PagedStats::default()
        };
        self.advise(true);
        for &p in &self.dir.pages {
            let page = self.page(p);
            let h = page_header(page);
            s.used_bytes += h.used as u64;
            for i in 0..h.n_blocks {
                let head = read_block(page, &h, i);
                s.blocks += 1;
                s.nodes += u64::from(head.mask.count_ones());
                s.masks[usize::from(head.mask_kind)] += 1;
                s.codecs[usize::from(head.codec)] += 1;
            }
        }
        self.advise(false);
        s
    }

    fn page(&self, p: u32) -> &[u8] {
        let start = p as usize * PAGE_SIZE;
        &self.mmap[start..start + PAGE_SIZE]
    }

    /// The page holding block `num`, and the block's header, if the block is stored
    fn find_block(&self, num: u64) -> Option<(&[u8], Head)> {
        let page = self.page(self.dir.pages[self.dir.find(num)?]);
        Some((page, find_in_page(page, num)?))
    }

    fn modify(&mut self, id: u64, loc: Option<Location>) -> OsmNodeCacheResult<()> {
        let (num, bit) = block_of(id);
        let Some(di) = self.dir.find(num) else {
            // below all pages, or there are none yet: a page of its own
            let Some(loc) = loc else { return Ok(()) };
            self.mark_dirty()?;
            return self.insert_page(0, &[Block::single(num, bit, loc)]);
        };
        let (first_block, mut blocks) = decode_page(self.page(self.dir.pages[di]));
        let pos = blocks.partition_point(|b| b.num < num);
        let mut appended = false;
        match (blocks.get_mut(pos).filter(|b| b.num == num), loc) {
            (Some(b), Some(loc)) => {
                let r = rank(b.mask, bit);
                if b.mask & bit == 0 {
                    b.mask |= bit;
                    b.locs.insert(r, loc);
                } else if b.locs[r] == loc {
                    return Ok(());
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
            (None, Some(loc)) => {
                let block = Block::single(num, bit, loc);
                if num - first_block > u64::from(u16::MAX) {
                    // too far above the page's first block for its table
                    self.mark_dirty()?;
                    return self.insert_page(di + 1, &[block]);
                }
                appended = pos == blocks.len() && pos > 0;
                blocks.insert(pos, block);
            }
            _ => return Ok(()),
        }
        self.mark_dirty()?;
        encode_page(first_block, &blocks, &mut self.buf);
        if appended && self.buf.len() > self.fill_limit {
            // a new last block of a page past `fill` starts a new page, as in a bulk load
            let block = blocks.pop().expect("the new block");
            return self.insert_page(di + 1, &[block]);
        }
        self.store(di, first_block, &blocks, pos + 1 >= blocks.len())
    }

    /// Writes `blocks`, encoded in `self.buf`, as the page at directory index `di`, moving the
    /// upper blocks to new pages if they do not fit. If the last block changed, only it moves, so
    /// appending in ID order fills pages.
    fn store(
        &mut self,
        di: usize,
        first_block: u64,
        blocks: &[Block],
        last_changed: bool,
    ) -> OsmNodeCacheResult<()> {
        if self.buf.len() <= PAGE_SIZE {
            self.write_page(self.dir.pages[di]);
            return Ok(());
        }
        let k = if last_changed && blocks.len() > 1 {
            blocks.len() - 1
        } else {
            split_point(blocks)
        };
        // upper blocks go first; if a crash leaves them in both pages, opening drops the lower copy
        self.insert_page(di + 1, &blocks[k..])?;
        encode_page(first_block, &blocks[..k], &mut self.buf);
        self.store(di, first_block, &blocks[..k], false)
    }

    /// Writes `blocks` to a new page at directory index `di`.
    fn insert_page(&mut self, di: usize, blocks: &[Block]) -> OsmNodeCacheResult<()> {
        let (first, p) = (blocks[0].num, self.alloc_page()?);
        self.dir.insert(di, first, p);
        encode_page(first, blocks, &mut self.buf);
        self.store(di, first, blocks, false)
    }

    /// Copies `self.buf` into page `p`, clearing the rest of the page.
    fn write_page(&mut self, p: u32) {
        let start = p as usize * PAGE_SIZE;
        let page = &mut self.mmap[start..start + PAGE_SIZE];
        page[..self.buf.len()].copy_from_slice(&self.buf);
        page[self.buf.len()..].fill(0);
    }

    fn alloc_page(&mut self) -> OsmNodeCacheResult<u32> {
        let needed = (self.allocated + 1) * PAGE_SIZE as u64;
        let len = self.mmap.len() as u64;
        if needed > len {
            let new_len = needed
                .max(len + GROW_MIN.max(len / 8))
                .next_multiple_of(PAGE_SIZE as u64);
            self.file.set_len(new_len)?;
            self.mmap = unsafe { MmapMut::map_mut(&self.file)? };
            self.advise(false);
        }
        let p = u32::try_from(self.allocated).expect("more than 2^32 pages");
        self.allocated += 1;
        Ok(p)
    }

    /// Marks the file changed before the first change after a flush, so opening detects a crash.
    fn mark_dirty(&mut self) -> OsmNodeCacheResult<()> {
        if !self.dirty {
            self.write_header(false)?;
            self.dirty = true;
        }
        Ok(())
    }

    /// Writes the header page: magic, version, page size and block IDs (u32 each), pages allocated
    /// (u64 at 24), the stamp (u64 at 32), and whether the file is clean (byte 40).
    fn write_header(&mut self, clean: bool) -> OsmNodeCacheResult<()> {
        let h = &mut self.mmap[..PAGE_SIZE];
        h.fill(0);
        h[..8].copy_from_slice(MAGIC);
        h[8..12].copy_from_slice(&VERSION.to_le_bytes());
        h[12..16].copy_from_slice(&u32::try_from(PAGE_SIZE).unwrap().to_le_bytes());
        h[16..20].copy_from_slice(&u32::try_from(BLOCK_LEN).unwrap().to_le_bytes());
        h[24..32].copy_from_slice(&self.allocated.to_le_bytes());
        h[32..40].copy_from_slice(&self.stamp.to_le_bytes());
        h[40] = u8::from(clean);
        self.mmap.flush_range(0, PAGE_SIZE)?;
        Ok(())
    }

    /// Rebuilds the directory from the page headers, reading the whole file, as the header's page
    /// count may be older than the pages. Blocks left in two pages by a crash during a split are
    /// dropped from the lower one.
    fn rebuild_directory(&mut self) -> OsmNodeCacheResult<()> {
        let pages = u32::try_from(self.mmap.len() / PAGE_SIZE).expect("more than 2^32 pages");
        self.advise(true);
        let mut entries: Vec<(u64, u32)> = (1..pages)
            .filter_map(|p| {
                let h = page_header(self.page(p));
                // a page allocated and never written is all zeros
                (h.used >= PAGE_HEADER).then_some((h.first_block, p))
            })
            .collect();
        self.advise(false);
        entries.sort_unstable();
        for w in entries.windows(2) {
            let ((first, p), (next, _)) = (w[0], w[1]);
            let page = self.page(p);
            let h = page_header(page);
            let last = h
                .n_blocks
                .checked_sub(1)
                .map(|i| first + u64::from(entry(page, i).0));
            if last.is_some_and(|last| last >= next) {
                let (_, mut blocks) = decode_page(page);
                blocks.retain(|b| b.num < next);
                encode_page(first, &blocks, &mut self.buf);
                self.write_page(p);
            }
        }
        self.allocated = entries
            .iter()
            .map(|&(_, p)| u64::from(p) + 1)
            .max()
            .unwrap_or(1);
        let (starts, pages) = entries.into_iter().unzip();
        self.dir = Directory::new(starts, pages);
        self.dir.changed = true;
        self.mark_dirty()
    }
}

impl Drop for PagedFileCache {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            let _ = self.flush();
        }
    }
}

impl PagedReader<'_> {
    /// The location of node `id`, if it is stored.
    #[inline]
    pub fn get(&mut self, id: u64) -> Option<Location> {
        let cache = self.cache;
        self.slots.get(id, |num, locs| {
            cache.find_block(num).map_or(0, |(page, head)| {
                head.decode_into(page, locs);
                head.mask
            })
        })
    }
}

impl PagedBuilder {
    /// Adds the location of node `id`, which must be above all IDs added before.
    pub fn push(&mut self, id: u64, loc: Location) -> OsmNodeCacheResult<()> {
        if let Some(last) = self.last_id.filter(|&last| id <= last) {
            return Err(OsmNodeCacheError::NotAscending { id, last });
        }
        self.last_id = Some(id);
        let (num, bit) = block_of(id);
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

    /// Writes the last page and flushes the cache.
    pub fn finish(mut self) -> OsmNodeCacheResult<PagedFileCache> {
        if self.block.mask != 0 {
            self.add_block()?;
        }
        self.write_page()?;
        self.cache.flush()?;
        Ok(self.cache)
    }

    /// Adds `self.block` to the page being filled, or to a new page if it does not fit.
    fn add_block(&mut self) -> OsmNodeCacheResult<()> {
        if !self.table.is_empty() {
            if let Ok(rel) = u16::try_from(self.block.num - self.first) {
                self.tmp.clear();
                encode_block(&mut self.tmp, &self.block, self.base);
                let size = PAGE_HEADER + ENTRY * (self.table.len() + 1) + self.blocks.len();
                if size + self.tmp.len() <= self.cache.fill_limit {
                    self.blocks.extend_from_slice(&self.tmp);
                    self.table.push((rel, self.blocks.len()));
                    return Ok(());
                }
            }
            self.write_page()?;
        }
        (self.first, self.base) = (self.block.num, self.block.locs[0]);
        self.blocks.clear();
        encode_block(&mut self.blocks, &self.block, self.base);
        self.table.push((0, self.blocks.len()));
        Ok(())
    }

    fn write_page(&mut self) -> OsmNodeCacheResult<()> {
        if self.table.is_empty() {
            return Ok(());
        }
        let start = PAGE_HEADER + ENTRY * self.table.len();
        let buf = &mut self.cache.buf;
        buf.clear();
        buf.resize(start, 0);
        for (i, &(rel, end)) in self.table.iter().enumerate() {
            set_entry(buf, i, rel, start + end);
        }
        buf.extend_from_slice(&self.blocks);
        set_page_header(buf, self.first, self.table.len(), self.base);
        let p = self.cache.alloc_page()?;
        self.cache.write_page(p);
        self.cache.dir.push(self.first, p);
        self.table.clear();
        Ok(())
    }
}

impl NodeStore for PagedFileCache {
    type Opts = PagedFileCacheOpts;
    type Reader<'a> = PagedReader<'a>;
    type Builder = PagedBuilder;

    fn builder(path: &Path, opts: &PagedFileCacheOpts) -> OsmNodeCacheResult<PagedBuilder> {
        PagedFileCache::builder(path, opts)
    }

    fn open(path: &Path, opts: &PagedFileCacheOpts) -> OsmNodeCacheResult<Self> {
        PagedFileCache::open(path, opts)
    }

    #[inline]
    fn reader(&self) -> PagedReader<'_> {
        PagedFileCache::reader(self)
    }

    #[inline]
    fn get(&self, id: u64) -> Option<Location> {
        PagedFileCache::get(self, id)
    }

    fn set(&mut self, id: u64, loc: Location) -> OsmNodeCacheResult<()> {
        PagedFileCache::set(self, id, loc)
    }

    fn remove(&mut self, id: u64) -> OsmNodeCacheResult<()> {
        PagedFileCache::remove(self, id)
    }

    fn stamp(&self) -> u64 {
        PagedFileCache::stamp(self)
    }

    /// Flushes the changes with `stamp`; see [`PagedFileCache::flush`].
    fn commit(&mut self, stamp: u64) -> OsmNodeCacheResult<()> {
        self.set_stamp(stamp)?;
        self.flush()
    }
}

impl NodeReader for PagedReader<'_> {
    #[inline]
    fn get(&mut self, id: u64) -> Option<Location> {
        PagedReader::get(self, id)
    }
}

impl NodeBuilder for PagedBuilder {
    type Store = PagedFileCache;

    fn push(&mut self, id: u64, loc: Location) -> OsmNodeCacheResult<()> {
        PagedBuilder::push(self, id, loc)
    }

    fn finish(mut self, stamp: u64) -> OsmNodeCacheResult<PagedFileCache> {
        self.cache.stamp = stamp;
        PagedBuilder::finish(self)
    }
}

/// The pages allocated, stamp, and clean flag from a header page written by `write_header`
fn read_header(h: &[u8]) -> Result<(u64, u64, bool), &'static str> {
    if h.len() < PAGE_SIZE || &h[..8] != MAGIC {
        return Err("not a paged node cache");
    }
    if u32_at(h, 8) != VERSION
        || u32_at(h, 12) as usize != PAGE_SIZE
        || u32_at(h, 16) as usize != BLOCK_LEN
    {
        return Err("unsupported version, page size or block size");
    }
    let allocated = u64_at(h, 24);
    if allocated > (h.len() / PAGE_SIZE) as u64 {
        return Err("file is shorter than its pages");
    }
    Ok((allocated, u64_at(h, 32), h[40] == 1))
}

pub(crate) fn dir_path(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".dir");
    PathBuf::from(s)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use rand::rngs::StdRng;
    use rand::{RngExt as _, SeedableRng as _};

    use super::*;
    use crate::traits::tests::TestFile;

    /// Nodes like a mapper's: mostly short steps, now and then a jump elsewhere. With probability
    /// `next` an ID follows the previous one, else it skips ahead.
    fn nodes(rng: &mut StdRng, count: usize, start: u64, next: f64) -> BTreeMap<u64, Location> {
        let mut out = BTreeMap::new();
        let (mut id, mut loc) = (start, Location::new(0, 0));
        for _ in 0..count {
            id += if rng.random_bool(next) {
                1
            } else {
                rng.random_range(2..200)
            };
            loc = if rng.random_bool(0.02) {
                random_location(rng)
            } else {
                Location::new(
                    (loc.lat + rng.random_range(-3000..3000)).clamp(-900_000_000, 900_000_000),
                    (loc.lon + rng.random_range(-3000..3000)).clamp(-1_800_000_000, 1_800_000_000),
                )
            };
            out.insert(id, loc);
        }
        out
    }

    fn random_location(rng: &mut StdRng) -> Location {
        Location::new(
            rng.random_range(-900_000_000..=900_000_000),
            rng.random_range(-1_800_000_000..=1_800_000_000),
        )
    }

    fn build(
        path: &Path,
        expected: &BTreeMap<u64, Location>,
        opts: &PagedFileCacheOpts,
    ) -> PagedFileCache {
        let mut b = PagedFileCache::builder(path, opts).unwrap();
        for (&id, &loc) in expected {
            b.push(id, loc).unwrap();
        }
        b.finish().unwrap()
    }

    fn check(cache: &PagedFileCache, expected: &BTreeMap<u64, Location>) {
        let (lo, hi) = (
            *expected.keys().next().unwrap(),
            *expected.keys().last().unwrap(),
        );
        // every ID in the range, or around each node when they are far apart
        let ids: Vec<u64> = if hi - lo < 1_000_000 {
            (lo.saturating_sub(100)..hi + 100).collect()
        } else {
            expected
                .keys()
                .flat_map(|&k| k.saturating_sub(1)..=k + 1)
                .collect()
        };
        for &id in &ids {
            assert_eq!(cache.get(id), expected.get(&id).copied(), "node {id}");
        }
        let mut reader = cache.reader();
        for &id in ids.iter().step_by(7) {
            assert_eq!(
                reader.get(id),
                expected.get(&id).copied(),
                "node {id} by the reader"
            );
        }
        let mut all = Vec::new();
        cache.for_each(|id, loc| all.push((id, loc)));
        assert!(all.into_iter().eq(expected.iter().map(|(&k, &v)| (k, v))));
        assert_eq!(cache.stats().nodes, expected.len() as u64);

        // the directory is sorted, its index agrees with a search, and each page's blocks are in
        // its range
        let dir = &cache.dir;
        assert!(dir.starts.is_sorted());
        for &s in &dir.starts {
            for num in [s.saturating_sub(1), s, s + 1] {
                let found = dir.starts.partition_point(|&x| x <= num).checked_sub(1);
                assert_eq!(dir.find(num), found, "block {num}");
            }
        }
        for (i, &p) in dir.pages.iter().enumerate() {
            let (first, blocks) = decode_page(cache.page(p));
            assert_eq!(first, dir.starts[i]);
            let next = dir.starts.get(i + 1).copied().unwrap_or(u64::MAX);
            assert!(blocks.iter().all(|b| b.num >= first && b.num < next));
        }
    }

    #[test]
    fn round_trip() {
        let f = TestFile::new("round_trip");
        let mut rng = StdRng::seed_from_u64(1);
        let expected = nodes(&mut rng, 50_000, 1000, 0.995);
        let cache = build(&f.0, &expected, &PagedFileCacheOpts::default());
        check(&cache, &expected);
        let stats = cache.stats();
        assert!(
            stats.codecs.iter().all(|&c| c > 0),
            "all codecs used: {stats:?}"
        );
        assert!(
            stats.masks.iter().all(|&c| c > 0),
            "all mask kinds used: {stats:?}"
        );
        drop(cache);
        let cache = PagedFileCache::open(&f.0, &PagedFileCacheOpts::default()).unwrap();
        check(&cache, &expected);
    }

    #[test]
    fn empty() {
        let f = TestFile::new("empty");
        let opts = PagedFileCacheOpts::default();
        let cache = PagedFileCache::builder(&f.0, &opts)
            .unwrap()
            .finish()
            .unwrap();
        assert_eq!(cache.get(0), None);
        drop(cache);
        let cache = PagedFileCache::open(&f.0, &opts).unwrap();
        assert_eq!(cache.reader().get(0), None);
        assert_eq!(cache.stats().nodes, 0);
    }

    #[test]
    fn locations() {
        let loc = Location::from_degrees(47.123_456_74, -122.987_654_37);
        assert_eq!(loc, Location::from((471_234_567, -1_229_876_544)));
        assert!((loc.lat_degrees() - 47.123_456_7).abs() < 1e-9);
        assert!((loc.lon_degrees() + 122.987_654_4).abs() < 1e-9);
    }

    #[test]
    fn extreme_locations() {
        let f = TestFile::new("extreme");
        let corners = [
            (-900_000_000, -1_800_000_000),
            (900_000_000, 1_800_000_000),
            (0, 0),
            (i32::MIN, i32::MAX),
        ];
        let expected: BTreeMap<u64, Location> = (0..640_u64)
            .zip(corners.iter().cycle())
            .map(|(i, &c)| (i * 3 + (1 << 40), Location::from(c)))
            .collect();
        let cache = build(&f.0, &expected, &PagedFileCacheOpts::default());
        check(&cache, &expected);
    }

    #[test]
    fn changes_match_a_map() {
        let f = TestFile::new("changes");
        let mut rng = StdRng::seed_from_u64(2);
        let mut expected = nodes(&mut rng, 20_000, 0, 0.8);
        let opts = PagedFileCacheOpts::default().fill(0.5);
        let mut cache = build(&f.0, &expected, &opts);
        let max = *expected.keys().last().unwrap();
        for i in 0..60_000 {
            let id = rng.random_range(0..max + 5000);
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
                cache.set(id, loc).unwrap();
                expected.insert(id, loc);
            } else {
                cache.remove(id).unwrap();
                expected.remove(&id);
            }
            if i % 20_000 == 0 {
                check(&cache, &expected);
            }
        }
        check(&cache, &expected);
        let pages = cache.stats().pages;
        drop(cache);
        let cache = PagedFileCache::open(&f.0, &opts).unwrap();
        assert_eq!(cache.stats().pages, pages);
        check(&cache, &expected);
    }

    #[test]
    fn appending_new_ids_fills_pages_as_a_bulk_load() {
        // sparse blocks, and full blocks that overflow a nearly full page as they grow
        for (fill, next) in [(0.8, 0.8), (0.97, 0.995)] {
            let f = TestFile::new(&format!("append_{fill}"));
            let mut rng = StdRng::seed_from_u64(3);
            let expected = nodes(&mut rng, 30_000, 5, next);
            let opts = PagedFileCacheOpts::default().fill(fill);
            let mut cache = PagedFileCache::create(&f.0, &opts).unwrap();
            for (&id, &loc) in &expected {
                cache.set(id, loc).unwrap();
            }
            check(&cache, &expected);
            let built = build(&TestFile::new("append_built").0, &expected, &opts);
            let (by_set, by_build) = (cache.stats().pages, built.stats().pages);
            assert!(
                by_set <= by_build + 1,
                "fill {fill}: {by_set} pages by set, {by_build} by bulk load"
            );
        }
    }

    #[test]
    fn set_below_first_page_and_into_empty() {
        let f = TestFile::new("below");
        let mut cache = PagedFileCache::create(&f.0, &PagedFileCacheOpts::default()).unwrap();
        assert_eq!(cache.get(5), None);
        cache.remove(5).unwrap();
        let mut expected = BTreeMap::new();
        for id in [100_000, 5000, 64, 63, 0, 1_000_000] {
            let loc = Location::new(i32::try_from(id).unwrap(), -1);
            cache.set(id, loc).unwrap();
            expected.insert(id, loc);
            check(&cache, &expected);
        }
        for id in [0, 63, 64] {
            cache.remove(id).unwrap();
            expected.remove(&id);
        }
        check(&cache, &expected);
    }

    #[test]
    fn ids_far_apart() {
        let f = TestFile::new("far");
        let expected: BTreeMap<u64, Location> = (0..500_u64)
            .map(|i| {
                (
                    i * 9_000_000 + i % 7,
                    Location::new(i32::try_from(i).unwrap(), 7),
                )
            })
            .collect();
        check(
            &build(&f.0, &expected, &PagedFileCacheOpts::default()),
            &expected,
        );
        // and set one by one, from the top down and then between them
        let f = TestFile::new("far_set");
        let mut cache = PagedFileCache::create(&f.0, &PagedFileCacheOpts::default()).unwrap();
        let mut by_set = BTreeMap::new();
        let shifted = expected.iter().rev().chain(expected.iter().take(50));
        let shifted = shifted.map(|(&id, &loc)| (id + 4_500_000, loc));
        for (id, loc) in shifted.chain(expected.iter().map(|(&id, &loc)| (id, loc))) {
            cache.set(id, loc).unwrap();
            by_set.insert(id, loc);
        }
        check(&cache, &by_set);
    }

    #[test]
    fn stamp_is_saved() {
        let f = TestFile::new("stamp");
        let opts = PagedFileCacheOpts::default();
        let mut cache = PagedFileCache::create(&f.0, &opts).unwrap();
        cache.set_stamp(42).unwrap();
        drop(cache);
        let mut cache = PagedFileCache::open(&f.0, &opts).unwrap();
        assert_eq!(cache.stamp(), 42);
        // a change that leaves the directory as it was
        cache.set_stamp(43).unwrap();
        drop(cache);
        assert_eq!(PagedFileCache::open(&f.0, &opts).unwrap().stamp(), 43);
    }

    #[test]
    fn rebuilds_a_missing_or_stale_directory() {
        let f = TestFile::new("no_dir");
        let mut rng = StdRng::seed_from_u64(6);
        let expected = nodes(&mut rng, 5000, 0, 0.8);
        let opts = PagedFileCacheOpts::default();
        drop(build(&f.0, &expected, &opts));
        fs::remove_file(dir_path(&f.0)).unwrap();
        check(&PagedFileCache::open(&f.0, &opts).unwrap(), &expected);
        fs::write(dir_path(&f.0), [0; 24]).unwrap();
        check(&PagedFileCache::open(&f.0, &opts).unwrap(), &expected);
    }

    #[test]
    fn rebuilds_directory_after_crash() {
        let f = TestFile::new("crash");
        let mut rng = StdRng::seed_from_u64(4);
        let mut expected = nodes(&mut rng, 20_000, 0, 0.8);
        let opts = PagedFileCacheOpts::default().fill(0.6);
        let mut cache = build(&f.0, &expected, &opts);
        for _ in 0..5000 {
            let (id, loc) = (rng.random_range(0..30_000), random_location(&mut rng));
            cache.set(id, loc).unwrap();
            expected.insert(id, loc);
        }
        // a crash: the pages are written, the directory and the header not
        cache.mmap.flush().unwrap();
        std::mem::forget(cache);
        check(&PagedFileCache::open(&f.0, &opts).unwrap(), &expected);
    }

    #[test]
    fn rebuild_drops_blocks_left_in_both_pages_of_a_split() {
        let f = TestFile::new("split_crash");
        let mut rng = StdRng::seed_from_u64(5);
        let expected = nodes(&mut rng, 3000, 0, 0.8);
        let mut cache = build(&f.0, &expected, &PagedFileCacheOpts::default());
        // copy the upper blocks of the first page to a new page, as a split does before it rewrites
        // the lower page
        let (_, blocks) = decode_page(cache.page(cache.dir.pages[0]));
        let k = blocks.len() / 2;
        let p = cache.alloc_page().unwrap();
        encode_page(blocks[k].num, &blocks[k..], &mut cache.buf);
        cache.write_page(p);
        cache.write_header(false).unwrap();
        cache.mmap.flush().unwrap();
        std::mem::forget(cache);
        check(
            &PagedFileCache::open(&f.0, &PagedFileCacheOpts::default()).unwrap(),
            &expected,
        );
    }

    #[test]
    fn open_rejects_other_files() {
        let f = TestFile::new("invalid");
        let opts = PagedFileCacheOpts::default();
        let res = PagedFileCache::open(&f.0, &opts);
        assert!(matches!(res, Err(OsmNodeCacheError::InvalidCacheFile(..))));
        assert!(!f.0.exists(), "a missing file is not created");
        drop(PagedFileCache::create(&f.0, &opts).unwrap());
        let good = fs::read(&f.0).unwrap();
        // the magic, the version, and a page count beyond the file
        for (at, byte) in [(0, b'X'), (8, 9), (31, 1)] {
            let mut bad = good.clone();
            bad[at] = byte;
            fs::write(&f.0, &bad).unwrap();
            let res = PagedFileCache::open(&f.0, &opts);
            assert!(
                matches!(res, Err(OsmNodeCacheError::InvalidFormat(..))),
                "byte {at}"
            );
            assert_eq!(
                fs::read(&f.0).unwrap(),
                bad,
                "a rejected file is left as it was"
            );
        }
        fs::write(&f.0, b"short").unwrap();
        let res = PagedFileCache::open(&f.0, &opts);
        assert!(matches!(res, Err(OsmNodeCacheError::InvalidFormat(..))));
    }

    #[test]
    fn rejects_unsorted_ids() {
        let f = TestFile::new("unsorted");
        let mut b = PagedFileCache::builder(&f.0, &PagedFileCacheOpts::default()).unwrap();
        b.push(10, Location::default()).unwrap();
        assert!(matches!(
            b.push(10, Location::default()),
            Err(OsmNodeCacheError::NotAscending { id: 10, last: 10 })
        ));
    }
}
