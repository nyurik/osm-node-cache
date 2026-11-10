//! The two files beside the database: the node file, a `PagedStore`, and the reverse index. Both follow the
//! database: it commits first, then they do, with its revision as their stamp, so a file is never ahead of it. A file
//! behind it catches up when it is opened: the node file from the cached diffs, the index from the database's rows.

use std::ops::RangeInclusive;
use std::path::Path;
use std::{fs, io};

use osmflow::nodes::PagedStore;
use osmflow::nodes::prelude::*;
use osmflow::osc::{Replication, State};
use postgres::Client;
use tracing::info;

use crate::db::DbState;
use crate::index::{Pair, ReverseIndex};
use crate::update::Changes;
use crate::{Mirror, SyncError};

/// The node file's metadata entry with the database's identity.
pub(crate) const ID_KEY: &str = "osmflow-sync.id";

/// The identity of the database a node file was built for, if it was finished.
pub(crate) fn node_file_id(store: &impl NodeRead) -> Option<i64> {
    let bytes = store.metadata().get(ID_KEY)?;
    Some(i64::from_le_bytes(bytes.try_into().ok()?))
}

/// Removes a node file, with the files kept beside it.
pub(crate) fn remove_node_file(path: &Path) -> Result<(), SyncError> {
    for ext in ["", ".dir", ".undo"] {
        let mut p = path.as_os_str().to_owned();
        p.push(ext);
        match fs::remove_file(p) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e.into()),
            _ => {}
        }
    }
    Ok(())
}

/// Checks that a file with identity `id` and revision `rev` belongs to the database and is not ahead of it.
fn check(path: &Path, id: Option<i64>, rev: i64, state: &DbState) -> Result<(), SyncError> {
    let path = path.display();
    if id != Some(state.id) {
        return Err(SyncError::state(format!(
            "{path} was not made for this database, or its rebuild was cut short: rebuild it"
        )));
    }
    if rev > state.rev {
        return Err(SyncError::state(format!(
            "{path} is at revision {rev}, ahead of the database's {}, as after restoring the database from a \
             backup: rebuild it",
            state.rev
        )));
    }
    Ok(())
}

/// Sets the node locations of `changes` in `store`, and returns the IDs of the nodes whose location changed, moved,
/// added or deleted, in order.
pub(crate) fn set_locations(
    store: &mut impl NodeStore,
    changes: &Changes,
) -> Result<Vec<u64>, SyncError> {
    let mut moved = Vec::new();
    {
        let mut reader = store.reader();
        for (id, loc) in changes.locations() {
            if reader.get(id) != loc {
                moved.push((id, loc));
            }
        }
    }
    for &(id, loc) in &moved {
        match loc {
            Some(loc) => store.set(id, loc)?,
            None => store.remove(id)?,
        }
    }
    Ok(moved.into_iter().map(|(id, _)| id).collect())
}

/// Reads the diffs of `sequences`, merged.
pub(crate) fn read_diffs(
    replication: &Replication,
    sequences: RangeInclusive<u64>,
) -> Result<(Changes, Option<State>), SyncError> {
    let mut changes = Changes::default();
    let mut last = None;
    for diff in replication.diffs(sequences)? {
        let diff = diff?;
        changes.add(diff.changes);
        last = Some(diff.state);
    }
    Ok((changes, last))
}

impl Mirror {
    /// Opens the node file, and brings it up to the database from the cached diffs if it is behind.
    pub(crate) fn open_node_file(
        &self,
        client: &mut Client,
        state: &DbState,
        replication: &Replication,
    ) -> Result<PagedStore, SyncError> {
        let path = self.node_path();
        if !path.exists() {
            return Err(SyncError::Missing(path));
        }
        let mut store = PagedStore::open(&path)?;
        let rev = store.stamp().cast_signed();
        check(&path, node_file_id(&store), rev, state)?;
        if rev < state.rev {
            let batches = client.query(
                "SELECT rev, first_sequence, last_sequence FROM osm.batches WHERE rev > $1 ORDER BY rev",
                &[&rev],
            )?;
            for b in batches {
                let (rev, first, last): (i64, i64, i64) = (b.get(0), b.get(1), b.get(2));
                let (changes, _) =
                    read_diffs(replication, first.cast_unsigned()..=last.cast_unsigned())?;
                set_locations(&mut store, &changes)?;
                store.commit(rev.cast_unsigned())?;
            }
            info!(
                from = rev,
                to = state.rev,
                "brought the node file up to the database"
            );
        }
        Ok(store)
    }

    /// Opens the reverse index, and adds the ways the database changed since its revision if it is behind.
    pub(crate) fn open_index(
        &self,
        client: &mut Client,
        state: &DbState,
    ) -> Result<ReverseIndex, SyncError> {
        let path = self.index_path();
        let index = ReverseIndex::open(&path)?;
        let (id, rev) = index.stamp()?;
        check(&path, Some(id), rev, state)?;
        if rev < state.rev {
            let rows = client.query(
                "SELECT id, nodes FROM osm.ways WHERE id IN \
                 (SELECT id FROM osm.changes WHERE kind = 'w' AND action = 'u' AND rev > $1)",
                &[&rev],
            )?;
            let mut add: Vec<Pair> = Vec::new();
            for row in rows {
                add.extend(way_pairs(row.get(0), &row.get::<_, Vec<i64>>(1)));
            }
            add.sort_unstable();
            add.dedup();
            index.apply(&[], &add, state.rev)?;
            info!(
                from = rev,
                to = state.rev,
                ways = add.len(),
                "brought the reverse index up to the database"
            );
        }
        Ok(index)
    }
}

/// The (node, way) pairs of way `id`; node IDs below 0, which only editors' files have, are left out.
pub(crate) fn way_pairs(id: i64, nodes: &[i64]) -> impl Iterator<Item = Pair> + '_ {
    nodes
        .iter()
        .filter_map(move |&n| Some((u64::try_from(n).ok()?, id.cast_unsigned())))
}
