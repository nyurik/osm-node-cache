//! `osmflow sync`: OpenStreetMap data in PostgreSQL, imported from a PBF file and kept current from replication.

use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use osmflow::PbfReader;
use osmflow::osc::Replication;
use osmflow_sync::{DbState, FileStamp, ImportOpts, Mirror, Rebuild, SyncError, UpdateOpts};

use crate::input::fetch;

#[derive(Debug, clap::Args)]
pub struct Args {
    #[command(subcommand)]
    command: Command,
}

/// Where the data is: the database and the directory of its files.
#[derive(Debug, clap::Args)]
struct Place {
    /// The database: `postgresql://user@host/db`, or `host=… user=… dbname=…`. Connections are not encrypted.
    #[arg(long, env = "DATABASE_URL", value_name = "URL")]
    db: String,
    /// The directory of the node file and the reverse index, beside the database.
    #[arg(long, env = "OSMFLOW_SYNC_DIR", value_name = "DIR")]
    dir: PathBuf,
}

/// Where diffs come from.
#[derive(Debug, clap::Args)]
struct Diffs {
    /// Where to keep the downloaded diffs; `<dir>/replication` by default. A file there is never downloaded again.
    #[arg(long, value_name = "DIR")]
    cache: Option<PathBuf>,
    /// The most diffs applied in one transaction.
    #[arg(long, default_value_t = UpdateOpts::default().max_diffs, value_name = "N")]
    max_diffs: u64,
}

#[derive(Debug, clap::Subcommand)]
enum Command {
    /// Creates the database's osm schema from a PBF file, and the node file and reverse index beside it.
    ///
    /// The file must be sorted by type and ID, as published files are. A Geofabrik extract's header names its
    /// replication stream and sequence. For a planet file, give the stream: the sequence is found from the file's
    /// timestamp, 6 minutes before it, as a change can show in a diff that long after it is made.
    Import {
        #[command(flatten)]
        place: Place,
        /// The PBF file.
        pbf: PathBuf,
        /// Drop an existing osm schema first.
        #[arg(long)]
        replace: bool,
        /// The replication stream that continues the file, instead of the one its header names.
        #[arg(long, value_name = "URL")]
        replication: Option<String>,
        /// The stream's sequence the file is up to, instead of the one its header gives or its timestamp finds; a
        /// later one, which would skip changes, is an error.
        #[arg(long, value_name = "SEQ")]
        sequence: Option<u64>,
        /// Where to keep the stream's state files read to find the sequence; `<dir>/replication` by default.
        #[arg(long, value_name = "DIR")]
        cache: Option<PathBuf>,
        /// Database connections to copy on at once; half the cores, up to 8, by default.
        #[arg(long, value_name = "N")]
        connections: Option<usize>,
    },
    /// Applies the replication diffs after the database's sequence, up to the latest.
    ///
    /// Brings the node file and the reverse index up to the database first, if a crash left them behind.
    Update {
        #[command(flatten)]
        place: Place,
        #[command(flatten)]
        diffs: Diffs,
        /// The last sequence to apply.
        #[arg(long, value_name = "SEQ")]
        until: Option<u64>,
    },
    /// Makes the node file and the reverse index again, up to the database. The database is only read.
    ///
    /// Without --pbf, node locations come from the database: its ways' geometries and its tagged nodes, so an untagged
    /// node in no way is left out, which matters only if a later edit adds it to a way. With it, from the PBF file,
    /// such as a new download of the planet or the extract, then the diffs up to the database's sequence.
    Rebuild {
        #[command(flatten)]
        place: Place,
        /// The PBF file to take node locations from, at or before the database's sequence.
        #[arg(long)]
        pbf: Option<PathBuf>,
        /// The stream's sequence the PBF file is up to, instead of the one its header gives.
        #[arg(long, value_name = "SEQ", requires = "pbf")]
        sequence: Option<u64>,
        #[command(flatten)]
        diffs: Diffs,
    },
    /// Shows the database's replication sequence and revision, and its files'.
    Status {
        #[command(flatten)]
        place: Place,
    },
}

