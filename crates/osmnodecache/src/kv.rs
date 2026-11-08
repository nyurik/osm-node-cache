//! [`KvStore`]: node locations in a key-value database, which keeps every commit through a crash or
//! a power loss. The `redb` feature adds [`crate::RedbBackend`]; other databases can implement
//! [`KvBackend`].
//!
//! Each value holds a group of `2^k` blocks (`64 * 2^k` node IDs), encoded as one page of
//! [`crate::codec`] of any size, under the group's number. A reserved key holds the format, the
//! stamp and the load state, written in the same transaction as the groups.

use std::collections::BTreeMap;
use std::marker::PhantomData;
use std::path::Path;

use crate::codec::{
    block_of, decode_page, encode_page, find_in_page, rank, u32_at, u64_at, Block, Location, Slots,
    BLOCK_LEN, BLOCK_SHIFT,
};
use crate::store::{check, check_order, check_resumed};
use crate::{
    BulkLoad, Error, FileStore, FixReport, LoadState, Metadata, Mode, NodeRead, NodeReader,
    NodeStore, ReadOnly, ReadWrite, Result,
};

#[cfg(feature = "redb")]
pub mod redb;

/// The key of the format, stamp and load state; group numbers stay below it.
const META: u64 = u64::MAX;
const MAGIC: &[u8; 8] = b"ONCKV002";
const VERSION: u32 = 2;
/// Where the reserved value's metadata starts
const METADATA_AT: usize = 33;
/// Groups a bulk load writes per transaction
const BATCH: usize = if cfg!(test) { 16 } else { 1 << 16 };
/// Groups a bulk load writes between checkpoints, which are durable
const CHECKPOINT: usize = if cfg!(test) { 64 } else { 1 << 20 };
/// A bulk load encodes batches of at least this many nodes on all threads, this many groups to a
/// task
#[cfg(any(feature = "paged", feature = "redb"))]
const PARALLEL_NODES: usize = if cfg!(test) { 100 } else { 8192 };
#[cfg(any(feature = "paged", feature = "redb"))]
const PARALLEL_GROUPS: usize = 64;

/// A key-value database holding the values of a [`KvStore`]: up to 64 KiB under `u64` keys.
pub trait KvBackend: Sync + Sized {
    type Opts: Default;

    /// Opens the database at `path` to read and write, creating it if it is missing or empty, and
    /// says whether it was created. No one else may open it while it is open.
    fn open(path: &Path, opts: &Self::Opts) -> Result<(Self, bool)>;

    /// Opens an existing database to read only, sharing it with other readers.
    fn open_read_only(path: &Path, opts: &Self::Opts) -> Result<Self>;

    /// Copies the committed value of `key` into `out`, and returns false if there is none.
    fn get(&self, key: u64, out: &mut Vec<u8>) -> Result<bool>;

    /// Calls `f` with every committed key below `below` and its value, in key order.
    fn for_each(&self, below: u64, f: &mut dyn FnMut(u64, &[u8])) -> Result<()>;

    /// Whether there are committed keys below `below`.
    fn has_keys_below(&self, below: u64) -> Result<bool>;

    /// Writes the values, removes the keys without one, and commits them all at once. A `durable`
    /// commit is on disk when this returns; others are kept by the next durable one. Later reads
    /// see the commit.
    fn commit(&mut self, values: &[(u64, Option<&[u8]>)], durable: bool) -> Result<()>;

    /// The bytes the database takes on disk.
    fn disk_bytes(&self) -> Result<u64>;

    /// Reads the database's files into memory; see [`NodeRead::preload`].
    fn preload(&self) -> Result<()> {
        Ok(())
    }
}

/// Options for [`KvStore`].
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
    /// Stores `2^shift` blocks of 64 IDs per value, when creating a store.
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

/// What the reserved key holds, with the metadata
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Meta {
    /// IDs per value: `2^shift`
    shift: u32,
    stamp: u64,
    state: LoadState,
}

