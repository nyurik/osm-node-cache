//! The import: a PBF file into the `osm` schema, the node file and the reverse index.

use std::hash::{BuildHasher as _, Hasher as _, RandomState};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Instant, SystemTime};
use std::{fs, io, thread};

use osmflow::nodes::PagedStore;
use osmflow::nodes::prelude::*;
use osmflow::osc::{COMMIT_DELAY, Replication, Start, StateSource, plan};
use osmflow::{Location, PbfError, PbfReader, Select, Timestamp};
use postgres::Client;
use tracing::{debug, info, warn};

use crate::copy::Rows;
use crate::db::{self, DbState, SCHEMA_VERSION, Table, Writers};
use crate::files::{ID_KEY, remove_node_file};
use crate::index::{self, ReverseIndex};
use crate::sort::Sorter;
use crate::{GeometryFormat, Mirror, SyncError};

/// The revision of the imported data.
pub(crate) const IMPORT_REV: i64 = 1;

/// How to import.
#[derive(Debug, Clone)]
pub struct ImportOpts {
    /// Drop an existing `osm` schema first; without it, one is an error.
    pub replace: bool,
    /// The URL of the replication stream that continues the file, kept for updates, instead of the one its header
    /// names.
    pub replication_url: Option<String>,
    /// The stream's sequence the file is up to, instead of the one its header gives. Without either, it is found from
    /// the file's timestamp in the stream, as for a planet file.
    pub sequence: Option<u64>,
    /// The database connections rows are copied on, at once.
    pub connections: usize,
    /// How `osm.nodes` and `osm.ways` store their geometries, from then on: TWKB by default.
    pub geometry: GeometryFormat,
}

impl Default for ImportOpts {
    fn default() -> Self {
        Self {
            replace: false,
            replication_url: None,
            sequence: None,
            connections: crate::default_connections(),
            geometry: GeometryFormat::default(),
        }
    }
}

/// What an import loaded.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImportStats {
    /// Nodes in the node file.
    pub nodes: u64,
    /// Rows of `osm.nodes`: the tagged nodes.
    pub tagged_nodes: u64,
    /// Rows of `osm.ways`.
    pub ways: u64,
    /// Rows of `osm.relations`.
    pub relations: u64,
    /// (node, way) pairs in the reverse index.
    pub pairs: u64,
}

pub(crate) fn import(
    m: &Mirror,
    pbf: &Path,
    replication: Option<&Replication>,
    opts: &ImportOpts,
) -> Result<ImportStats, SyncError> {
    let reader = PbfReader::open(pbf)?;
    let header = reader.header();
    let header_sequence = match header.replication_sequence.map(i64::cast_unsigned) {
        Some(h) if !same_url(opts, header.replication_base_url.as_ref()) => {
            header_stream(h, header.replication_timestamp, replication)?
        }
        h => h,
    };
    let (sequence, timestamp) = start(
        opts.sequence,
        header_sequence,
        header.replication_timestamp,
        replication,
    )?;
    let url = opts
        .replication_url
        .clone()
        .or_else(|| header.replication_base_url.clone());

    let mut client = db::connect(&m.url)?;
    create_schema(&mut client, opts.replace, opts.geometry)?;
    fs::create_dir_all(&m.dir)?;
    remove_node_file(&m.node_path())?;
    index::remove(&m.index_path())?;
    let id = random_id();

    let t = Instant::now();
    let mut store = PagedStore::open(m.node_path())?;
    let mut stats = ImportStats {
        nodes: reader.load_nodes(&mut store)?,
        ..ImportStats::default()
    };
    store.preload()?;
    info!(
        nodes = stats.nodes,
        secs = t.elapsed().as_secs_f32(),
        "loaded the node file"
    );

    let t = Instant::now();
    let pairs = copy_objects(
        &reader,
        &store,
        &m.url,
        opts.connections,
        opts.geometry,
        &mut stats,
        &m.dir,
    )?;
    info!(
        tagged_nodes = stats.tagged_nodes,
        ways = stats.ways,
        relations = stats.relations,
        secs = t.elapsed().as_secs_f32(),
        "copied the objects"
    );

    // The files and the database are finished at once: the index on this thread, the tables on connections of
    // their own
    let t = Instant::now();
    let (index, tables) = thread::scope(|s| {
        let tables = s.spawn(|| finish_tables(&m.url));
        let t = Instant::now();
        let index = pairs.finish().map_err(SyncError::from).and_then(|sorted| {
            ReverseIndex::create(
                &m.index_path(),
                id,
                IMPORT_REV,
                sorted.map(|p| p.map_err(SyncError::from)),
            )
        });
        debug!(secs = t.elapsed().as_secs_f32(), "built the reverse index");
        (
            index,
            tables
                .join()
                .unwrap_or_else(|_| Err(SyncError::state("finishing the tables panicked"))),
        )
    });
    stats.pairs = index?.1;
    tables?;
    store.set_metadata(ID_KEY, &id.to_le_bytes())?;
    store.commit(IMPORT_REV.cast_unsigned())?;
    info!(
        pairs = stats.pairs,
        secs = t.elapsed().as_secs_f32(),
        "built the reverse index and the primary keys"
    );

    let position = DbState {
        id,
        rev: IMPORT_REV,
        url,
        sequence,
        timestamp,
        geometry: opts.geometry,
    };
    record(&mut client, &position, &stats)?;
    Ok(stats)
}

