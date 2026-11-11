//! The reverse index: the ways through each node, in a `redb` file beside the node file.
//!
//! Node IDs are grouped in blocks of [`BLOCK`]; a block's value lists its (node, way) pairs in order, delta-coded as
//! varints. Entries may be outdated, never missing: a way that lost a node may still be listed for it after a crash
//! between the database's commit and the index's, so every way it gives is checked against the way's node list.

use std::path::{Path, PathBuf};
use std::{fs, io};

use rayon::prelude::*;
use redb::{Database, Durability, ReadableDatabase as _, ReadableTable as _, TableDefinition};

use crate::SyncError;

const WAYS: TableDefinition<u64, &[u8]> = TableDefinition::new("ways");
const META: TableDefinition<&str, i64> = TableDefinition::new("meta");
/// Node IDs per block.
const BLOCK: u64 = 256;
/// Pairs written in one commit while creating an index, at least.
const CHUNK: usize = 1 << 22;

/// A (node ID, way ID) pair.
pub(crate) type Pair = (u64, u64);

/// The reverse index file.
pub(crate) struct ReverseIndex {
    db: Database,
    path: PathBuf,
}

impl ReverseIndex {
    /// Creates the index anew from `pairs`, sorted and without duplicates, with the database's identity and revision;
    /// returns it and the number of pairs. The pairs are written in chunks, each committed without waiting for the
    /// disk, and the identity only with the last, so an index cut short is never taken as made for the database.
    pub(crate) fn create(
        path: &Path,
        id: i64,
        rev: i64,
        pairs: impl Iterator<Item = Result<Pair, SyncError>>,
    ) -> Result<(Self, u64), SyncError> {
        remove(path)?;
        let at = |e: redb::Error| SyncError::index(path, e);
        let db = Database::create(path).map_err(|e| at(e.into()))?;
        let mut chunk: Vec<Pair> = Vec::with_capacity(CHUNK);
        let mut count = 0;
        for pair in pairs {
            let pair = pair?;
            if chunk.len() >= CHUNK && chunk.last().is_some_and(|l| l.0 / BLOCK != pair.0 / BLOCK) {
                write_blocks(&db, &chunk, None).map_err(at)?;
                chunk.clear();
            }
            chunk.push(pair);
            count += 1;
        }
        write_blocks(&db, &chunk, Some((id, rev))).map_err(at)?;
        let index = Self {
            db,
            path: path.to_path_buf(),
        };
        Ok((index, count))
    }

    /// Opens an existing index.
    pub(crate) fn open(path: &Path) -> Result<Self, SyncError> {
        if !path.exists() {
            return Err(SyncError::Missing(path.to_path_buf()));
        }
        let db = Database::open(path).map_err(|e| SyncError::index(path, e.into()))?;
        Ok(Self {
            db,
            path: path.to_path_buf(),
        })
    }

    /// The identity of the database the index was built for, and the revision it is up to.
    pub(crate) fn stamp(&self) -> Result<(i64, i64), SyncError> {
        (|| -> Result<_, redb::Error> {
            let txn = self.db.begin_read()?;
            let meta = txn.open_table(META)?;
            let get = |k| meta.get(k).map(|v| v.map_or(0, |v| v.value()));
            Ok((get("id")?, get("rev")?))
        })()
        .map_err(|e| SyncError::index(&self.path, e))
    }

    /// The (node, way) pairs of `nodes`, in any order and with repeats, sorted.
    pub(crate) fn ways_of(
        &self,
        nodes: impl IntoIterator<Item = u64>,
    ) -> Result<Vec<Pair>, SyncError> {
        let mut nodes: Vec<u64> = nodes.into_iter().collect();
        nodes.sort_unstable();
        nodes.dedup();
        (|| -> Result<_, redb::Error> {
            let txn = self.db.begin_read()?;
            let ways = txn.open_table(WAYS)?;
            let mut out = Vec::new();
            for nodes in nodes.chunk_by(|a, b| a / BLOCK == b / BLOCK) {
                if let Some(value) = ways.get(nodes[0] / BLOCK)? {
                    let pairs = decode(nodes[0] / BLOCK, value.value());
                    out.extend(
                        pairs
                            .into_iter()
                            .filter(|p| nodes.binary_search(&p.0).is_ok()),
                    );
                }
            }
            Ok(out)
        })()
        .map_err(|e| SyncError::index(&self.path, e))
    }

