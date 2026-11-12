//! Updates: replication diffs applied to the database and its files, a batch of diffs per transaction.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::time::Instant;

use osmflow::nodes::PagedStore;
use osmflow::nodes::prelude::*;
use osmflow::osc::{self, Action, Change, Object, Replication, StateSource as _};
use osmflow::{Location, Timestamp};
use postgres::{Client, Transaction};
use tracing::{debug, info, warn};

use crate::copy::Rows;
use crate::db::{self, DbState, Table};
use crate::files::{read_diffs, set_locations, way_pairs};
use crate::index::{Pair, ReverseIndex};
use crate::{GeometryFormat, OpenMirror, SyncError};

/// How to update.
#[derive(Debug, Clone)]
pub struct UpdateOpts {
    /// The most diffs applied in one transaction, 60 by default. Merged, they cost far less than one at a time, as an
    /// object changed in several is written once.
    pub max_diffs: u64,
    /// The last sequence to apply, instead of the stream's latest.
    pub until: Option<u64>,
}

impl Default for UpdateOpts {
    fn default() -> Self {
        Self {
            max_diffs: 60,
            until: None,
        }
    }
}

/// What an update applied.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UpdateStats {
    /// Transactions, each a new revision.
    pub batches: u64,
    /// Diffs read.
    pub diffs: u64,
    /// Rows of `osm.nodes`, `osm.ways` and `osm.relations` written or deleted.
    pub rows: u64,
    /// Ways whose geometry changed only because their nodes did.
    pub moved_ways: u64,
    /// The sequence the database is now at.
    pub sequence: i64,
}

/// The changes of one or more diffs: of each object, its last version, or that it was deleted.
#[derive(Debug, Default)]
pub(crate) struct Changes {
    nodes: BTreeMap<i64, (bool, osc::Node)>,
    ways: BTreeMap<i64, (bool, osc::Way)>,
    relations: BTreeMap<i64, (bool, osc::Relation)>,
}

impl Changes {
    pub(crate) fn add(&mut self, changes: Vec<Change>) {
        for c in changes {
            let deleted = c.action == Action::Delete;
            match c.object {
                Object::Node(n) => self.nodes.insert(n.id, (deleted, n)).map(drop),
                Object::Way(w) => self.ways.insert(w.id, (deleted, w)).map(drop),
                Object::Relation(r) => self.relations.insert(r.id, (deleted, r)).map(drop),
            };
        }
    }

    /// Each changed node's new location, or none if it was deleted, in ID order.
    pub(crate) fn locations(&self) -> impl Iterator<Item = (u64, Option<Location>)> + '_ {
        self.nodes.iter().filter_map(|(&id, (deleted, n))| {
            Some((
                u64::try_from(id).ok()?,
                if *deleted { None } else { n.location },
            ))
        })
    }

    fn len(&self) -> usize {
        self.nodes.len() + self.ways.len() + self.relations.len()
    }
}

impl OpenMirror {
    /// Applies the diffs of `replication` after the database's sequence, up to the stream's latest or
    /// [`UpdateOpts::until`]. The files are checked against the database first, and brought up to it if a crash or an
    /// earlier error left them behind; a file of another database or ahead of it is an error, as on
    /// [`Mirror::open`](crate::Mirror::open).
    pub fn update(
        &mut self,
        replication: &Replication,
        opts: &UpdateOpts,
    ) -> Result<UpdateStats, SyncError> {
        let mut state = DbState::read(&mut self.client)?;
        check_stream(&state, replication)?;
        self.sync_index(&state)?;
        // Out of the mirror until every batch succeeds: a failed batch can leave locations in the node file that its
        // database did not commit, so the file is opened again on the next update
        let mut store = self.take_node_file(&state, replication)?;
        if !self.temp_tables {
            create_temp_tables(&mut self.client)?;
            self.temp_tables = true;
        }
        let total = apply_diffs(
            &mut self.client,
            &mut store,
            &self.index,
            &mut state,
            replication,
            opts,
        )?;
        self.store = Some(store);
        Ok(total)
    }
}

