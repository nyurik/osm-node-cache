//! Rebuilding the node file and the reverse index, up to the database's revision, which is only read.

use std::path::Path;
use std::time::Instant;
use std::{fs, iter};

use osmflow::nodes::PagedStore;
use osmflow::nodes::prelude::*;
use osmflow::osc::Replication;
use osmflow::{Location, PbfReader};
use postgres::Client;
use postgres::fallible_iterator::FallibleIterator as _;
use tracing::info;

use crate::copy::wkb_locations;
use crate::db::{self, DbState};
use crate::files::{ID_KEY, read_diffs, remove_node_file, set_locations, way_pairs};
use crate::index::ReverseIndex;
use crate::sort::{Sorted, Sorter};
use crate::update::check_stream;
use crate::{Mirror, SyncError};

/// Where [`Mirror::rebuild`] takes node locations from. The reverse index is always made from the database's ways.
#[derive(Clone, Copy)]
pub enum Rebuild<'a> {
    /// The database: the points of the ways' geometries, and the tagged nodes. Nodes in no way with a geometry
    /// and without tags are not in the database, so they are missing from the new file; they matter only if a later
    /// edit adds them to a way.
    Database,
    /// A PBF file, then the diffs after it up to the database's sequence: every node, as an import has it.
    Pbf {
        /// The file, which must be sorted by type and ID.
        path: &'a Path,
        /// The stream's sequence the file is up to, instead of the one its header gives.
        sequence: Option<u64>,
        /// The database's replication stream, which has the diffs.
        replication: &'a Replication,
        /// The most diffs applied in one commit.
        max_diffs: u64,
    },
}

/// What a rebuild made.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RebuildStats {
    /// Nodes in the node file after loading, before any diffs.
    pub nodes: u64,
    /// Diffs applied to the node file.
    pub diffs: u64,
    /// (node, way) pairs in the reverse index.
    pub pairs: u64,
}

pub(crate) fn rebuild(m: &Mirror, from: &Rebuild<'_>) -> Result<RebuildStats, SyncError> {
    let mut client = db::connect(&m.url)?;
    let state = DbState::read(&mut client)?;
    let mut made = RebuildStats::default();

    let t = Instant::now();
    let path = m.node_path();
    fs::create_dir_all(&m.dir)?;
    remove_node_file(&path)?;
    let mut store = PagedStore::open(&path)?;
    match *from {
        Rebuild::Database => {
            let mut load = store.bulk_load()?;
            let mut batch = Vec::with_capacity(1 << 20);
            let mut last = None;
            for node in db_locations(&mut client, &m.dir)? {
                // A node of several ways has the same location in each; one is enough
                let (id, loc) = node?;
                if last != Some(id) {
                    last = Some(id);
                    batch.push((id, unpack(loc)));
                }
                if batch.len() == batch.capacity() {
                    load.push_batch(&batch)?;
                    made.nodes += batch.len() as u64;
                    batch.clear();
                }
            }
            load.push_batch(&batch)?;
            made.nodes += batch.len() as u64;
            load.finish(0)?;
        }
        Rebuild::Pbf {
            path,
            sequence,
            replication,
            max_diffs,
        } => {
            check_stream(&state, replication)?;
            let reader = PbfReader::open(path)?;
            let from =
                sequence.or_else(|| reader.header().replication_sequence.map(i64::cast_unsigned));
            let Some(from) = from.filter(|&s| s <= state.sequence.cast_unsigned()) else {
                return Err(SyncError::state(format!(
                    "{}: give the sequence of the file, which must be at or before the database's {}",
                    path.display(),
                    state.sequence
                )));
            };
            made.nodes = reader.load_nodes(&mut store)?;
            info!(
                nodes = made.nodes,
                secs = t.elapsed().as_secs_f32(),
                "loaded the node file"
            );
            let end = state.sequence.cast_unsigned();
            let mut next = from + 1;
            while next <= end {
                let last = end.min(next + max_diffs.max(1) - 1);
                let (changes, _) = read_diffs(replication, next..=last)?;
                set_locations(&mut store, &changes)?;
                // Without the database's identity until the end, so a rebuild cut short is never taken as done
                store.commit(0)?;
                made.diffs += last + 1 - next;
                next = last + 1;
            }
        }
    }
    store.set_metadata(ID_KEY, &state.id.to_le_bytes())?;
    store.commit(state.rev.cast_unsigned())?;
    info!(
        nodes = made.nodes,
        diffs = made.diffs,
        secs = t.elapsed().as_secs_f32(),
        "rebuilt the node file"
    );

    let t = Instant::now();
    let pairs = db_pairs(&mut client, &m.dir)?.map(|p| p.map_err(SyncError::from));
    made.pairs = ReverseIndex::create(&m.index_path(), state.id, state.rev, pairs)?.1;
    info!(
        pairs = made.pairs,
        secs = t.elapsed().as_secs_f32(),
        "rebuilt the reverse index"
    );
    Ok(made)
}

/// The node locations the database has, as (ID, packed location), sorted, sorted with temporary files in `dir`.
fn db_locations(client: &mut Client, dir: &Path) -> Result<Sorted, SyncError> {
    let sorter = Sorter::new(dir)?;
    // The ways' points, then the tagged nodes': (node IDs, WKB) rows
    for sql in [
        "SELECT nodes, ST_AsBinary(geom) FROM osm.ways WHERE geom IS NOT NULL",
        "SELECT ARRAY[id], ST_AsBinary(geom) FROM osm.nodes WHERE geom IS NOT NULL",
    ] {
        let mut rows = client.query_raw(sql, iter::empty::<&str>())?;
        while let Some(row) = rows.next()? {
            let nodes: Vec<i64> = row.get(0);
            let points = wkb_locations(row.get(1));
            sorter.extend(
                nodes
                    .into_iter()
                    .zip(points)
                    .filter_map(|(id, l)| Some((u64::try_from(id).ok()?, pack(l)))),
            )?;
        }
    }
    Ok(sorter.finish()?)
}

/// The reverse index's pairs, from the ways' node lists, sorted with temporary files in `dir`.
fn db_pairs(client: &mut Client, dir: &Path) -> Result<Sorted, SyncError> {
    let sorter = Sorter::new(dir)?;
    let mut rows = client.query_raw("SELECT id, nodes FROM osm.ways", iter::empty::<&str>())?;
    while let Some(row) = rows.next()? {
        sorter.extend(way_pairs(row.get(0), &row.get::<_, Vec<i64>>(1)))?;
    }
    Ok(sorter.finish()?)
}

/// A location as one number to sort beside its node ID.
fn pack(l: Location) -> u64 {
    (u64::from(l.lat.cast_unsigned()) << 32) | u64::from(l.lon.cast_unsigned())
}

fn unpack(v: u64) -> Location {
    let [lat, lon] =
        [v >> 32, v & 0xffff_ffff].map(|h| u32::try_from(h).unwrap_or_default().cast_signed());
    Location::new(lat, lon)
}