impl Meta {
    fn encode(&self, metadata: &Metadata) -> Vec<u8> {
        let mut m = vec![0_u8; METADATA_AT];
        m[..8].copy_from_slice(MAGIC);
        m[8..12].copy_from_slice(&VERSION.to_le_bytes());
        m[12..16].copy_from_slice(&self.shift.to_le_bytes());
        m[16..24].copy_from_slice(&self.stamp.to_le_bytes());
        if let LoadState::Loading { last_id } = self.state {
            m[24] = 1 + u8::from(last_id.is_some());
            m[25..33].copy_from_slice(&last_id.unwrap_or(0).to_le_bytes());
        }
        metadata.encode(&mut m);
        m
    }

    fn decode(m: &[u8], path: &Path) -> Result<(Self, Metadata)> {
        if m.len() < 12 || !m.starts_with(&MAGIC[..6]) {
            return Err(Error::NotAStore(path.to_path_buf()));
        }
        let version = u32_at(m, 8);
        if &m[..8] != MAGIC || version != VERSION {
            return Err(Error::UnsupportedVersion {
                path: path.to_path_buf(),
                found: version,
                expected: VERSION,
            });
        }
        let corrupt = || Error::Corrupt {
            path: path.to_path_buf(),
            reason: "invalid store metadata".to_string(),
        };
        // the fixed fields, then the named values
        if m.len() < METADATA_AT {
            return Err(corrupt());
        }
        let shift = u32_at(m, 12);
        let (Some(metadata), true) = (
            Metadata::decode(&m[METADATA_AT..]),
            (BLOCK_SHIFT..=BLOCK_SHIFT + 6).contains(&shift),
        ) else {
            return Err(corrupt());
        };
        let meta = Self {
            shift,
            stamp: u64_at(m, 16),
            state: match m[24] {
                0 => LoadState::Complete,
                1 => LoadState::Loading { last_id: None },
                _ => LoadState::Loading {
                    last_id: Some(u64_at(m, 25)),
                },
            },
        };
        Ok((meta, metadata))
    }
}

/// Node locations in a key-value database; the module documentation has the format.
///
/// Changes are held in memory until [`NodeStore::commit`]; dropping the store loses them.
pub struct KvStore<B, M: Mode = ReadWrite> {
    kv: B,
    meta: Meta,
    /// with the changes not yet committed
    metadata: Metadata,
    /// the groups changed since the last commit, decoded
    pending: BTreeMap<u64, Vec<Block>>,
    mode: PhantomData<M>,
}

/// Reads locations of a [`KvStore`] for one thread, keeping the last value it read.
pub struct KvReader<'a, B, M: Mode = ReadWrite> {
    store: &'a KvStore<B, M>,
    /// the key of `value`, which is only valid if `found`
    key: u64,
    value: Vec<u8>,
    found: bool,
    slots: Slots,
}

impl<B: KvBackend, M: Mode> KvStore<B, M> {
    /// The bytes the database takes on disk.
    pub fn disk_bytes(&self) -> Result<u64> {
        self.kv.disk_bytes()
    }

    fn new(kv: B, meta: Meta, metadata: Metadata) -> Self {
        Self {
            kv,
            meta,
            metadata,
            pending: BTreeMap::new(),
            mode: PhantomData,
        }
    }

    fn read_meta(kv: &B, path: &Path) -> Result<(Meta, Metadata)> {
        let mut m = Vec::new();
        if !kv.get(META, &mut m)? {
            return Err(Error::NotAStore(path.to_path_buf()));
        }
        Meta::decode(&m, path)
    }

    /// The group holding node `id`
    fn group(&self, id: u64) -> u64 {
        id >> self.meta.shift
    }

    /// The decoded blocks of a committed group.
    fn read_group(&self, group: u64) -> Result<Vec<Block>> {
        let mut value = Vec::new();
        Ok(if self.kv.get(group, &mut value)? {
            decode_page(&value).1
        } else {
            Vec::new()
        })
    }
}

impl<B: KvBackend, M: Mode> NodeRead for KvStore<B, M> {
    type Reader<'a>
        = KvReader<'a, B, M>
    where
        Self: 'a;

