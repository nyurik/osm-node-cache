//! Node stores in key-value databases, which keep their commits through a crash or a power loss.
//! The `redb` feature adds [`crate::RedbBackend`]; other databases can implement [`KvBackend`].
//!
//! Each value holds a group of `2^k` blocks (`64 * 2^k` node IDs), encoded as one page of
//! [`crate::codec`] of any size, under the group's number. A reserved key holds the format and the
//! stamp, written in the same commit as the groups.

use std::collections::btree_map::Entry;
use std::collections::BTreeMap;
use std::path::Path;

use crate::codec::{
    block_of, decode_page, encode_page, find_in_page, rank, u32_at, u64_at, Block, Location, Slots,
    BLOCK_LEN, BLOCK_SHIFT,
};
use crate::{NodeBuilder, NodeReader, NodeStore, OsmNodeCacheError, OsmNodeCacheResult};

#[cfg(feature = "redb")]
pub mod redb;

/// The key of the format and stamp; group numbers stay below it.
const META: u64 = u64::MAX;
const MAGIC: &[u8; 8] = b"ONCKV001";
const VERSION: u32 = 1;
/// Groups a bulk load writes per commit
const BUILD_BATCH: usize = 1 << 16;

/// A key-value database holding the values of a [`KvNodeStore`]: up to 64 KiB under `u64` keys.
pub trait KvBackend: Sync + Sized {
    type Opts: Default;
    type Reader<'a>: KvReader
    where
        Self: 'a;

    /// An empty database at `path`, replacing the values of any there.
    fn create(path: &Path, opts: &Self::Opts) -> OsmNodeCacheResult<Self>;

    fn open(path: &Path, opts: &Self::Opts) -> OsmNodeCacheResult<Self>;

    /// A reader for one thread.
    fn reader(&self) -> Self::Reader<'_>;

    /// Writes the values, removes the keys without one, and commits them all at once. A `durable`
    /// commit is on disk when this returns; others are kept by the next durable one.
    fn commit(&mut self, values: &[(u64, Option<&[u8]>)], durable: bool) -> OsmNodeCacheResult<()>;

    /// The bytes the database takes on disk.
    fn disk_bytes(&self) -> OsmNodeCacheResult<u64>;
}

pub trait KvReader {
    /// Copies the value of `key` into `out`, and returns false if there is none.
    ///
    /// # Panics
    /// If the database cannot be read.
    fn get(&mut self, key: u64, out: &mut Vec<u8>) -> bool;
}

/// Options for [`KvNodeStore`].
#[derive(Clone, Debug)]
pub struct KvOpts<O> {
    group_shift: u32,
    backend: O,
}

impl<O: Default> Default for KvOpts<O> {
    fn default() -> Self {
        // on Europe, 4 blocks per value make values of about 1 KiB
        Self {
            group_shift: 2,
            backend: O::default(),
        }
    }
}

impl<O> KvOpts<O> {
    /// Stores `2^shift` blocks of 64 IDs per value, when building a store.
    ///
    /// # Panics
    /// If `shift` is above 6, as values could then exceed 64 KiB.
    #[must_use]
    pub fn group_shift(mut self, shift: u32) -> Self {
        assert!(shift <= 6, "group shift must be at most 6, not {shift}");
        self.group_shift = shift;
        self
    }

    #[must_use]
    pub fn backend(mut self, backend: O) -> Self {
        self.backend = backend;
        self
    }
}

/// Node locations in a key-value database, which keeps every commit through a crash or a power
/// loss. Changes are held in memory until [`NodeStore::commit`]; dropping the store loses them.
pub struct KvNodeStore<B> {
    kv: B,
    /// IDs per value: `2^shift`
    shift: u32,
    stamp: u64,
    /// the groups changed since the last commit, decoded
    pending: BTreeMap<u64, Vec<Block>>,
}

impl<B: KvBackend> KvNodeStore<B> {
    /// The bytes the database takes on disk.
    pub fn disk_bytes(&self) -> OsmNodeCacheResult<u64> {
        self.kv.disk_bytes()
    }