/// Applies the diffs after `state`'s sequence in batches, moving `state` along.
fn apply_diffs(
    client: &mut Client,
    store: &mut PagedStore,
    index: &ReverseIndex,
    state: &mut DbState,
    replication: &Replication,
    opts: &UpdateOpts,
) -> Result<UpdateStats, SyncError> {
    let latest = match opts.until {
        Some(until) => until,
        None => replication.state()?.sequence,
    };
    let mut total = UpdateStats {
        sequence: state.sequence,
        ..UpdateStats::default()
    };
    let mut next = state.sequence.cast_unsigned() + 1;
    while next <= latest {
        let t = Instant::now();
        let last = latest.min(next + opts.max_diffs.max(1) - 1);
        let (changes, last_state) = read_diffs(replication, next..=last)?;
        let read = t.elapsed();
        let batch = Batch {
            rev: state.rev + 1,
            first: next.cast_signed(),
            last: last.cast_signed(),
            timestamp: last_state.map(|s| s.timestamp),
            geometry: state.geometry,
        };
        let applied = apply(client, store, index, &changes, &batch)?;
        info!(
            rev = batch.rev,
            sequences = format!("{next}..={last}"),
            changes = changes.len(),
            rows = applied.rows,
            moved_ways = applied.moved_ways,
            read_secs = read.as_secs_f32(),
            secs = t.elapsed().as_secs_f32(),
            "applied a batch"
        );
        state.rev = batch.rev;
        total.batches += 1;
        total.diffs += last + 1 - next;
        total.rows += applied.rows;
        total.moved_ways += applied.moved_ways;
        total.sequence = batch.last;
        next = last + 1;
    }
    Ok(total)
}

/// Checks that `replication` is the database's stream: its diff at the database's sequence has the database's time.
/// A stream without that diff's state, such as a directory of only the diffs after it, cannot be checked: a warning,
/// unless the stream ends before it.
pub(crate) fn check_stream(state: &DbState, replication: &Replication) -> Result<(), SyncError> {
    let Some(time) = state.timestamp else {
        return Ok(());
    };
    let sequence = state.sequence.cast_unsigned();
    let Some(theirs) = replication.get(sequence)? else {
        let latest = replication.state()?.sequence;
        if latest < sequence {
            return Err(SyncError::state(format!(
                "the replication stream ends at diff {latest}, before the database's {sequence}: it is not the \
                 database's stream, {}",
                state.url.as_deref().unwrap_or("which it does not name")
            )));
        }
        warn!(
            "the replication stream has no state of diff {sequence}, the database's, so it cannot be checked to be \
             the database's stream"
        );
        return Ok(());
    };
    if theirs.timestamp == time {
        Ok(())
    } else {
        Err(SyncError::state(format!(
            "the replication stream's diff {sequence} is at {}, but the database's is at {time}: the stream is \
             not the database's, {}",
            theirs.timestamp,
            state.url.as_deref().unwrap_or("which it does not name")
        )))
    }
}

/// One transaction's place in the stream.
struct Batch {
    rev: i64,
    first: i64,
    last: i64,
    timestamp: Option<Timestamp>,
    geometry: GeometryFormat,
}

/// Rows to write into a table, and IDs to delete from it.
#[derive(Default)]
struct Writes {
    rows: Rows,
    written: Vec<i64>,
    deleted: Vec<i64>,
}

/// What a batch writes into the database, and changes in the reverse index.
#[derive(Default)]
struct Plan {
    nodes: Writes,
    ways: Writes,
    relations: Writes,
    /// The new geometries of the ways whose nodes moved but which did not change themselves.
    geoms: Rows,
    moved_ways: Vec<i64>,
    remove: Vec<Pair>,
    add: Vec<Pair>,
}

