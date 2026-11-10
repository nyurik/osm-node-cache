//! The traits every store implements.

use std::collections::BTreeMap;
use std::ops::RangeInclusive;
use std::path::Path;
use std::str;

use super::codec::{u16_at, u64_at};
use super::{MAX_NODE_ID, NodeStoreError, Result};
use crate::Location;

/// The most bytes [`Metadata`] may take, encoded: 2 for the entry count, and per entry 1 for the
/// key's length, the key, 2 for the value's length, and the value.
pub const MAX_METADATA_BYTES: usize = 1024;

/// A few named values kept with a store, such as where its data came from, committed with its
/// nodes. Keys are 1 to 255 bytes of UTF-8; all of it may take [`MAX_METADATA_BYTES`] encoded.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Metadata(BTreeMap<String, Vec<u8>>);

impl Metadata {
    /// The value of `key`. Values are bytes, as they need not be text.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&[u8]> {
        self.0.get(key).map(Vec::as_slice)
    }

    /// The entries, in key order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &[u8])> {
        self.0.iter().map(|(k, v)| (k.as_str(), v.as_slice()))
    }

    /// The number of entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether there are no entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The bytes it takes encoded.
    #[must_use]
    pub fn encoded_len(&self) -> usize {
        2 + self
            .0
            .iter()
            .map(|(k, v)| 3 + k.len() + v.len())
            .sum::<usize>()
    }

    pub(crate) fn set(&mut self, key: &str, value: &[u8]) -> Result<()> {
        if key.is_empty() || key.len() > 255 {
            return Err(NodeStoreError::MetadataTooLarge);
        }
        let old = self.0.insert(key.to_owned(), value.to_vec());
        if self.encoded_len() > MAX_METADATA_BYTES {
            match old {
                Some(old) => self.0.insert(key.to_owned(), old),
                None => self.0.remove(key),
            };
            return Err(NodeStoreError::MetadataTooLarge);
        }
        Ok(())
    }

    pub(crate) fn remove(&mut self, key: &str) {
        self.0.remove(key);
    }

    #[expect(clippy::cast_possible_truncation)]
    pub(crate) fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&(self.0.len() as u16).to_le_bytes());
        for (k, v) in &self.0 {
            out.push(k.len() as u8);
            out.extend_from_slice(k.as_bytes());
            out.extend_from_slice(&(v.len() as u16).to_le_bytes());
            out.extend_from_slice(v);
        }
    }

    /// Decodes metadata from the start of `b`, if it is valid.
    pub(crate) fn decode(b: &[u8]) -> Option<Self> {
        let mut map = BTreeMap::new();
        let n = u16_at(b.get(..2)?, 0);
        let mut at = 2;
        for _ in 0..n {
            let k = usize::from(*b.get(at)?);
            let key = str::from_utf8(b.get(at + 1..at + 1 + k)?).ok()?;
            at += 1 + k;
            let v = usize::from(u16_at(b.get(at..at + 2)?, 0));
            map.insert(key.to_owned(), b.get(at + 2..at + 2 + v)?.to_vec());
            at += 2 + v;
        }
        let meta = Self(map);
        (meta.encoded_len() <= MAX_METADATA_BYTES).then_some(meta)
    }
}

mod sealed {
    pub trait Sealed {}
    impl Sealed for super::ReadOnly {}
    impl Sealed for super::ReadWrite {}
}

/// Whether a store was opened to read only, or to read and write. A read-only store has none of
/// the [`NodeStore`] methods that change it.
pub trait Mode: sealed::Sealed + Send + Sync + 'static {}

/// Opened with [`FileStore::open_read_only`]: other processes may read the store at the same time.
#[derive(Debug)]
pub struct ReadOnly;

/// Opened with [`FileStore::open`] or [`FileStore::open_and_fix`]: no one else may open the store.
#[derive(Debug)]
pub struct ReadWrite;

impl Mode for ReadOnly {}
impl Mode for ReadWrite {}

/// Whether a store's bulk load is complete.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoadState {
    /// The store is loaded, or was never bulk loaded, and can be changed.
    Complete,
    /// A bulk load was started and not finished. It kept the nodes up to `last_id`, if any, and
    /// readers see at least those; [`NodeStore::bulk_load`] resumes the load.
    Loading {
        /// The highest node ID the interrupted load kept, if it kept any.
        last_id: Option<u64>,
    },
}

