//! The traits that every node location store implements.

use std::path::Path;

use crate::{Location, OsmNodeCacheResult};

/// Node locations by ID, in a file or a database.
///
/// Changes are seen by readers made after them, and are kept once committed. A store that was not
/// closed cleanly opens with its last committed state, or, for [`crate::PagedFileCache`], with all
/// changes written before a process crash (a power loss may lose some of them).
pub trait NodeStore: Sync + Sized {
    type Opts: Default;
    type Reader<'a>: NodeReader
    where
        Self: 'a;
    type Builder: NodeBuilder<Store = Self>;

    /// An empty store at `path`, to be loaded in ascending ID order; replaces any store there.
    fn builder(path: &Path, opts: &Self::Opts) -> OsmNodeCacheResult<Self::Builder>;

    fn open(path: &Path, opts: &Self::Opts) -> OsmNodeCacheResult<Self>;

    /// A reader for one thread, faster than [`NodeStore::get`] for many lookups.
    fn reader(&self) -> Self::Reader<'_>;

    /// The location of node `id`, if it is stored.
    fn get(&self, id: u64) -> Option<Location> {
        self.reader().get(id)
    }

    /// Stores the location of node `id`, replacing any earlier one.
    fn set(&mut self, id: u64, loc: Location) -> OsmNodeCacheResult<()>;

    /// Removes node `id`, if it is stored.
    fn remove(&mut self, id: u64) -> OsmNodeCacheResult<()>;

    /// The stamp of the last commit, such as the replication sequence its locations reflect.
    fn stamp(&self) -> u64;

    /// Keeps all changes since the last commit, together with `stamp`.
    fn commit(&mut self, stamp: u64) -> OsmNodeCacheResult<()>;
}

/// Reads locations for one thread, keeping recently read blocks decoded.
pub trait NodeReader {
    /// The location of node `id`, if it is stored.
    fn get(&mut self, id: u64) -> Option<Location>;
}

/// Bulk loads a new store from locations in ascending ID order.
pub trait NodeBuilder {
    type Store;

    /// Adds the location of node `id`, which must be above all IDs added before.
    fn push(&mut self, id: u64, loc: Location) -> OsmNodeCacheResult<()>;

    /// Writes the rest and commits the store with `stamp`.
    fn finish(self, stamp: u64) -> OsmNodeCacheResult<Self::Store>;
}

/// The same checks for every store.
#[cfg(all(test, any(feature = "native", feature = "redb")))]
mod tests {
    use std::collections::BTreeMap;
    use std::path::Path;

    use rand::rngs::StdRng;
    use rand::{RngExt as _, SeedableRng as _};

    use super::{NodeBuilder as _, NodeReader as _, NodeStore};
    use crate::traits::tests::TestFile;
    use crate::{Location, OsmNodeCacheError};

    /// IDs mostly in runs, with gaps of all sizes, and locations a short step apart.
    fn nodes(rng: &mut StdRng, count: usize) -> BTreeMap<u64, Location> {
        let (mut id, mut loc) = (1, Location::new(0, 0));
        (0..count)
            .map(|_| {
                id += match rng.random_range(0..100) {
                    0..80 => 1,
                    80..98 => rng.random_range(2..200),
                    _ => rng.random_range(200..100_000),
                };
                loc = Location::new(
                    loc.lat + rng.random_range(-2000..=2000),
                    loc.lon + rng.random_range(-2000..=2000),
                );
                (id, loc)
            })
            .collect()
    }

    fn build<S: NodeStore>(path: &Path, nodes: &BTreeMap<u64, Location>, stamp: u64) -> S {
        let mut b = S::builder(path, &S::Opts::default()).unwrap();
        for (&id, &loc) in nodes {
            b.push(id, loc).unwrap();
        }
        b.finish(stamp).unwrap()
    }

    /// Checks the stored nodes, and that their neighbors are not stored.
    fn check<S: NodeStore>(store: &S, expected: &BTreeMap<u64, Location>) {
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
        assert_eq!(store.get(u64::MAX >> 1), None);
    }

    fn round_trip<S: NodeStore>(name: &str) {
        let f = TestFile::new(name);
        let expected = nodes(&mut StdRng::seed_from_u64(1), 50_000);
        let store = build::<S>(&f.0, &expected, 7);
        assert_eq!(store.stamp(), 7);
        check(&store, &expected);
        drop(store);
        let store = S::open(&f.0, &S::Opts::default()).unwrap();
        assert_eq!(store.stamp(), 7);
        check(&store, &expected);
    }

    fn empty<S: NodeStore>(name: &str) {
        let f = TestFile::new(name);
        let store = build::<S>(&f.0, &BTreeMap::new(), 0);
        check(&store, &BTreeMap::new());
        drop(store);
        let mut store = S::open(&f.0, &S::Opts::default()).unwrap();
        store.set(5, Location::new(1, 2)).unwrap();
        store.commit(1).unwrap();
        check(&store, &BTreeMap::from([(5, Location::new(1, 2))]));
    }

    /// Moves, removes and adds nodes, as replication diffs do, checking before and after commits.
    fn changes_match_a_map<S: NodeStore>(name: &str) {
        let f = TestFile::new(name);
        let rng = &mut StdRng::seed_from_u64(2);
        let mut expected = nodes(rng, 30_000);
        let mut store = build::<S>(&f.0, &expected, 1);
        let ids: Vec<u64> = expected.keys().copied().collect();
        let mut next = ids[ids.len() - 1] + 1;
        for round in 2..6 {
            for _ in 0..3000 {
                let id = ids[rng.random_range(0..ids.len())];
                match rng.random_range(0..10) {
                    0..5 => {
                        let loc = Location::new(rng.random(), rng.random());
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
                        let loc = Location::new(rng.random(), rng.random());
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
        drop(store);
        let store = S::open(&f.0, &S::Opts::default()).unwrap();
        assert_eq!(store.stamp(), 5);
        check(&store, &expected);
    }

    fn rejects_unsorted_ids<S: NodeStore>(name: &str) {
        let f = TestFile::new(name);
        let mut b = S::builder(&f.0, &S::Opts::default()).unwrap();
        b.push(10, Location::new(1, 1)).unwrap();
        let err = b.push(10, Location::new(1, 1)).err().unwrap();
        assert!(matches!(
            err,
            OsmNodeCacheError::NotAscending { id: 10, last: 10 }
        ));
    }

    /// Changes not committed are gone after reopening.
    #[cfg(feature = "redb")]
    fn drops_uncommitted<S: NodeStore>(name: &str) {
        let f = TestFile::new(name);
        let expected = nodes(&mut StdRng::seed_from_u64(3), 10_000);
        let mut store = build::<S>(&f.0, &expected, 1);
        let (&first, _) = expected.first_key_value().unwrap();
        store.remove(first).unwrap();
        store
            .set(first + 1_000_000_000, Location::new(3, 4))
            .unwrap();
        drop(store);
        let store = S::open(&f.0, &S::Opts::default()).unwrap();
        assert_eq!(store.stamp(), 1);
        check(&store, &expected);
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

    #[cfg(feature = "native")]
    mod native {
        store_tests!(
            crate::PagedFileCache,
            round_trip,
            empty,
            changes_match_a_map,
            rejects_unsorted_ids
        );
    }

    #[cfg(feature = "redb")]
    mod redb {
        store_tests!(
            crate::KvNodeStore<crate::RedbBackend>,
            round_trip,
            empty,
            changes_match_a_map,
            rejects_unsorted_ids,
            drops_uncommitted
        );
    }
}