/// Applies `changes` as revision `batch.rev`: the database in one transaction, then the node file and the index.
fn apply(
    client: &mut Client,
    store: &mut PagedStore,
    index: &ReverseIndex,
    changes: &Changes,
    batch: &Batch,
) -> Result<UpdateStats, SyncError> {
    let t = Instant::now();
    let moved = set_locations(store, changes)?;
    let through = index.ways_of(moved.iter().copied())?;
    // The node lists of the changed ways, and of the ways the index says pass through a moved node
    let mut ids: Vec<i64> = changes
        .ways
        .keys()
        .copied()
        .chain(through.iter().map(|p| p.1.cast_signed()))
        .collect();
    ids.sort_unstable();
    ids.dedup();
    let old: HashMap<i64, Vec<i64>> = client
        .query("SELECT id, nodes FROM osm.ways WHERE id = ANY($1)", &[&ids])?
        .into_iter()
        .map(|r| (r.get(0), r.get(1)))
        .collect();
    let read = t.elapsed();
    let plan = plan(store, changes, &through, &old, batch);
    let planned = t.elapsed();
    let rows = write(client, changes, &plan, batch)?;
    let written = t.elapsed();
    // The database is committed; the files follow it, and catch up on the next start if a crash comes first
    store.commit(batch.rev.cast_unsigned())?;
    index.apply(&plan.remove, &plan.add, batch.rev)?;
    debug!(
        moved_nodes = moved.len(),
        read_secs = read.as_secs_f32(),
        encode_secs = planned.saturating_sub(read).as_secs_f32(),
        db_secs = written.saturating_sub(planned).as_secs_f32(),
        files_secs = t.elapsed().saturating_sub(written).as_secs_f32(),
        "batch steps"
    );
    Ok(UpdateStats {
        rows,
        moved_ways: plan.moved_ways.len() as u64,
        ..UpdateStats::default()
    })
}

/// Encodes the rows of `changes`, given the ways the index found through the moved nodes and the old node lists of
/// those and of the changed ways.
fn plan(
    store: &PagedStore,
    changes: &Changes,
    through: &[Pair],
    old: &HashMap<i64, Vec<i64>>,
    batch: &Batch,
) -> Plan {
    let (rev, format) = (batch.rev, batch.geometry);
    let mut plan = Plan::default();
    plan.nodes.rows = Rows::new(format);
    plan.ways.rows = Rows::new(format);
    plan.geoms = Rows::new(format);
    for (&id, (deleted, w)) in &changes.ways {
        if let Some(nodes) = old.get(&id) {
            plan.remove.extend(way_pairs(id, nodes));
        }
        if !deleted {
            plan.add.extend(way_pairs(id, &w.refs));
        }
    }
    // Ways through a moved node that did not change themselves; an outdated entry of the index is dropped
    let mut moved_ways = BTreeSet::new();
    for &(node, way) in through {
        let id = way.cast_signed();
        match old.get(&id) {
            _ if changes.ways.contains_key(&id) => {}
            Some(nodes) if nodes.contains(&node.cast_signed()) => {
                moved_ways.insert(id);
            }
            _ => plan.remove.push((node, way)),
        }
    }
    for pairs in [&mut plan.remove, &mut plan.add] {
        pairs.sort_unstable();
        pairs.dedup();
    }

    let mut reader = store.reader();
    let mut locs = Vec::new();
    for (&id, (deleted, n)) in &changes.nodes {
        if *deleted || n.tags.is_empty() {
            plan.nodes.deleted.push(id);
        } else {
            plan.nodes.written.push(id);
            plan.nodes
                .rows
                .node(id, (&n.meta).into(), rev, n.tags.iter(), n.location);
        }
    }
    for (&id, (deleted, w)) in &changes.ways {
        if *deleted {
            plan.ways.deleted.push(id);
        } else {
            plan.ways.written.push(id);
            let complete = locate(&mut reader, &w.refs, &mut locs);
            let line = complete.then_some(&locs[..]);
            let nodes = w.refs.iter().copied();
            plan.ways
                .rows
                .way(id, (&w.meta).into(), rev, w.tags.iter(), nodes, line);
        }
    }
    for &id in &moved_ways {
        let complete = locate(&mut reader, &old[&id], &mut locs);
        plan.geoms
            .row(2)
            .i64(id)
            .line(complete.then_some(&locs[..]));
    }
    plan.moved_ways = moved_ways.into_iter().collect();
    for (&id, (deleted, r)) in &changes.relations {
        if *deleted {
            plan.relations.deleted.push(id);
        } else {
            plan.relations.written.push(id);
            plan.relations
                .rows
                .relation(id, (&r.meta).into(), rev, r.tags.iter(), r.members());
        }
    }
    plan
}