    /// Removes `remove` and adds `add`, both sorted, and records `rev`, in one commit.
    pub(crate) fn apply(&self, remove: &[Pair], add: &[Pair], rev: i64) -> Result<(), SyncError> {
        let mut blocks: Vec<u64> = remove.iter().chain(add).map(|p| p.0 / BLOCK).collect();
        blocks.sort_unstable();
        blocks.dedup();
        let in_block = |pairs: &[Pair], block: u64| {
            let start = pairs.partition_point(|p| p.0 < block * BLOCK);
            let end = pairs.partition_point(|p| p.0 < (block + 1) * BLOCK);
            start..end
        };
        (|| -> Result<(), redb::Error> {
            let txn = self.db.begin_write()?;
            {
                let mut ways = txn.open_table(WAYS)?;
                for block in blocks {
                    let mut pairs = ways
                        .get(block)?
                        .map(|v| decode(block, v.value()))
                        .unwrap_or_default();
                    let gone = &remove[in_block(remove, block)];
                    pairs.retain(|p| gone.binary_search(p).is_err());
                    pairs.extend_from_slice(&add[in_block(add, block)]);
                    pairs.sort_unstable();
                    pairs.dedup();
                    if pairs.is_empty() {
                        ways.remove(block)?;
                    } else {
                        ways.insert(block, encode(&pairs).as_slice())?;
                    }
                }
                txn.open_table(META)?.insert("rev", rev)?;
            }
            txn.commit()?;
            Ok(())
        })()
        .map_err(|e| SyncError::index(&self.path, e))
    }
}

/// Writes the blocks of `pairs`, which are whole blocks, encoded in parallel; with the stamp, durably.
fn write_blocks(
    db: &Database,
    pairs: &[Pair],
    stamp: Option<(i64, i64)>,
) -> Result<(), redb::Error> {
    let blocks: Vec<(u64, Vec<u8>)> = pairs
        .chunk_by(|a, b| a.0 / BLOCK == b.0 / BLOCK)
        .collect::<Vec<_>>()
        .into_par_iter()
        .map(|pairs| (pairs[0].0 / BLOCK, encode(pairs)))
        .collect();
    let mut txn = db.begin_write()?;
    if stamp.is_none() {
        txn.set_durability(Durability::None)?;
    }
    {
        let mut ways = txn.open_table(WAYS)?;
        for (block, value) in &blocks {
            ways.insert(block, value.as_slice())?;
        }
        if let Some((id, rev)) = stamp {
            let mut meta = txn.open_table(META)?;
            meta.insert("id", id)?;
            meta.insert("rev", rev)?;
        }
    }
    txn.commit()?;
    Ok(())
}

/// Removes the index file at `path`, if there is one.
pub(crate) fn remove(path: &Path) -> Result<(), SyncError> {
    match fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(SyncError::Io(e)),
        _ => Ok(()),
    }
}

/// A block's pairs: for each node, the step from the node before it (from the block's first ID for the first), the
/// number of its ways, and each way as the step from the one before it.
fn encode(pairs: &[Pair]) -> Vec<u8> {
    let mut out = Vec::with_capacity(pairs.len() * 4);
    let mut node = pairs[0].0 / BLOCK * BLOCK;
    for ways in pairs.chunk_by(|a, b| a.0 == b.0) {
        varint(&mut out, ways[0].0 - node);
        node = ways[0].0;
        varint(&mut out, ways.len() as u64);
        let mut way = 0;
        for &(_, w) in ways {
            varint(&mut out, w - way);
            way = w;
        }
    }
    out
}

/// The pairs of block `block`, from [`encode`].
fn decode(block: u64, mut b: &[u8]) -> Vec<Pair> {
    let mut out = Vec::new();
    let mut node = block * BLOCK;
    while !b.is_empty() {
        node += read_varint(&mut b);
        let mut way = 0;
        for _ in 0..read_varint(&mut b) {
            way += read_varint(&mut b);
            out.push((node, way));
        }
    }
    out
}

#[expect(clippy::cast_possible_truncation, reason = "the low 7 bits, by design")]
fn varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn read_varint(b: &mut &[u8]) -> u64 {
    let mut v = 0;
    let mut shift = 0;
    while let Some((&byte, rest)) = b.split_first() {
        *b = rest;
        v |= u64::from(byte & 0x7f) << shift;
        if byte < 0x80 {
            break;
        }
        shift += 7;
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_round_trip() {
        let pairs = [(512, 3), (512, 1_000_000_000), (513, 3), (767, 0)];
        assert_eq!(decode(2, &encode(&pairs)), pairs);
    }

    #[test]
    fn index_finds_adds_and_removes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ways.redb");
        let pairs = [(1, 10), (1, 11), (300, 10)];
        let (index, count) = ReverseIndex::create(&path, 7, 1, pairs.into_iter().map(Ok)).unwrap();
        assert_eq!(count, 3);
        assert_eq!(index.stamp().unwrap(), (7, 1));
        assert_eq!(index.ways_of([2, 1, 2]).unwrap(), [(1, 10), (1, 11)]);
        index.apply(&[(1, 10), (300, 10)], &[(2, 12)], 2).unwrap();
        drop(index);
        let index = ReverseIndex::open(&path).unwrap();
        assert_eq!(index.stamp().unwrap(), (7, 2));
        assert_eq!(index.ways_of([1, 2, 300]).unwrap(), [(1, 11), (2, 12)]);
    }
}