impl LoadState {
    /// Writes the state into a file store's header at `at`: the last loaded ID (u64), then whether a bulk load is
    /// unfinished (1) and has kept nodes (2). The header is zero there before.
    pub(crate) fn encode(self, h: &mut [u8], at: usize) {
        if let Self::Loading { last_id } = self {
            h[at..at + 8].copy_from_slice(&last_id.unwrap_or(0).to_le_bytes());
            h[at + 8] = 1 + u8::from(last_id.is_some());
        }
    }

    /// The state that [`encode`](Self::encode) wrote at `at`.
    pub(crate) fn decode(h: &[u8], at: usize) -> Self {
        match h[at + 8] {
            0 => Self::Complete,
            1 => Self::Loading { last_id: None },
            _ => Self::Loading {
                last_id: Some(u64_at(h, at)),
            },
        }
    }
}

/// What [`FileStore::open_and_fix`] repaired.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FixReport {
    /// Whether the store's index of its data was rebuilt from the data, which loses nothing.
    pub rebuilt_index: bool,
    /// The node IDs whose locations were lost with damaged data, as ranges.
    pub lost: Vec<RangeInclusive<u64>>,
}

impl FixReport {
    /// Whether the store needed no fixing.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        !self.rebuilt_index && self.lost.is_empty()
    }
}

/// Reading node locations, which every store can do however it was opened.
pub trait NodeRead: Sync + Sized {
    /// What [`NodeRead::reader`] returns.
    type Reader<'a>: NodeReader
    where
        Self: 'a;

    /// A reader for one thread, faster than [`NodeRead::get`] for many lookups: a paged store's
    /// reader keeps up to 4,096 blocks of 64 node IDs it read decoded (2 MiB), and a way's nodes
    /// are mostly in a few blocks. Keep one per thread rather than making many.
    fn reader(&self) -> Self::Reader<'_>;

    /// The location of node `id`, if it is stored.
    fn get(&self, id: u64) -> Option<Location> {
        self.reader().get(id)
    }

    /// The stamp of the last commit, such as the replication sequence its locations reflect.
    fn stamp(&self) -> u64;

    /// Whether a bulk load was started and not finished, and how far it got.
    fn load_state(&self) -> LoadState;

    /// The store's metadata, with the changes not yet committed.
    fn metadata(&self) -> &Metadata;

    /// Calls `f` with every stored node, in ID order.
    fn for_each(&self, f: impl FnMut(u64, Location)) -> Result<()>;

    /// Reads all of the store's files into memory, in big reads on many threads, ahead of lookups
    /// all over it, such as resolving the nodes of all ways. When the files fit in memory, this
    /// is much faster than reading them a page at a time on first use; when they do not, it is
    /// of little use. A memory store has nothing to read.
    fn preload(&self) -> Result<()> {
        Ok(())
    }
}

/// Reads node locations for one thread.
pub trait NodeReader {
    /// The location of node `id`, if it is stored. The stores kept in a file read it through a
    /// memory map, so a file that cannot be read, as after a disk error, gives a `SIGBUS`.
    fn get(&mut self, id: u64) -> Option<Location>;
}

/// Changing node locations, which every store opened to read and write can do.
///
/// Changes are seen by readers made after them. A store kept in a file ([`FileStore`]) keeps them
/// with [`NodeStore::commit`]; those not committed are lost when the store is dropped, or by a
/// crash, except in a dense store, which writes them in place: after a crash it may have some of
/// them, and applying the same changes again gives the right result. A memory store has no file,
/// and keeps nothing once it is dropped.
pub trait NodeStore: NodeRead {
    /// What [`NodeStore::bulk_load`] returns, to feed nodes in ascending ID order.
    type Loader<'a>: BulkLoad
    where
        Self: 'a;

    /// Stores the location of node `id`, replacing any earlier one.
    fn set(&mut self, id: u64, loc: Location) -> Result<()>;

    /// Removes node `id`, if it is stored.
    fn remove(&mut self, id: u64) -> Result<()>;