pub fn run(args: &Args) -> Result<()> {
    match &args.command {
        Command::Import {
            place,
            pbf,
            replace,
            replication,
            sequence,
            cache,
            connections,
        } => {
            let mut opts = ImportOpts {
                replace: *replace,
                replication_url: replication.clone(),
                sequence: *sequence,
                ..ImportOpts::default()
            };
            if let Some(n) = connections {
                opts.connections = *n;
            }
            let url = match replication {
                Some(url) => Some(url.clone()),
                None => PbfReader::open(pbf)?.header().replication_base_url.clone(),
            };
            let stream = url
                .map(|url| stream(&url, place, cache.as_ref()))
                .transpose()?;
            let s = mirror(place)
                .import(pbf, stream.as_ref(), &opts)
                .with_context(|| format!("importing {}", pbf.display()))?;
            out(&format!(
                "imported {} nodes: {} tagged, {} ways, {} relations; {} reverse index pairs",
                s.nodes, s.tagged_nodes, s.ways, s.relations, s.pairs
            ))
        }
        Command::Update {
            place,
            diffs,
            until,
        } => {
            let mirror = mirror(place);
            let replication = replication(&mirror, place, diffs)?;
            let opts = UpdateOpts {
                max_diffs: diffs.max_diffs,
                until: *until,
            };
            let s = mirror.update(&replication, &opts).context("updating")?;
            out(&format!(
                "applied {} diffs in {} batches: {} rows, {} ways moved by their nodes; now at sequence {}",
                s.diffs, s.batches, s.rows, s.moved_ways, s.sequence
            ))
        }
        Command::Rebuild {
            place,
            pbf,
            sequence,
            diffs,
        } => {
            let mirror = mirror(place);
            let s = match pbf {
                None => mirror.rebuild(&Rebuild::Database),
                Some(path) => {
                    let replication = replication(&mirror, place, diffs)?;
                    let from = Rebuild::Pbf {
                        path,
                        sequence: *sequence,
                        replication: &replication,
                        max_diffs: diffs.max_diffs,
                    };
                    mirror.rebuild(&from)
                }
            }
            .context("rebuilding")?;
            out(&format!(
                "rebuilt the node file from {} nodes and {} diffs, and the reverse index with {} pairs",
                s.nodes, s.diffs, s.pairs
            ))
        }
        Command::Status { place } => {
            let s = mirror(place).status()?;
            let DbState {
                rev,
                url,
                sequence,
                timestamp,
                ..
            } = &s.db;
            let time = timestamp.map_or_else(String::new, |t| format!(" at {t}"));
            out(&format!(
                "database      revision {rev}, sequence {sequence}{time} of {}",
                url.as_deref().unwrap_or("no stream")
            ))?;
            out(&format!("node file     {}", file(&s.nodes, &s.db)))?;
            out(&format!("reverse index {}", file(&s.index, &s.db)))
        }
    }
}

fn mirror(place: &Place) -> Mirror {
    Mirror::new(&place.db, &place.dir)
}

/// The database's replication stream.
fn replication(mirror: &Mirror, place: &Place, diffs: &Diffs) -> Result<Replication> {
    let url = mirror
        .state()?
        .url
        .context("the database has no replication stream; import with --replication")?;
    stream(&url, place, diffs.cache.as_ref())
}

/// The stream at `url`: a server read through `cache` (`<dir>/replication` by default), or a directory.
fn stream(url: &str, place: &Place, cache: Option<&PathBuf>) -> Result<Replication> {
    if url.starts_with("http://") || url.starts_with("https://") {
        let cache = cache.map_or_else(|| place.dir.join("replication"), Clone::clone);
        Ok(Replication::remote(url, cache, fetch(url)?))
    } else {
        Ok(Replication::local(Path::new(url)))
    }
}

/// A file's line of the status.
fn file(stamp: &Result<FileStamp, SyncError>, db: &DbState) -> String {
    match stamp {
        Err(e) => format!("unreadable: {e}"),
        Ok(s) if s.id != Some(db.id) => {
            format!("revision {}, not made for this database: rebuild it", s.rev)
        }
        Ok(s) if s.rev == db.rev => format!("revision {}, up to date", s.rev),
        Ok(s) if s.rev < db.rev => format!(
            "revision {}, behind: the next update brings it forward",
            s.rev
        ),
        Ok(s) => format!("revision {}, ahead of the database: rebuild it", s.rev),
    }
}

fn out(line: &str) -> Result<()> {
    writeln!(io::stdout(), "{line}")?;
    Ok(())
}