    fn reader(&self) -> KvReader<'_, B, M> {
        KvReader {
            store: self,
            key: META,
            value: Vec::new(),
            found: false,
            slots: Slots::default(),
        }
    }

    /// Reads the node's group, and decodes the node's block only up to the node.
    fn get(&self, id: u64) -> Option<Location> {
        let (num, bit) = block_of(id);
        let group = self.group(id);
        if let Some(blocks) = self.pending.get(&group) {
            let b = &blocks[blocks.binary_search_by_key(&num, |b| b.num).ok()?];
            return (b.mask & bit != 0).then(|| b.locs[rank(b.mask, bit)]);
        }
        let mut value = Vec::new();
        if !self
            .kv
            .get(group, &mut value)
            .expect("cannot read the database")
        {
            return None;
        }
        let head = find_in_page(&value, num)?;
        (head.mask & bit != 0).then(|| head.location(&value, rank(head.mask, bit)))
    }

    fn stamp(&self) -> u64 {
        self.meta.stamp
    }

    fn load_state(&self) -> LoadState {
        self.meta.state
    }

    fn metadata(&self) -> &Metadata {
        &self.metadata
    }

    fn preload(&self) -> Result<()> {
        self.kv.preload()
    }

    fn for_each(&self, mut f: impl FnMut(u64, Location)) -> Result<()> {
        let mut pending = self.pending.iter().peekable();
        let mut emit = |blocks: &[Block]| {
            blocks
                .iter()
                .flat_map(Block::nodes)
                .for_each(|(id, loc)| f(id, loc));
        };
        self.kv.for_each(META, &mut |key, value| {
            // the changed groups before this one, then this one, changed or not
            while let Some((_, blocks)) = pending.next_if(|&(&k, _)| k < key) {
                emit(blocks);
            }
            if let Some((_, blocks)) = pending.next_if(|&(&k, _)| k == key) {
                emit(blocks);
            } else {
                emit(&decode_page(value).1);
            }
        })?;
        for (_, blocks) in pending {
            emit(blocks);
        }
        Ok(())
    }
}