/// Records the import's batch and the database's state, which marks the import done.
fn record(client: &mut Client, at: &DbState, loaded: &ImportStats) -> Result<(), SyncError> {
    let timestamp = at.timestamp.map(Timestamp::as_millis);
    let mut tx = client.transaction()?;
    tx.execute(
        "INSERT INTO osm.batches (rev, first_sequence, last_sequence, timestamp, nodes, ways, relations) \
         VALUES ($1, $2, $2, to_timestamp($3::int8 / 1000.0), $4, $5, $6)",
        &[
            &at.rev,
            &at.sequence,
            &timestamp,
            &loaded.tagged_nodes.cast_signed(),
            &loaded.ways.cast_signed(),
            &loaded.relations.cast_signed(),
        ],
    )?;
    tx.execute(
        "INSERT INTO osm.state (id, version, rev, url, sequence, timestamp, geometry) \
         VALUES ($1, $2, $3, $4, $5, to_timestamp($6::int8 / 1000.0), $7)",
        &[
            &at.id,
            &SCHEMA_VERSION,
            &at.rev,
            &at.url,
            &at.sequence,
            &timestamp,
            &at.geometry.as_ref(),
        ],
    )?;
    tx.commit()?;
    Ok(())
}

/// Whether the stream is the one the header names: none other given, or the same URL.
fn same_url(opts: &ImportOpts, header_url: Option<&String>) -> bool {
    opts.replication_url
        .as_ref()
        .is_none_or(|u| Some(u) == header_url)
}

/// The header's sequence `h`, if it is of `replication`, another URL than the header's: a copy of the header's stream
/// has the header's time at `h`, or no state of it, as a directory of only the diffs after the file, and a stream not
/// given cannot say otherwise. Another stream's state at `h` has another time, and its sequence is found from the
/// file's time instead.
fn header_stream(
    h: u64,
    time: Option<Timestamp>,
    replication: Option<&Replication>,
) -> Result<Option<u64>, SyncError> {
    let Some(r) = replication else {
        return Ok(Some(h));
    };
    Ok(match r.get(h)? {
        Some(state) if Some(state.timestamp) != time => None,
        _ => Some(h),
    })
}

/// The sequence the file is up to, and the time of its state.
///
/// An extract's header gives its sequence on its own stream (`header`), which is exact: a later one given would skip
/// changes and is an error. Without it, as for a planet file, it is found or checked in the stream from the file's
/// timestamp: the file has the changes made up to then, and a change can show in a diff up to [`COMMIT_DELAY`] after
/// it is made, so a sequence after the last diff before then is an error. An earlier sequence only repeats changes the
/// file has, which applying again changes nothing in the end, so it is only warned of.
fn start(
    given: Option<u64>,
    header: Option<u64>,
    file_time: Option<Timestamp>,
    replication: Option<&Replication>,
) -> Result<(i64, Option<Timestamp>), SyncError> {
    // The last sequence the file is known to have, and its state's time
    let bound = match (header, replication, file_time) {
        (Some(h), ..) => Some((h, file_time)),
        (None, Some(r), Some(at)) => {
            let sources: [&dyn StateSource; 1] = [r];
            let first = plan(&sources, Start::Snapshot { at }, None)?
                .first()
                .map(|s| s.from);
            let since = first.map_or_else(|| r.state(), Ok)?;
            Some((since.sequence, Some(since.timestamp)))
        }
        (None, ..) => None,
    };
    match (given, bound) {
        (None, Some((b, time))) => {
            if header.is_none() {
                info!(sequence = b, "found the file's sequence from its timestamp");
            }
            Ok((b.cast_signed(), time))
        }
        (Some(g), Some((b, time))) if g == b => Ok((g.cast_signed(), time)),
        (Some(g), Some((b, _))) if g > b => Err(SyncError::state(match header {
            Some(_) => format!(
                "the header says the file is up to diff {b}, so diffs {} to {g} would be skipped; give sequence \
                 {b} or earlier",
                b + 1
            ),
            None => format!(
                "the file has the changes made up to {}, and a change can show in a diff {} minutes after it is \
                 made, so diffs {} to {g} may have changes it does not have; give sequence {b} or earlier",
                file_time.unwrap_or_default(),
                COMMIT_DELAY.as_secs() / 60,
                b + 1,
            ),
        })),
        (Some(g), bound) => {
            if let Some((b, _)) = bound {
                warn!(
                    "sequence {g} is {} diffs before the file's {b}: until they are applied, the database has older \
                     versions of the objects they change, and nodes they move add to their ways' minor",
                    b - g
                );
            }
            let time = replication
                .map(|r| r.sequence_state(g))
                .transpose()?
                .map(|s| s.timestamp);
            Ok((g.cast_signed(), time))
        }
        (None, None) => Err(SyncError::state(if file_time.is_some() {
            "the header gives no replication sequence; give the replication stream, to find it from the file's \
             timestamp, or the sequence"
        } else {
            "the header gives neither a replication sequence nor a timestamp; give the sequence"
        })),
    }
}