    fn new(kv: B, shift: u32, stamp: u64) -> Self {
        Self {
            kv,
            shift,
            stamp,
            pending: BTreeMap::new(),
        }
    }

    fn meta(&self, stamp: u64) -> Vec<u8> {
        let mut meta = MAGIC.to_vec();
        meta.extend_from_slice(&VERSION.to_le_bytes());
        meta.extend_from_slice(&self.shift.to_le_bytes());
        meta.extend_from_slice(&stamp.to_le_bytes());
        meta
    }

    fn modify(&mut self, id: u64, loc: Option<Location>) {
        let (num, bit) = block_of(id);
        let blocks = match self.pending.entry(id >> self.shift) {
            Entry::Occupied(e) => e.into_mut(),
            Entry::Vacant(e) => {
                let mut value = Vec::new();
                let blocks = if self.kv.reader().get(*e.key(), &mut value) {
                    decode_page(&value).1
                } else {
                    Vec::new()
                };
                e.insert(blocks)
            }
        };
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
    }
}

impl<B: KvBackend> NodeStore for KvNodeStore<B> {
    type Opts = KvOpts<B::Opts>;
    type Reader<'a>
        = KvNodeReader<'a, B>
    where
        B: 'a;
    type Builder = KvBuilder<B>;

    fn builder(path: &Path, opts: &Self::Opts) -> OsmNodeCacheResult<KvBuilder<B>> {
        let kv = B::create(path, &opts.backend)?;
        Ok(KvBuilder {
            store: Self::new(kv, BLOCK_SHIFT + opts.group_shift, 0),
            last_id: None,
            blocks: Vec::new(),
            used: 0,
            values: Vec::with_capacity(BUILD_BATCH),
        })
    }

    fn open(path: &Path, opts: &Self::Opts) -> OsmNodeCacheResult<Self> {
        let kv = B::open(path, &opts.backend)?;
        let mut meta = Vec::new();
        let invalid = |e: &str| OsmNodeCacheError::InvalidFormat(path.to_path_buf(), e.to_string());
        if !kv.reader().get(META, &mut meta) || meta.len() != 24 || &meta[..8] != MAGIC {
            return Err(invalid("not a node store"));
        }
        if u32_at(&meta, 8) != VERSION {
            return Err(invalid("unsupported version"));
        }
        let shift = u32_at(&meta, 12);
        if !(BLOCK_SHIFT..=BLOCK_SHIFT + 6).contains(&shift) {
            return Err(invalid("invalid group size"));
        }
        Ok(Self::new(kv, shift, u64_at(&meta, 16)))
    }

    fn reader(&self) -> KvNodeReader<'_, B> {
        KvNodeReader {
            store: self,
            kv: self.kv.reader(),
            key: META,
            value: Vec::new(),
            found: false,
            slots: Slots::default(),
        }
    }

    fn set(&mut self, id: u64, loc: Location) -> OsmNodeCacheResult<()> {
        self.modify(id, Some(loc));
        Ok(())
    }

    fn remove(&mut self, id: u64) -> OsmNodeCacheResult<()> {
        self.modify(id, None);
        Ok(())
    }

    fn stamp(&self) -> u64 {
        self.stamp
    }

    fn commit(&mut self, stamp: u64) -> OsmNodeCacheResult<()> {
        let mut encoded = Vec::with_capacity(self.pending.len());
        for (&key, blocks) in &self.pending {
            let mut value = Vec::new();
            if let Some(first) = blocks.first() {
                encode_page(first.num, blocks, &mut value);
            }
            encoded.push((key, value));
        }
        let meta = self.meta(stamp);
        let mut values: Vec<_> = encoded
            .iter()
            .map(|(key, v)| (*key, (!v.is_empty()).then_some(v.as_slice())))
            .collect();
        values.push((META, Some(&meta)));
        self.kv.commit(&values, true)?;
        self.pending.clear();
        self.stamp = stamp;
        Ok(())
    }
}