    /// Sets a metadata entry, kept by the next commit, or bulk load checkpoint or finish.
    fn set_metadata(&mut self, key: &str, value: &[u8]) -> Result<()>;

    /// Removes a metadata entry, kept by the next commit, or bulk load checkpoint or finish.
    fn remove_metadata(&mut self, key: &str);

    /// Keeps all changes since the last commit, together with `stamp`.
    fn commit(&mut self, stamp: u64) -> Result<()>;

    /// Starts a bulk load into an empty store, or resumes one that was interrupted.
    fn bulk_load(&mut self) -> Result<Self::Loader<'_>>;
}

/// Opening a store kept in a file, which every store but the memory store is.
///
/// [`FileStore::open`] and [`FileStore::open_read_only`] check what they read to open the store,
/// such as its header, and damage there is an error. They do not check the node data, which would
/// read the whole file: damage in it, as after a disk error, shows only when the damaged part is
/// read, as wrong locations or a panic. [`FileStore::open_and_fix`] checks the data too, where the
/// store keeps a way to: `PagedStore` reads every page and checks its CRC-32.
pub trait FileStore: NodeStore {
    /// Options for opening the store; the methods without `_with` use their `Default`.
    type Opts: Default;
    /// The type [`FileStore::open_read_only`] returns.
    type ReadOnly: NodeRead;

    /// Opens the store at `path` to read and write, creating it if it is missing. Rolls back a
    /// commit interrupted by a crash; other damage it finds is an error. It does not check the node
    /// data, as the [`FileStore`] documentation says.
    fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::open_with(path, &Self::Opts::default())
    }

    /// [`FileStore::open`] with options.
    fn open_with(path: impl AsRef<Path>, opts: &Self::Opts) -> Result<Self>;

    /// Opens the store as [`FileStore::open`] does, checks its node data too where the store keeps a
    /// way to, and repairs what it can of a damaged store. `PagedStore` reads every page for it,
    /// which takes as long as reading the whole file.
    fn open_and_fix(path: impl AsRef<Path>) -> Result<(Self, FixReport)> {
        Self::open_and_fix_with(path, &Self::Opts::default())
    }

    /// [`FileStore::open_and_fix`] with options.
    fn open_and_fix_with(path: impl AsRef<Path>, opts: &Self::Opts) -> Result<(Self, FixReport)>;

    /// Opens an existing store to read only, sharing it with other readers, checking it as
    /// [`FileStore::open`] does. A store with an interrupted commit is an error, as rolling it back
    /// needs writing.
    fn open_read_only(path: impl AsRef<Path>) -> Result<Self::ReadOnly> {
        Self::open_read_only_with(path, &Self::Opts::default())
    }

    /// [`FileStore::open_read_only`] with options.
    fn open_read_only_with(path: impl AsRef<Path>, opts: &Self::Opts) -> Result<Self::ReadOnly>;
}

/// Loads a store from nodes in ascending ID order, much faster than [`NodeStore::set`]. A store
/// kept in a file keeps what it loaded every so often, so a load interrupted by a crash can resume:
/// feed it the same nodes from the start, and it checks those it already has instead of storing
/// them again.
///
/// Dropping a loader without [`BulkLoad::finish`] keeps the nodes up to its last checkpoint, except
/// in a memory store, which it leaves empty.
pub trait BulkLoad: Sized {
    /// The last node ID an interrupted load kept. Nodes up to it are checked against the store.
    fn resume_after(&self) -> Option<u64>;

    /// Adds the location of node `id`, which must be above all IDs added before.
    fn push(&mut self, id: u64, loc: Location) -> Result<()>;

    /// Adds nodes in ascending ID order; faster than [`BulkLoad::push`] for some stores.
    fn push_batch(&mut self, nodes: &[(u64, Location)]) -> Result<()> {
        for &(id, loc) in nodes {
            self.push(id, loc)?;
        }
        Ok(())
    }

    /// Writes the rest, and commits the loaded store with `stamp`.
    fn finish(self, stamp: u64) -> Result<()>;
}

