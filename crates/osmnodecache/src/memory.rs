//! [`MemoryStore`]: node locations in a hash map in memory, with no file.

use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};

use crate::store::{check, check_order};
use crate::{
    BulkLoad, Error, LoadState, Location, Metadata, NodeRead, NodeReader, NodeStore, Result,
};

/// Hashes node IDs: the high bits of a multiplication spread nearby IDs over the table.
#[derive(Default)]
struct IdHasher(u64);

impl Hasher for IdHasher {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.write_u64(u64::from(b));
        }
    }

    #[expect(clippy::cast_possible_truncation)]
    fn write_u64(&mut self, id: u64) {
        let m = u128::from(id ^ self.0) * 0x9e37_79b9_7f4a_7c15;
        self.0 = (m as u64) ^ ((m >> 64) as u64);
    }
}

type Nodes = HashMap<u64, Location, BuildHasherDefault<IdHasher>>;

/// Node locations in a hash map in memory, with no file: it takes no disk space, and keeps nothing
/// once it is dropped. Changes take effect when they are made, and a commit only records its stamp.
///
/// The fastest store to change, and the simplest for a small extract, which loads from its PBF file
/// in a second or two. It takes the most memory, about 20 to 39 bytes per node depending on how full
/// its hash map is ([`MemoryStore::memory_bytes`]): 2 to 4 GB for 100 million nodes.
#[derive(Default)]
pub struct MemoryStore {
    nodes: Nodes,
    stamp: u64,
    metadata: Metadata,
}

/// Reads locations of a [`MemoryStore`].
pub struct MemoryReader<'a>(&'a MemoryStore);

impl MemoryStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// An empty store with room for `nodes` nodes, so that loading them does not grow it.
    #[must_use]
    pub fn with_capacity(nodes: usize) -> Self {
        Self {
            nodes: Nodes::with_capacity_and_hasher(nodes, BuildHasherDefault::default()),
            ..Self::default()
        }
    }

    /// The number of nodes stored.
    #[must_use]
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// About the bytes of memory the nodes take: the hash map has 16 bytes and a control byte for
    /// each of its slots, and fills at most 7 in 8 of them.
    #[must_use]
    pub fn memory_bytes(&self) -> usize {
        self.nodes.capacity() / 7 * 8 * (size_of::<(u64, Location)>() + 1)
    }
}

impl NodeRead for MemoryStore {
    type Reader<'a> = MemoryReader<'a>;

    fn reader(&self) -> MemoryReader<'_> {
        MemoryReader(self)
    }

    #[inline]
    fn get(&self, id: u64) -> Option<Location> {
        self.nodes.get(&id).copied()
    }

    fn stamp(&self) -> u64 {
        self.stamp
    }

    /// Nothing is kept of a load that did not finish, so the store is always complete.
    fn load_state(&self) -> LoadState {
        LoadState::Complete
    }

    fn metadata(&self) -> &Metadata {
        &self.metadata
    }

    fn for_each(&self, mut f: impl FnMut(u64, Location)) -> Result<()> {
        let mut ids: Vec<u64> = self.nodes.keys().copied().collect();
        ids.sort_unstable();
        for id in ids {
            f(id, self.nodes[&id]);
        }
        Ok(())
    }
}

impl NodeReader for MemoryReader<'_> {
    #[inline]
    fn get(&mut self, id: u64) -> Option<Location> {
        self.0.nodes.get(&id).copied()
    }
}

impl NodeStore for MemoryStore {
    type Loader<'a> = MemoryLoader<'a>;

    fn set(&mut self, id: u64, loc: Location) -> Result<()> {
        check(id, loc)?;
        self.nodes.insert(id, loc);
        Ok(())
    }

    fn remove(&mut self, id: u64) -> Result<()> {
        check(id, Location::default())?;
        self.nodes.remove(&id);
        Ok(())
    }

    fn set_metadata(&mut self, key: &str, value: &[u8]) -> Result<()> {
        self.metadata.set(key, value)
    }

    fn remove_metadata(&mut self, key: &str) {
        self.metadata.remove(key);
    }

    /// Records `stamp`; the changes took effect when they were made.
    fn commit(&mut self, stamp: u64) -> Result<()> {
        self.stamp = stamp;
        Ok(())
    }

    fn bulk_load(&mut self) -> Result<MemoryLoader<'_>> {
        if !self.nodes.is_empty() {
            return Err(Error::NotEmpty);
        }
        Ok(MemoryLoader {
            store: self,
            last_id: None,
            finished: false,
        })
    }
}

/// Bulk loads a [`MemoryStore`]; see [`BulkLoad`]. Dropping it before it finishes leaves the store
/// empty, so an interrupted load starts over.
pub struct MemoryLoader<'a> {
    store: &'a mut MemoryStore,
    last_id: Option<u64>,
    finished: bool,
}

impl BulkLoad for MemoryLoader<'_> {
    fn resume_after(&self) -> Option<u64> {
        None
    }

    fn push(&mut self, id: u64, loc: Location) -> Result<()> {
        check(id, loc)?;
        check_order(self.last_id, id)?;
        self.last_id = Some(id);
        self.store.nodes.insert(id, loc);
        Ok(())
    }

    fn push_batch(&mut self, nodes: &[(u64, Location)]) -> Result<()> {
        self.store.nodes.reserve(nodes.len());
        for &(id, loc) in nodes {
            self.push(id, loc)?;
        }
        Ok(())
    }

    fn finish(mut self, stamp: u64) -> Result<()> {
        self.store.stamp = stamp;
        self.finished = true;
        Ok(())
    }
}

impl Drop for MemoryLoader<'_> {
    /// Forgets the nodes of an unfinished load, which started with an empty store.
    fn drop(&mut self) {
        if !self.finished {
            self.store.nodes.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{BulkLoad as _, LoadState, Location, MemoryStore, NodeRead as _, NodeStore as _};

    #[test]
    fn forgets_an_unfinished_load() {
        let mut store = MemoryStore::new();
        let mut load = store.bulk_load().unwrap();
        load.push(5, Location::new(1, 2)).unwrap();
        drop(load);
        assert!(store.is_empty());
        assert_eq!(store.load_state(), LoadState::Complete);
        let mut load = store.bulk_load().unwrap();
        assert_eq!(load.resume_after(), None);
        load.push_batch(&[(5, Location::new(1, 2)), (9, Location::new(3, 4))])
            .unwrap();
        load.finish(2).unwrap();
        assert_eq!(store.len(), 2);
        assert_eq!(store.get(9), Some(Location::new(3, 4)));
        assert_eq!(store.stamp(), 2);
    }

    #[test]
    fn memory_bytes() {
        let mut store = MemoryStore::with_capacity(1000);
        assert!((1000 * 17..=2300 * 17).contains(&store.memory_bytes()));
        for id in 0..100_000 {
            store.set(id, Location::new(1, 1)).unwrap();
        }
        let bytes = store.memory_bytes();
        assert!(
            (19 * 100_000..=39 * 100_000).contains(&bytes),
            "{bytes} bytes"
        );
    }
}