/// Creates the `osm` schema, with its geometry columns and `osm.geom` for `format`, after dropping an existing one
/// if `replace`.
fn create_schema(
    client: &mut Client,
    replace: bool,
    format: GeometryFormat,
) -> Result<(), SyncError> {
    if replace {
        client.batch_execute("DROP SCHEMA IF EXISTS osm CASCADE")?;
    } else if client
        .query_one("SELECT to_regnamespace('osm') IS NOT NULL", &[])?
        .get(0)
    {
        return Err(SyncError::state(
            "the database already has an osm schema: replace it, or update it",
        ));
    }
    // PostGIS first, so the function can name its schema
    client.batch_execute("CREATE EXTENSION IF NOT EXISTS postgis")?;
    let postgis: String = client
        .query_one(
            "SELECT n.nspname FROM pg_extension e JOIN pg_namespace n ON n.oid = e.extnamespace \
             WHERE e.extname = 'postgis'",
            &[],
        )?
        .get(0);
    let (point, line, geom) = format.sql(&postgis);
    let schema = include_str!("schema.sql")
        .replace("{point}", point)
        .replace("{line}", line)
        .replace("{geom_function}", &geom);
    client.batch_execute(&schema)?;
    Ok(())
}

/// Copies the tagged nodes, the ways and the relations into their tables, and returns the reverse index's pairs, to
/// sort with temporary files in `dir`.
fn copy_objects(
    reader: &PbfReader,
    store: &PagedStore,
    url: &str,
    connections: usize,
    format: GeometryFormat,
    stats: &mut ImportStats,
    dir: &Path,
) -> Result<Sorter, SyncError> {
    let writers = Writers::start(url, connections)?;
    let pairs = Sorter::new(dir)?;
    let counts = [const { AtomicU64::new(0) }; 3];
    let read = reader.par_resolve(Select::all(), store, |block, locator| {
        let mut rows = [Rows::new(format), Rows::new(format), Rows::default()];
        for n in block.nodes().filter(|n| n.tags().len() > 0) {
            let loc = Location::new(n.decimicro_lat(), n.decimicro_lon());
            rows[0].node(n.id(), n.info().into(), IMPORT_REV, n.tags(), Some(loc));
        }
        let mut block_pairs = Vec::new();
        let mut locs = Vec::new();
        for w in block.ways() {
            locs.clear();
            let complete = w
                .refs()
                .all(|r| locator.location(r).map(|l| locs.push(l)).is_some());
            block_pairs.extend(
                w.refs()
                    .filter_map(|n| Some((u64::try_from(n).ok()?, w.id().cast_unsigned()))),
            );
            let line = complete.then_some(&locs[..]);
            rows[1].way(
                w.id(),
                w.info().into(),
                IMPORT_REV,
                w.tags(),
                w.refs(),
                line,
            );
        }
        for r in block.relations() {
            rows[2].relation(r.id(), r.info().into(), IMPORT_REV, r.tags(), r.members());
        }
        let mut sent = true;
        for ((rows, count), table) in
            rows.into_iter()
                .zip(&counts)
                .zip([Table::Nodes, Table::Ways, Table::Relations])
        {
            count.fetch_add(rows.count, Ordering::Relaxed);
            sent &= writers.send(table, rows);
        }
        pairs.extend(block_pairs).map_err(PbfError::Io)?;
        if sent {
            Ok(())
        } else {
            Err(PbfError::Io(io::Error::other(
                "the database connections stopped",
            )))
        }
    });
    // A failed connection stops the read too, so its error goes first
    writers.finish()?;
    read?;
    [stats.tagged_nodes, stats.ways, stats.relations] = counts.map(AtomicU64::into_inner);
    Ok(pairs)
}

