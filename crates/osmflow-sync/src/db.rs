//! The database side: connections, the state row, and writing rows with binary `COPY` on several connections.

use std::io::Write as _;
use std::thread::{self, JoinHandle};

use crossbeam_channel::{Receiver, Sender, bounded};
use osmflow::Timestamp;
use postgres::{Client, CopyInWriter, GenericClient, NoTls};

use crate::SyncError;
use crate::copy::{HEADER, Rows, TRAILER};

/// The format of the `osm` schema this crate reads and writes.
pub(crate) const SCHEMA_VERSION: i32 = 1;

pub(crate) fn connect(url: &str) -> Result<Client, SyncError> {
    Ok(Client::connect(url, NoTls)?)
}

/// Where the database is in its replication stream, from `osm.state`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DbState {
    /// A random number made by the import, which the node file and the reverse index carry too.
    pub id: i64,
    /// The last revision: 1 after the import, one more after each update batch.
    pub rev: i64,
    /// The replication stream's base URL, if known.
    pub url: Option<String>,
    /// The last sequence of the stream applied.
    pub sequence: i64,
    /// The time of that sequence's state.
    pub timestamp: Option<Timestamp>,
}

impl DbState {
    /// Reads the state row; an error if there is no imported database.
    pub(crate) fn read(client: &mut impl GenericClient) -> Result<Self, SyncError> {
        let exists: bool = client
            .query_one("SELECT to_regclass('osm.state') IS NOT NULL", &[])?
            .get(0);
        if !exists {
            return Err(SyncError::state(
                "the database has no osm schema: import a PBF file first",
            ));
        }
        let row = client
            .query_opt(
                "SELECT id, version, rev, url, sequence, (extract(epoch FROM timestamp) * 1000)::int8 FROM osm.state",
                &[],
            )?
            .ok_or_else(|| SyncError::state("the import did not finish: run it again, replacing the osm schema"))?;
        let version: i32 = row.get(1);
        if version != SCHEMA_VERSION {
            return Err(SyncError::state(format!(
                "the osm schema has format {version}, but this osmflow-sync reads {SCHEMA_VERSION}"
            )));
        }
        Ok(Self {
            id: row.get(0),
            rev: row.get(2),
            url: row.get(3),
            sequence: row.get(4),
            timestamp: row.get::<_, Option<i64>>(5).map(Timestamp::from_millis),
        })
    }
}

/// A table rows are copied into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Table {
    Nodes,
    Ways,
    Relations,
}

impl Table {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Nodes => "osm.nodes",
            Self::Ways => "osm.ways",
            Self::Relations => "osm.relations",
        }
    }

    /// The table's columns, in the order rows are encoded.
    pub(crate) fn columns(self) -> &'static [&'static str] {
        match self {
            Self::Nodes => &["id", "version", "changeset", "ts", "rev", "tags", "geom"],
            Self::Ways => &[
                "id",
                "version",
                "changeset",
                "ts",
                "minor",
                "rev",
                "tags",
                "nodes",
                "geom",
            ],
            Self::Relations => &[
                "id",
                "version",
                "changeset",
                "ts",
                "rev",
                "tags",
                "member_types",
                "member_ids",
                "member_roles",
            ],
        }
    }
}

/// Copies `rows` into `table` (a name, with its columns in the rows' order) in one `COPY`.
pub(crate) fn copy(
    client: &mut impl GenericClient,
    table: &str,
    columns: &str,
    rows: &Rows,
) -> Result<(), SyncError> {
    let mut w = copy_in(client, table, columns)?;
    w.write_all(&rows.buf)?;
    copy_end(w)
}

/// Starts a binary `COPY` into `table`'s `columns`: rows are written to it, then [`copy_end`] ends it.
fn copy_in<'a>(
    client: &'a mut impl GenericClient,
    table: &str,
    columns: &str,
) -> Result<CopyInWriter<'a>, SyncError> {
    let mut w = client.copy_in(&format!(
        "COPY {table} ({columns}) FROM STDIN (FORMAT binary)"
    ))?;
    w.write_all(HEADER)?;
    Ok(w)
}

fn copy_end(mut w: CopyInWriter<'_>) -> Result<(), SyncError> {
    w.write_all(TRAILER)?;
    w.finish()?;
    Ok(())
}

/// Copies rows into the tables on several connections at once, each with a `COPY` per table it is sent rows of.
pub(crate) struct Writers {
    tx: Sender<(Table, Vec<u8>)>,
    threads: Vec<JoinHandle<Result<(), SyncError>>>,
}

impl Writers {
    pub(crate) fn start(url: &str, connections: usize) -> Result<Self, SyncError> {
        let (tx, rx) = bounded(connections * 4);
        let threads = (0..connections.max(1))
            .map(|i| {
                let (url, rx) = (url.to_owned(), rx.clone());
                thread::Builder::new()
                    .name(format!("osmflow-copy-{i}"))
                    .spawn(move || write(&url, &rx))
            })
            .collect::<Result<_, _>>()?;
        Ok(Self { tx, threads })
    }

    /// Hands rows to a connection; false if the connections stopped, as after an error, which
    /// [`finish`](Self::finish) returns.
    pub(crate) fn send(&self, table: Table, rows: Rows) -> bool {
        rows.buf.is_empty() || self.tx.send((table, rows.buf)).is_ok()
    }

    /// Ends every `COPY`, and returns the first error.
    pub(crate) fn finish(self) -> Result<(), SyncError> {
        drop(self.tx);
        let mut result = Ok(());
        for t in self.threads {
            let r = t
                .join()
                .unwrap_or_else(|_| Err(SyncError::state("a COPY thread panicked")));
            result = result.and(r);
        }
        result
    }
}

/// One connection's work: a `COPY` for each run of rows of one table.
fn write(url: &str, rx: &Receiver<(Table, Vec<u8>)>) -> Result<(), SyncError> {
    let mut client = connect(url)?;
    client.batch_execute("SET synchronous_commit = off; SET default_toast_compression = lz4")?;
    let mut next = rx.recv().ok();
    while let Some((table, buf)) = next {
        let mut w = copy_in(&mut client, table.name(), &table.columns().join(", "))?;
        w.write_all(&buf)?;
        loop {
            next = rx.recv().ok();
            match &next {
                Some((t, buf)) if *t == table => w.write_all(buf)?,
                _ => break,
            }
        }
        copy_end(w)?;
    }
    Ok(())
}