impl<B: KvBackend, M: Mode> NodeReader for KvReader<'_, B, M> {
    #[inline]
    fn get(&mut self, id: u64) -> Option<Location> {
        let Self {
            store,
            key,
            value,
            found,
            slots,
        } = self;
        slots.get(id, |num, locs| {
            let group = store.group(id);
            if let Some(blocks) = store.pending.get(&group) {
                let Ok(i) = blocks.binary_search_by_key(&num, |b| b.num) else {
                    return 0;
                };
                locs[..blocks[i].locs.len()].copy_from_slice(&blocks[i].locs);
                return blocks[i].mask;
            }
            if *key != group {
                *key = group;
                *found = store
                    .kv
                    .get(group, value)
                    .expect("cannot read the database");
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

impl<B: KvBackend> FileStore for KvStore<B, ReadWrite> {
    type Opts = KvOpts<B::Opts>;
    type ReadOnly = KvStore<B, ReadOnly>;

    fn open_with(path: impl AsRef<Path>, opts: &Self::Opts) -> Result<Self> {
        let path = path.as_ref();
        let (mut kv, created) = B::open(path, &opts.backend)?;
        let (meta, metadata) = if created {
            let meta = Meta {
                shift: BLOCK_SHIFT + opts.group_shift,
                stamp: 0,
                state: LoadState::Complete,
            };
            let metadata = Metadata::default();
            kv.commit(&[(META, Some(&meta.encode(&metadata)))], true)?;
            (meta, metadata)
        } else {
            Self::read_meta(&kv, path)?
        };
        Ok(Self::new(kv, meta, metadata))
    }

    /// The database repairs itself when it is opened, so there is nothing more to fix.
    fn open_and_fix_with(path: impl AsRef<Path>, opts: &Self::Opts) -> Result<(Self, FixReport)> {
        Ok((Self::open_with(path, opts)?, FixReport::default()))
    }

    fn open_read_only_with(
        path: impl AsRef<Path>,
        opts: &Self::Opts,
    ) -> Result<KvStore<B, ReadOnly>> {
        let path = path.as_ref();
        let kv = B::open_read_only(path, &opts.backend)?;
        let (meta, metadata) = Self::read_meta(&kv, path)?;
        Ok(KvStore::new(kv, meta, metadata))
    }
}

impl<B: KvBackend> NodeStore for KvStore<B, ReadWrite> {
    type Loader<'a>
        = KvLoader<'a, B>
    where
        Self: 'a;

    fn set(&mut self, id: u64, loc: Location) -> Result<()> {
        check(id, loc)?;
        self.modify(id, Some(loc))
    }

    fn remove(&mut self, id: u64) -> Result<()> {
        check(id, Location::default())?;
        self.modify(id, None)
    }

    fn set_metadata(&mut self, key: &str, value: &[u8]) -> Result<()> {
        self.metadata.set(key, value)
    }

    fn remove_metadata(&mut self, key: &str) {
        self.metadata.remove(key);
    }

    fn commit(&mut self, stamp: u64) -> Result<()> {
        self.require_complete()?;
        let encoded: Vec<(u64, Vec<u8>)> = self
            .pending
            .iter()
            .map(|(&key, blocks)| {
                let mut value = Vec::new();
                if let Some(first) = blocks.first() {
                    encode_page(first.num, blocks, &mut value);
                }
                (key, value)
            })
            .collect();
        let meta = Meta { stamp, ..self.meta };
        let meta_bytes = meta.encode(&self.metadata);
        let mut values: Vec<_> = encoded
            .iter()
            .map(|(key, v)| (*key, (!v.is_empty()).then_some(v.as_slice())))
            .collect();
        values.push((META, Some(&meta_bytes)));
        self.kv.commit(&values, true)?;
        self.pending.clear();
        self.meta = meta;
        Ok(())
    }

    fn bulk_load(&mut self) -> Result<KvLoader<'_, B>> {
        if !self.pending.is_empty() {
            return Err(Error::NotEmpty);
        }
        let resume = match self.meta.state {
            LoadState::Loading { last_id } => last_id,
            LoadState::Complete if self.kv.has_keys_below(META)? => return Err(Error::NotEmpty),
            LoadState::Complete => {
                // a load dropped or cut short before its first checkpoint must not look complete
                let meta = Meta {
                    state: LoadState::Loading { last_id: None },
                    ..self.meta
                };
                self.kv
                    .commit(&[(META, Some(&meta.encode(&self.metadata)))], true)?;
                self.meta = meta;
                None
            }
        };
        Ok(KvLoader {
            floor_group: resume.map(|id| self.group(id)),
            store: self,
            resume,
            last_id: None,
            slots: Slots::default(),
            blocks: Vec::new(),
            used: 0,
            values: Vec::with_capacity(BATCH),
            since_checkpoint: 0,
            unsaved: Vec::new(),
        })
    }
}

impl<B: KvBackend> KvStore<B, ReadWrite> {
    fn require_complete(&self) -> Result<()> {
        match self.meta.state {
            LoadState::Complete => Ok(()),
            LoadState::Loading { .. } => Err(Error::LoadUnfinished),
        }
    }

    fn modify(&mut self, id: u64, loc: Option<Location>) -> Result<()> {
        self.require_complete()?;
        let (num, bit) = block_of(id);
        let group = self.group(id);
        if !self.pending.contains_key(&group) {
            let blocks = self.read_group(group)?;
            self.pending.insert(group, blocks);
        }
        let blocks = self.pending.get_mut(&group).expect("just inserted");
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
}

/// Bulk loads a [`KvStore`]; see [`BulkLoad`].
pub struct KvLoader<'a, B: KvBackend> {
    store: &'a mut KvStore<B, ReadWrite>,
    resume: Option<u64>,
    /// groups up to this one are stored already
    floor_group: Option<u64>,
    last_id: Option<u64>,
    /// for checking the nodes of a resumed load against the store
    slots: Slots,
    /// the group being filled, in its first `used` blocks; the rest are kept for their memory
    blocks: Vec<Block>,
    used: usize,
    /// encoded groups not yet written
    values: Vec<(u64, Vec<u8>)>,
    since_checkpoint: usize,
    /// groups written since the last checkpoint, which a dropped load removes
    unsaved: Vec<u64>,
}

impl<B: KvBackend> KvLoader<'_, B> {
    /// Encodes the group being filled.
    fn add_group(&mut self) -> Result<()> {
        if self.used == 0 {
            return Ok(());
        }
        let blocks = &self.blocks[..self.used];
        let mut value = Vec::new();
        encode_page(blocks[0].num, blocks, &mut value);
        let last = &blocks[self.used - 1];
        let last_id = (last.num << BLOCK_SHIFT) | u64::from(last.mask.ilog2());
        let group = blocks[0].num >> (self.store.meta.shift - BLOCK_SHIFT);
        self.used = 0;
        self.add_value(group, value, last_id)
    }

    /// Adds an encoded group whose last node is `last_id`, and writes the groups when it is time.
    fn add_value(&mut self, group: u64, value: Vec<u8>, last_id: u64) -> Result<()> {
        self.values.push((group, value));
        self.since_checkpoint += 1;
        if self.since_checkpoint >= CHECKPOINT {
            self.write(
                LoadState::Loading {
                    last_id: Some(last_id),
                },
                true,
            )?;
            self.since_checkpoint = 0;
        } else if self.values.len() >= BATCH {
            self.write(self.store.meta.state, false)?;
        }
        Ok(())
    }

    /// Writes the encoded groups, and in a durable transaction the load state too.
    fn write(&mut self, state: LoadState, durable: bool) -> Result<()> {
        let meta = Meta {
            state,
            ..self.store.meta
        };
        let meta_bytes = meta.encode(&self.store.metadata);
        let mut values: Vec<_> = self
            .values
            .iter()
            .map(|(key, v)| (*key, Some(v.as_slice())))
            .collect();
        if durable {
            values.push((META, Some(&meta_bytes)));
        }
        self.store.kv.commit(&values, durable)?;
        if durable {
            self.unsaved.clear();
            self.store.meta = meta;
        } else {
            self.unsaved.extend(self.values.iter().map(|&(key, _)| key));
        }
        self.values.clear();
        Ok(())
    }
}

impl<B: KvBackend> Drop for KvLoader<'_, B> {
    /// Removes the groups written since the last checkpoint, so the store holds only the nodes up
    /// to it, as a reopened store would after a crash.
    fn drop(&mut self) {
        if !self.unsaved.is_empty() {
            let removed: Vec<_> = self.unsaved.iter().map(|&key| (key, None)).collect();
            // an error leaves them, and resuming the load writes them again
            let _ = self.store.kv.commit(&removed, true);
        }
    }
}

impl<B: KvBackend> BulkLoad for KvLoader<'_, B> {
    fn resume_after(&self) -> Option<u64> {
        self.resume
    }

    fn push(&mut self, id: u64, loc: Location) -> Result<()> {
        check(id, loc)?;
        check_order(self.last_id, id)?;
        let previous = self.last_id.replace(id);
        if self.resume.is_some_and(|r| id <= r) {
            let store = &*self.store;
            let stored = self.slots.get(id, |num, locs| {
                let blocks = store.read_group(store.group(id)).unwrap_or_default();
                blocks.iter().find(|b| b.num == num).map_or(0, |b| {
                    locs[..b.locs.len()].copy_from_slice(&b.locs);
                    b.mask
                })
            });
            return check_resumed(stored, id, loc);
        }
        let group = self.store.group(id);
        if self.floor_group.is_some_and(|f| group <= f) {
            // the interrupted load had finished this group
            return Err(Error::ResumeMismatch(id));
        }
        if previous.is_some_and(|p| self.store.group(p) != group) {
            self.add_group()?;
        }
        let (num, bit) = block_of(id);
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

    /// Encodes the groups of big batches on all threads.
    #[cfg(any(feature = "paged", feature = "redb"))]
    fn push_batch(&mut self, nodes: &[(u64, Location)]) -> Result<()> {
        use rayon::prelude::*;
        let store = &*self.store;
        // resumed nodes, and those of the group being filled, one by one
        let first_new = nodes.partition_point(|&(id, _)| self.resume.is_some_and(|r| id <= r));
        let open = self.last_id.map(|id| store.group(id));
        let one_by_one = first_new
            + nodes[first_new..]
                .iter()
                .take_while(|&&(id, _)| Some(store.group(id)) == open)
                .count();
        for &(id, loc) in &nodes[..one_by_one] {
            self.push(id, loc)?;
        }
        let rest = &nodes[one_by_one..];
        let store = &*self.store;
        // the last group may go on in the next batch
        let last_group = rest.last().map(|&(id, _)| store.group(id));
        let whole = rest.partition_point(|&(id, _)| Some(store.group(id)) != last_group);
        if whole < PARALLEL_NODES {
            for &(id, loc) in rest {
                self.push(id, loc)?;
            }
            return Ok(());
        }
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
            .floor_group
            .is_some_and(|f| store.group(rest[0].0) <= f)
        {
            return Err(Error::ResumeMismatch(rest[0].0));
        }
        self.add_group()?;
        let store = &*self.store;
        let nodes = &rest[..whole];
        // the index of each group's first node
        let mut starts = vec![0];
        starts.par_extend(
            (1..nodes.len())
                .into_par_iter()
                .filter(|&i| store.group(nodes[i].0) != store.group(nodes[i - 1].0)),
        );
        starts.push(nodes.len());
        let groups = starts.len() - 1;
        let encoded: Vec<Vec<(u64, Vec<u8>, u64)>> = (0..groups)
            .step_by(PARALLEL_GROUPS)
            .collect::<Vec<_>>()
            .into_par_iter()
            .map(|g| {
                let mut blocks: Vec<Block> = Vec::new();
                starts[g..=(g + PARALLEL_GROUPS).min(groups)]
                    .windows(2)
                    .map(|w| {
                        let group = &nodes[w[0]..w[1]];
                        blocks.clear();
                        for &(id, loc) in group {
                            let (num, bit) = block_of(id);
                            match blocks.last_mut() {
                                Some(b) if b.num == num => {
                                    b.mask |= bit;
                                    b.locs.push(loc);
                                }
                                _ => blocks.push(Block {
                                    num,
                                    mask: bit,
                                    locs: vec![loc],
                                }),
                            }
                        }
                        let mut value = Vec::new();
                        encode_page(blocks[0].num, &blocks, &mut value);
                        (store.group(group[0].0), value, group[group.len() - 1].0)
                    })
                    .collect()
            })
            .collect();
        for (group, value, last_id) in encoded.into_iter().flatten() {
            self.add_value(group, value, last_id)?;
        }
        self.last_id = Some(nodes[nodes.len() - 1].0);
        for &(id, loc) in &rest[whole..] {
            self.push(id, loc)?;
        }
        Ok(())
    }

    fn finish(mut self, stamp: u64) -> Result<()> {
        self.add_group()?;
        self.store.meta.stamp = stamp;
        self.write(LoadState::Complete, true)
    }
}

/// An error of a backend database
#[cfg(feature = "redb")]
pub(crate) fn backend_err(e: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> Error {
    Error::Backend(e.into())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{Meta, BLOCK_SHIFT};
    use crate::{Error, LoadState, Metadata};

    /// A reserved value cut short anywhere is an error, not a panic.
    #[test]
    fn rejects_truncated_metadata() {
        let meta = Meta {
            shift: BLOCK_SHIFT + 2,
            stamp: 7,
            state: LoadState::Loading { last_id: Some(9) },
        };
        let mut metadata = Metadata::default();
        metadata.set("source", b"planet.osm.pbf").unwrap();
        let m = meta.encode(&metadata);
        let path = Path::new("store.redb");
        assert_eq!(Meta::decode(&m, path).unwrap(), (meta, metadata));
        for len in 0..m.len() {
            let res = Meta::decode(&m[..len], path);
            assert!(
                matches!(res, Err(Error::NotAStore(_) | Error::Corrupt { .. })),
                "{len} bytes: {res:?}"
            );
        }
    }
}