/// Adds the primary keys and analyzes, each table on a connection of its own. Autovacuum, off during the load,
/// vacuums them later.
fn finish_tables(url: &str) -> Result<(), SyncError> {
    thread::scope(|s| {
        let tables = [Table::Nodes, Table::Ways, Table::Relations].map(|t| {
            s.spawn(move || -> Result<(), SyncError> {
                let mut client = db::connect(url)?;
                for sql in [
                    format!("ALTER TABLE {t} ADD PRIMARY KEY (id)"),
                    format!("ANALYZE {t}"),
                    format!("ALTER TABLE {t} RESET (autovacuum_enabled)"),
                ] {
                    let t = Instant::now();
                    client.batch_execute(&sql)?;
                    debug!(secs = t.elapsed().as_secs_f32(), sql, "finished");
                }
                Ok(())
            })
        });
        tables.into_iter().try_for_each(|t| {
            t.join()
                .unwrap_or_else(|_| Err(SyncError::state("finishing a table panicked")))
        })
    })
}

/// A random positive identity for a new database. `RandomState` is keyed from the system's random source, once per
/// thread and then stepped; the time is hashed too, to tell apart imports that start alike.
fn random_id() -> i64 {
    let mut h = RandomState::new().build_hasher();
    let now = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH);
    h.write_u128(now.map_or(0, |d| d.as_nanos()));
    (h.finish() >> 1).cast_signed()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stream whose diffs 1705 to 1709 are at 20:08, 20:14, 20:20, 20:21:01 and 20:22.
    fn stream(dir: &Path) -> Replication {
        let times = ["20:08:00", "20:14:00", "20:20:00", "20:21:01", "20:22:00"];
        fs::create_dir_all(dir.join("000/001")).unwrap();
        for (n, time) in (705..).zip(times) {
            let state = format!(
                "sequenceNumber={}\ntimestamp=2025-04-23T{}Z\n",
                1000 + n,
                time.replace(':', "\\:")
            );
            fs::write(dir.join(format!("000/001/{n}.state.txt")), &state).unwrap();
            if n == 709 {
                fs::write(dir.join("state.txt"), &state).unwrap();
            }
        }
        Replication::local(dir)
    }

    #[test]
    fn the_sequence_is_found_or_checked() {
        let dir = tempfile::tempdir().unwrap();
        let r = stream(dir.path());
        let at = |t: &str| format!("2025-04-23T{t}Z").parse::<Timestamp>().unwrap();
        let made = Some(at("20:21:01"));
        // As the header gives it, given again or not, without reading the stream; a later one is an error
        assert_eq!(start(None, Some(1708), made, None).unwrap(), (1708, made));
        assert_eq!(
            start(Some(1708), Some(1708), made, Some(&r)).unwrap(),
            (1708, made)
        );
        let err = start(Some(1709), Some(1708), made, Some(&r))
            .unwrap_err()
            .to_string();
        assert!(err.contains("give sequence 1708"), "{err}");
        assert_eq!(
            start(Some(1707), Some(1708), made, Some(&r)).unwrap(),
            (1707, Some(at("20:20:00")))
        );
        // From the file's time: changes made after 20:15:01 can be in diff 1707
        let found = (1706, Some(at("20:14:00")));
        assert_eq!(start(None, None, made, Some(&r)).unwrap(), found);
        assert_eq!(start(Some(1706), None, made, Some(&r)).unwrap(), found);
        let err = start(Some(1708), None, made, Some(&r))
            .unwrap_err()
            .to_string();
        assert!(err.contains("diffs 1707 to 1708"), "{err}");
        start(None, None, made, None).unwrap_err();
        assert_eq!(start(Some(1708), None, None, None).unwrap(), (1708, None));
    }

    #[test]
    fn a_copy_of_the_headers_stream_keeps_its_sequence() {
        let dir = tempfile::tempdir().unwrap();
        let r = stream(dir.path());
        let at = |t: &str| Some(format!("2025-04-23T{t}Z").parse::<Timestamp>().unwrap());
        // The same time at 1708, or no state of 1710: the header's stream; another time at 1708: another stream
        assert_eq!(
            header_stream(1708, at("20:21:01"), Some(&r)).unwrap(),
            Some(1708)
        );
        assert_eq!(
            header_stream(1710, at("20:23:00"), Some(&r)).unwrap(),
            Some(1710)
        );
        assert_eq!(header_stream(1708, at("20:30:00"), Some(&r)).unwrap(), None);
    }
}