/// Writes `plan` into the database in one transaction, with the change log and the new state; returns the rows
/// written and deleted.
fn write(
    client: &mut Client,
    changes: &Changes,
    plan: &Plan,
    batch: &Batch,
) -> Result<u64, SyncError> {
    let rev = batch.rev;
    let mut tx = client.transaction()?;
    let mut log = Rows::default();
    let mut rows = 0;
    for (table, kind, w) in [
        (Table::Nodes, b'n', &plan.nodes),
        (Table::Ways, b'w', &plan.ways),
        (Table::Relations, b'r', &plan.relations),
    ] {
        let gone = if w.deleted.is_empty() {
            Vec::new()
        } else {
            tx.query(
                &format!("DELETE FROM {table} WHERE id = ANY($1) RETURNING id"),
                &[&w.deleted],
            )?
        };
        for row in &gone {
            log.row(4).i64(rev).char(kind).i64(row.get(0)).char(b'd');
        }
        upsert(&mut tx, table, &w.rows)?;
        for &id in &w.written {
            log.row(4).i64(rev).char(kind).i64(id).char(b'u');
        }
        rows += gone.len() as u64 + w.rows.count;
    }
    if plan.geoms.count > 0 {
        db::copy(&mut tx, "osm_geoms", "id, geom", &plan.geoms)?;
        tx.execute(
            "UPDATE osm.ways w SET geom = g.geom, minor = w.minor + 1, rev = $1 FROM osm_geoms g WHERE w.id = g.id",
            &[&rev],
        )?;
        for &id in &plan.moved_ways {
            log.row(4).i64(rev).char(b'w').i64(id).char(b'g');
        }
    }
    if log.count > 0 {
        db::copy(&mut tx, "osm.changes", "rev, kind, id, action", &log)?;
    }
    let timestamp = batch.timestamp.map(Timestamp::as_millis);
    let count = |n: usize| i64::try_from(n).unwrap_or(i64::MAX);
    tx.execute(
        "INSERT INTO osm.batches (rev, first_sequence, last_sequence, timestamp, nodes, ways, relations) \
         VALUES ($1, $2, $3, to_timestamp($4::int8 / 1000.0), $5, $6, $7)",
        &[
            &rev,
            &batch.first,
            &batch.last,
            &timestamp,
            &count(changes.nodes.len()),
            &count(changes.ways.len()),
            &count(changes.relations.len()),
        ],
    )?;
    tx.execute(
        "UPDATE osm.state SET rev = $1, sequence = $2, timestamp = to_timestamp($3::int8 / 1000.0)",
        &[&rev, &batch.last, &timestamp],
    )?;
    tx.commit()?;
    Ok(rows)
}

/// Creates the session's temporary tables that each batch copies its rows into, emptied at every commit.
fn create_temp_tables(client: &mut Client) -> Result<(), SyncError> {
    let tables = [Table::Nodes, Table::Ways, Table::Relations].map(|t| {
        format!(
            "CREATE TEMP TABLE {} (LIKE {t}) ON COMMIT DELETE ROWS;",
            temp_name(t)
        )
    });
    client.batch_execute(&format!(
        "CREATE TEMP TABLE osm_geoms ON COMMIT DELETE ROWS AS SELECT id, geom FROM osm.ways WITH NO DATA; {}",
        tables.concat()
    ))?;
    Ok(())
}

/// The temporary table rows of `table` are copied into.
fn temp_name(table: Table) -> String {
    <&str>::from(table).replace('.', "_")
}

/// Writes `rows` of `table` over the rows with their IDs, or as new rows: copied into its temporary table, then
/// merged.
fn upsert(tx: &mut Transaction<'_>, table: Table, rows: &Rows) -> Result<(), SyncError> {
    if rows.count == 0 {
        return Ok(());
    }
    let (list, tmp) = (table.columns(), temp_name(table));
    // Every column but `id`, set to the new row's
    let rest = list.strip_prefix("id, ").unwrap_or(list);
    let new = rest.replace(", ", ", EXCLUDED.");
    db::copy(tx, &tmp, list, rows)?;
    tx.execute(&format!("INSERT INTO {table} ({list}) SELECT {list} FROM {tmp} ON CONFLICT (id) DO UPDATE SET ({rest}) = ROW(EXCLUDED.{new})"), &[])?;
    Ok(())
}

/// Puts the locations of `refs` in `locs`; false if one is missing.
fn locate(reader: &mut impl NodeReader, refs: &[i64], locs: &mut Vec<Location>) -> bool {
    locs.clear();
    refs.iter().all(|&r| {
        u64::try_from(r)
            .ok()
            .and_then(|r| reader.get(r))
            .map(|l| locs.push(l))
            .is_some()
    })
}