/// Reads locations of a [`KvNodeStore`] for one thread, keeping the last value it read.
pub struct KvNodeReader<'a, B: KvBackend + 'a> {
    store: &'a KvNodeStore<B>,
    kv: B::Reader<'a>,
    /// the key of `value`, which is only valid if `found`
    key: u64,
    value: Vec<u8>,
    found: bool,
    slots: Slots,
}

impl<B: KvBackend> NodeReader for KvNodeReader<'_, B> {
    #[inline]
    fn get(&mut self, id: u64) -> Option<Location> {
        let Self {
            store,
            kv,
            key,
            value,
            found,
            slots,
        } = self;
        slots.get(id, |num, locs| {
            let group = id >> store.shift;
            if let Some(blocks) = store.pending.get(&group) {
                let Ok(i) = blocks.binary_search_by_key(&num, |b| b.num) else {
                    return 0;
                };
                locs[..blocks[i].locs.len()].copy_from_slice(&blocks[i].locs);
                return blocks[i].mask;
            }
            if *key != group {
                *key = group;
                *found = kv.get(group, value);
            }
            if !*found {
                return 0;
            }
            find_in_page(value, num).map_or(0, |head| {
                head.decode_into(value, locs);
                head.mask
            })
        })
    }
}

/// Bulk loads a new [`KvNodeStore`] from locations in ascending ID order.
pub struct KvBuilder<B> {
    store: KvNodeStore<B>,
    last_id: Option<u64>,
    /// the group being filled, in its first `used` blocks; the rest are kept for their memory
    blocks: Vec<Block>,
    used: usize,
    /// encoded groups not yet written
    values: Vec<(u64, Vec<u8>)>,
}

impl<B: KvBackend> KvBuilder<B> {
    /// Encodes the group being filled, and writes the encoded groups if there are enough.
    fn add_group(&mut self, last: bool) -> OsmNodeCacheResult<()> {
        if self.used > 0 {
            let blocks = &self.blocks[..self.used];
            let mut value = Vec::new();
            encode_page(blocks[0].num, blocks, &mut value);
            self.values
                .push((blocks[0].num >> (self.store.shift - BLOCK_SHIFT), value));
            self.used = 0;
        }
        if last || self.values.len() == BUILD_BATCH {
            let values: Vec<_> = self
                .values
                .iter()
                .map(|(key, v)| (*key, Some(v.as_slice())))
                .collect();
            self.store.kv.commit(&values, false)?;
            self.values.clear();
        }
        Ok(())
    }
}

impl<B: KvBackend> NodeBuilder for KvBuilder<B> {
    type Store = KvNodeStore<B>;

    fn push(&mut self, id: u64, loc: Location) -> OsmNodeCacheResult<()> {
        if let Some(last) = self.last_id.filter(|&last| id <= last) {
            return Err(OsmNodeCacheError::NotAscending { id, last });
        }
        let (num, bit) = block_of(id);
        if let Some(last) = self.last_id {
            if last >> self.store.shift != id >> self.store.shift {
                self.add_group(false)?;
            }
        }
        self.last_id = Some(id);
        if self.used > 0 && self.blocks[self.used - 1].num == num {
            let b = &mut self.blocks[self.used - 1];
            b.mask |= bit;
            b.locs.push(loc);
        } else {
            if self.used == self.blocks.len() {
                self.blocks.push(Block::default());
            }
            let b = &mut self.blocks[self.used];
            (b.num, b.mask) = (num, bit);
            b.locs.clear();
            b.locs.reserve(BLOCK_LEN);
            b.locs.push(loc);
            self.used += 1;
        }
        Ok(())
    }

    fn finish(mut self, stamp: u64) -> OsmNodeCacheResult<KvNodeStore<B>> {
        self.add_group(true)?;
        let meta = self.store.meta(stamp);
        self.store.kv.commit(&[(META, Some(&meta))], true)?;
        self.store.stamp = stamp;
        Ok(self.store)
    }
}

/// An error of a backend database
#[cfg(feature = "redb")]
pub(crate) fn store_err(
    e: impl Into<Box<dyn std::error::Error + Send + Sync>>,
) -> OsmNodeCacheError {
    OsmNodeCacheError::Store(e.into())
}