/// Checks a node before it is stored.
pub(crate) fn check(id: u64, loc: Location) -> Result<()> {
    if id > MAX_NODE_ID {
        Err(NodeStoreError::IdTooLarge(id))
    } else if loc.is_valid() {
        Ok(())
    } else {
        Err(NodeStoreError::InvalidLocation(loc))
    }
}

/// Checks the order of bulk loaded nodes.
pub(crate) fn check_order(last: Option<u64>, id: u64) -> Result<()> {
    match last {
        Some(last) if id <= last => Err(NodeStoreError::NotAscending { id, last }),
        _ => Ok(()),
    }
}

/// Checks a bulk loaded node, and that it comes after `last`, which it then becomes.
pub(crate) fn check_next(last: &mut Option<u64>, id: u64, loc: Location) -> Result<()> {
    check(id, loc)?;
    check_order(*last, id)?;
    *last = Some(id);
    Ok(())
}

/// Checks a node a resumed load was given again against the stored one.
pub(crate) fn check_resumed(stored: Option<Location>, id: u64, loc: Location) -> Result<()> {
    if stored == Some(loc) {
        Ok(())
    } else {
        Err(NodeStoreError::ResumeMismatch(id))
    }
}

/// The same checks for every store.
#[cfg(test)]
#[allow(clippy::panic, reason = "test helpers fail the test")]
pub(crate) mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::Path;

    use rand::rngs::StdRng;
    use rand::{RngExt as _, SeedableRng as _};

    use super::{BulkLoad as _, FileStore, NodeRead, NodeReader as _, NodeStore};
    use crate::Location;
    use crate::nodes::file::tests::TestFile;
    use crate::nodes::{
        DenseStore, LoadState, MAX_METADATA_BYTES, MAX_NODE_ID, MemoryStore, Metadata,
        NodeStoreError, PagedStore,
    };

    /// IDs mostly in runs, with gaps of all sizes, and locations a short step apart.
    pub(crate) fn nodes(rng: &mut StdRng, count: usize) -> BTreeMap<u64, Location> {
        let (mut id, mut loc) = (1, Location::new(0, 0));
        (0..count)
            .map(|_| {
                id += match rng.random_range(0..100) {
                    0..80 => 1,
                    80..98 => rng.random_range(2..200),
                    _ => rng.random_range(200..100_000),
                };
                loc = Location::new(
                    (loc.lat + rng.random_range(-2000..=2000)).clamp(-900_000_000, 900_000_000),
                    (loc.lon + rng.random_range(-2000..=2000)).clamp(-1_800_000_000, 1_800_000_000),
                );
                (id, loc)
            })
            .collect()
    }

    pub(crate) fn random_location(rng: &mut StdRng) -> Location {
        Location::new(
            rng.random_range(-900_000_000..=900_000_000),
            rng.random_range(-1_800_000_000..=1_800_000_000),
        )
    }

    /// Bulk loads `nodes` into an empty store, in batches of `batch`.
    pub(crate) fn load<S: NodeStore>(
        store: &mut S,
        nodes: &BTreeMap<u64, Location>,
        stamp: u64,
        batch: usize,
    ) {
        let mut load = store.bulk_load().unwrap();
        let all: Vec<(u64, Location)> = nodes.iter().map(|(&k, &v)| (k, v)).collect();
        for chunk in all.chunks(batch) {
            load.push_batch(chunk).unwrap();
        }
        load.finish(stamp).unwrap();
    }

    /// Creates the store at `path`, and bulk loads `nodes`, in batches of `batch`.
    pub(crate) fn build<S: FileStore>(
        path: &Path,
        nodes: &BTreeMap<u64, Location>,
        stamp: u64,
        batch: usize,
    ) -> S {
        let mut store = S::open(path).unwrap();
        load(&mut store, nodes, stamp, batch);
        store
    }

    /// A store for a test, kept in a file or in memory.
    pub(crate) trait TestStore: NodeStore {
        /// Its file, if it has one, removed when dropped.
        type File;

        /// An empty store for the test `name`.
        fn create(name: &str) -> (Self, Self::File);

        /// Checks that the store has what was committed: `expected` nodes, `stamp` and `metadata`,
        /// reopened from its file to read and write, and to read only, if it has a file.
        fn check_committed(
            self,
            file: &Self::File,
            expected: &BTreeMap<u64, Location>,
            stamp: u64,
            metadata: &Metadata,
        );
    }

    impl<S: FileStore> TestStore for S {
        type File = TestFile;

        fn create(name: &str) -> (Self, TestFile) {
            let f = TestFile::new(name);
            (S::open(&f.0).unwrap(), f)
        }

        fn check_committed(
            self,
            f: &TestFile,
            expected: &BTreeMap<u64, Location>,
            stamp: u64,
            metadata: &Metadata,
        ) {
            drop(self);
            let store = S::open(&f.0).unwrap();
            assert_eq!((store.stamp(), store.metadata()), (stamp, metadata));
            check(&store, expected);
            drop(store);
            let store = S::open_read_only(&f.0).unwrap();
            assert_eq!((store.stamp(), store.metadata()), (stamp, metadata));
            check(&store, expected);
        }
    }

    impl TestStore for MemoryStore {
        type File = ();

        fn create(_name: &str) -> (Self, ()) {
            (Self::new(), ())
        }

        fn check_committed(
            self,
            (): &(),
            expected: &BTreeMap<u64, Location>,
            stamp: u64,
            metadata: &Metadata,
        ) {
            assert_eq!((self.stamp(), self.metadata()), (stamp, metadata));
            check(&self, expected);
        }
    }

    /// Checks the stored nodes one by one, with a reader, and all of them in order.
    pub(crate) fn check<S: NodeRead>(store: &S, expected: &BTreeMap<u64, Location>) {
        let mut reader = store.reader();
        for (&id, &loc) in expected {
            assert_eq!(reader.get(id), Some(loc), "node {id}");
            for id in [id.saturating_sub(1), id + 1] {
                assert_eq!(reader.get(id), expected.get(&id).copied(), "node {id}");
            }
        }
        for (&id, &loc) in expected.iter().step_by(97) {
            assert_eq!(store.get(id), Some(loc), "node {id}");
        }
        assert_eq!(store.get(MAX_NODE_ID), expected.get(&MAX_NODE_ID).copied());
        let mut all = Vec::new();
        store.for_each(|id, loc| all.push((id, loc))).unwrap();
        assert!(
            all.into_iter().eq(expected.iter().map(|(&k, &v)| (k, v))),
            "every node, in order"
        );
    }

    fn round_trip<S: TestStore>(name: &str) {
        let (mut store, f) = S::create(name);
        let expected = nodes(&mut StdRng::seed_from_u64(1), 50_000);
        load(&mut store, &expected, 7, 20_000);
        assert_eq!(store.stamp(), 7);
        assert_eq!(store.load_state(), LoadState::Complete);
        // Reads the file while the store, open to write, holds its lock.
        store.preload().unwrap();
        check(&store, &expected);
        store.check_committed(&f, &expected, 7, &Metadata::default());
    }

    fn empty<S: TestStore>(name: &str) {
        let (mut store, f) = S::create(name);
        check(&store, &BTreeMap::new());
        store.set(5, Location::new(1, 2)).unwrap();
        store.commit(1).unwrap();
        let expected = BTreeMap::from([(5, Location::new(1, 2))]);
        store.check_committed(&f, &expected, 1, &Metadata::default());
    }

    /// Moves, removes and adds nodes, as replication diffs do, checking before and after commits.
    fn changes_match_a_map<S: TestStore>(name: &str) {
        let (mut store, f) = S::create(name);
        let rng = &mut StdRng::seed_from_u64(2);
        let mut expected = nodes(rng, 30_000);
        load(&mut store, &expected, 1, 1000);
        let ids: Vec<u64> = expected.keys().copied().collect();
        let mut next = ids[ids.len() - 1] + 1;
        for round in 2..6 {
            for _ in 0..3000 {
                let id = ids[rng.random_range(0..ids.len())];
                match rng.random_range(0..10) {
                    0..5 => {
                        let loc = random_location(rng);
                        store.set(id, loc).unwrap();
                        expected.insert(id, loc);
                    }
                    5..7 => {
                        // a run, emptying whole blocks now and then
                        for id in id..id + rng.random_range(1..150) {
                            store.remove(id).unwrap();
                            expected.remove(&id);
                        }
                    }
                    _ => {
                        let loc = random_location(rng);
                        store.set(next, loc).unwrap();
                        expected.insert(next, loc);
                        next += rng.random_range(1..3);
                    }
                }
            }
            check(&store, &expected);
            store.commit(round).unwrap();
            check(&store, &expected);
        }
        store.check_committed(&f, &expected, 5, &Metadata::default());
    }

    /// Changes and metadata not committed are gone after reopening.
    fn drops_uncommitted<S: FileStore>(name: &str) {
        let f = TestFile::new(name);
        let expected = nodes(&mut StdRng::seed_from_u64(3), 10_000);
        let mut store = build::<S>(&f.0, &expected, 1, 1000);
        let (&first, _) = expected.first_key_value().unwrap();
        store.remove(first).unwrap();
        store
            .set(first + 1_000_000_000, Location::new(3, 4))
            .unwrap();
        store.set_metadata("k", b"v").unwrap();
        drop(store);
        let store = S::open(&f.0).unwrap();
        assert_eq!(store.stamp(), 1);
        assert!(store.metadata().is_empty());
        check(&store, &expected);
    }

    fn locks<S: FileStore>(name: &str) {
        let f = TestFile::new(name);
        let store = S::open(&f.0).unwrap();
        assert!(matches!(S::open(&f.0), Err(NodeStoreError::Locked(_))));
        assert!(matches!(
            S::open_read_only(&f.0),
            Err(NodeStoreError::Locked(_))
        ));
        drop(store);
        let (a, b) = (
            S::open_read_only(&f.0).unwrap(),
            S::open_read_only(&f.0).unwrap(),
        );
        assert!(matches!(S::open(&f.0), Err(NodeStoreError::Locked(_))));
        drop((a, b));
        drop(S::open(&f.0).unwrap());
    }

    /// A creation cut short after the file grew but before its header was written leaves only zeros,
    /// which opening the store for writing starts again.
    fn finishes_an_unfinished_creation<S: FileStore>(name: &str) {
        let f = TestFile::new(name);
        fs::write(&f.0, [0_u8; 4096]).unwrap();
        assert!(matches!(
            S::open_read_only(&f.0),
            Err(NodeStoreError::NotAStore(_))
        ));
        let mut store = S::open(&f.0).unwrap();
        check(&store, &BTreeMap::new());
        store.set(5, Location::new(1, 2)).unwrap();
        store.commit(1).unwrap();
        drop(store);
        let expected = BTreeMap::from([(5, Location::new(1, 2))]);
        check(&S::open_read_only(&f.0).unwrap(), &expected);
    }

    /// A load dropped before its first checkpoint starts over, and keeps none of the nodes it wrote
    /// when the new input lacks them.
    fn restarts_a_load_cut_short<S: FileStore>(name: &str) {
        let f = TestFile::new(name);
        let mut store = S::open(&f.0).unwrap();
        let mut load = store.bulk_load().unwrap();
        // far enough apart to be written to the file before the first checkpoint
        for id in 1..=10 {
            load.push(id * 1000, Location::new(1, 1)).unwrap();
        }
        drop(load);
        drop(store);
        let mut store = S::open(&f.0).unwrap();
        let mut load = store.bulk_load().unwrap();
        assert_eq!(load.resume_after(), None);
        load.push(5, Location::new(2, 2)).unwrap();
        load.finish(1).unwrap();
        check(&store, &BTreeMap::from([(5, Location::new(2, 2))]));
    }

    fn rejects_other_files<S: FileStore>(name: &str) {
        let f = TestFile::new(name);
        assert!(matches!(
            S::open_read_only(&f.0),
            Err(NodeStoreError::NotFound(_))
        ));
        assert!(
            !f.0.exists(),
            "a missing file is not created by a read-only open"
        );
        let other = b"not a node store, but long enough to look like one at first ".repeat(80);
        fs::write(&f.0, &other).unwrap();
        assert!(matches!(S::open(&f.0), Err(NodeStoreError::NotAStore(_))));
        assert!(matches!(
            S::open_read_only(&f.0),
            Err(NodeStoreError::NotAStore(_))
        ));
        assert_eq!(fs::read(&f.0).unwrap(), other, "the file is left as it was");
    }

    /// The highest ID a store takes is stored and removed like any other.
    fn takes_the_highest_id<S: TestStore>(name: &str) {
        // Only this node is looked up: a full check would read every ID the file has room for.
        let (mut store, f) = S::create(name);
        let ok = Location::new(1, 1);
        store.set(MAX_NODE_ID, ok).unwrap();
        assert_eq!(store.get(MAX_NODE_ID), Some(ok));
        store.commit(1).unwrap();
        store.remove(MAX_NODE_ID).unwrap();
        assert_eq!(store.get(MAX_NODE_ID), None);
        drop((store, f));
    }

    fn rejects_bad_nodes<S: TestStore>(name: &str) {
        let (mut store, _f) = S::create(name);
        let ok = Location::new(1, 1);
        assert!(matches!(
            store.set(MAX_NODE_ID + 1, ok),
            Err(NodeStoreError::IdTooLarge(_))
        ));
        assert!(matches!(
            store.remove(u64::MAX),
            Err(NodeStoreError::IdTooLarge(_))
        ));
        for bad in [Location::new(900_000_001, 0), Location::new(0, i32::MIN)] {
            assert!(matches!(
                store.set(1, bad),
                Err(NodeStoreError::InvalidLocation(_))
            ));
        }
        for id in [MAX_NODE_ID + 1, 1 << 61, u64::MAX] {
            assert_eq!(store.get(id), None, "{id}");
        }
        store.set(1, ok).unwrap();
        assert!(matches!(
            store.bulk_load().err(),
            Some(NodeStoreError::NotEmpty)
        ));
        store.remove(1).unwrap();
        store.commit(0).unwrap();
        drop(store);
        let (mut store, _f) = S::create(&format!("{name}_load"));
        let mut load = store.bulk_load().unwrap();
        load.push(10, ok).unwrap();
        assert!(matches!(
            load.push(10, ok),
            Err(NodeStoreError::NotAscending { id: 10, last: 10 })
        ));
        assert!(matches!(
            load.push(MAX_NODE_ID + 1, ok),
            Err(NodeStoreError::IdTooLarge(_))
        ));
    }

    /// A load dropped part way resumes from its last checkpoint, checking what it kept.
    fn resumes_a_load<S: FileStore>(name: &str) {
        let f = TestFile::new(name);
        let expected = nodes(&mut StdRng::seed_from_u64(4), 60_000);
        let all: Vec<(u64, Location)> = expected.iter().map(|(&k, &v)| (k, v)).collect();
        let mut store = S::open(&f.0).unwrap();
        store.set_metadata("source", b"test.osm.pbf").unwrap();
        let mut load = store.bulk_load().unwrap();
        assert_eq!(load.resume_after(), None);
        load.push_batch(&all[..40_000]).unwrap();
        drop(load);
        drop(store);

        let mut store = S::open(&f.0).unwrap();
        let state = store.load_state();
        let LoadState::Loading {
            last_id: Some(last),
        } = state
        else {
            panic!("a dropped load is unfinished: {state:?}");
        };
        assert!(
            last >= all[1000].0 && last <= all[39_999].0,
            "kept up to {last}"
        );
        assert_eq!(store.metadata().get("source"), Some(&b"test.osm.pbf"[..]));
        assert!(matches!(
            store.set(1, Location::new(1, 1)),
            Err(NodeStoreError::LoadUnfinished)
        ));
        assert!(matches!(
            store.commit(9),
            Err(NodeStoreError::LoadUnfinished)
        ));
        // the same input, but for a node the load kept
        let mut load = store.bulk_load().unwrap();
        assert_eq!(load.resume_after(), Some(last));
        let (id, loc) = all[500];
        load.push_batch(&all[..500]).unwrap();
        let moved = Location::new(loc.lat + 1, loc.lon);
        assert!(matches!(
            load.push(id, moved),
            Err(NodeStoreError::ResumeMismatch(_))
        ));
        drop(load);
        let mut load = store.bulk_load().unwrap();
        for chunk in all.chunks(7000) {
            load.push_batch(chunk).unwrap();
        }
        load.finish(3).unwrap();
        assert_eq!(store.load_state(), LoadState::Complete);
        check(&store, &expected);
        drop(store);
        check(&S::open_read_only(&f.0).unwrap(), &expected);
    }

    /// A dropped load keeps only the nodes up to its last checkpoint, before and after reopening.
    fn drops_a_load_after_its_checkpoint<S: FileStore>(name: &str) {
        let f = TestFile::new(name);
        let expected = nodes(&mut StdRng::seed_from_u64(5), 40_000);
        let all: Vec<(u64, Location)> = expected.iter().map(|(&k, &v)| (k, v)).collect();
        let mut store = S::open(&f.0).unwrap();
        let mut load = store.bulk_load().unwrap();
        load.push_batch(&all).unwrap();
        drop(load);
        let state = store.load_state();
        let LoadState::Loading {
            last_id: Some(last),
        } = state
        else {
            panic!("a dropped load is unfinished: {state:?}");
        };
        let kept: BTreeMap<u64, Location> =
            expected.range(..=last).map(|(&k, &v)| (k, v)).collect();
        assert!(
            kept.len() < all.len(),
            "the load went past its last checkpoint: kept {} of {}",
            kept.len(),
            all.len()
        );
        check(&store, &kept);
        drop(store);
        check(&S::open(&f.0).unwrap(), &kept);
    }

    fn metadata<S: TestStore>(name: &str) {
        let (mut store, f) = S::create(name);
        store
            .set_metadata("replication", b"https://example.org/minute")
            .unwrap();
        store
            .set_metadata("sequence", &42_u64.to_le_bytes())
            .unwrap();
        store.set_metadata("gone", b"x").unwrap();
        store.remove_metadata("gone");
        assert_eq!(store.metadata().len(), 2);
        assert!(matches!(
            store.set_metadata("", b"x"),
            Err(NodeStoreError::MetadataTooLarge)
        ));
        let big = vec![7; MAX_METADATA_BYTES];
        assert!(matches!(
            store.set_metadata("big", &big),
            Err(NodeStoreError::MetadataTooLarge)
        ));
        assert_eq!(
            store.metadata().get("big"),
            None,
            "a rejected entry is not kept"
        );
        let fits = vec![7; MAX_METADATA_BYTES - store.metadata().encoded_len() - 6];
        store.set_metadata("big", &fits).unwrap();
        assert_eq!(store.metadata().encoded_len(), MAX_METADATA_BYTES);
        store.commit(1).unwrap();
        let expected = store.metadata().clone();
        assert_eq!(expected.get("sequence"), Some(&42_u64.to_le_bytes()[..]));
        store.check_committed(&f, &BTreeMap::new(), 1, &expected);
    }

    macro_rules! store_tests {
        ($store:ty, $($test:ident),+) => {
            $(
                #[test]
                fn $test() {
                    super::$test::<$store>(concat!(stringify!($test), ".", module_path!()));
                }
            )+
        };
    }

    mod paged {
        store_tests!(
            super::PagedStore,
            round_trip,
            empty,
            changes_match_a_map,
            drops_uncommitted,
            locks,
            finishes_an_unfinished_creation,
            restarts_a_load_cut_short,
            rejects_other_files,
            rejects_bad_nodes,
            takes_the_highest_id,
            resumes_a_load,
            drops_a_load_after_its_checkpoint,
            metadata
        );
    }

    mod dense {
        // changes are written in place, so uncommitted ones are not dropped
        store_tests!(
            super::DenseStore,
            round_trip,
            empty,
            changes_match_a_map,
            locks,
            finishes_an_unfinished_creation,
            restarts_a_load_cut_short,
            rejects_other_files,
            rejects_bad_nodes,
            resumes_a_load,
            metadata
        );

        // The highest ID makes a 512 GiB file. NTFS keeps no holes in a file not marked sparse, so
        // Windows would allocate all of it.
        #[cfg(not(windows))]
        store_tests!(super::DenseStore, takes_the_highest_id);
    }

    mod memory {
        // with no file, nothing is kept to check after reopening
        store_tests!(
            super::MemoryStore,
            round_trip,
            empty,
            changes_match_a_map,
            rejects_bad_nodes,
            takes_the_highest_id,
            metadata
        );
    }
}
