//! `osmflow sync`: OpenStreetMap data in PostgreSQL, imported from a PBF file and kept current from replication.

use std::fmt;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use osmflow::PbfReader;
use osmflow::osc::Replication;
use osmflow_sync::{
    DbState, FileStamp, GeometryFormat, ImportOpts, Mirror, Rebuild, SyncError, UpdateOpts,
};

use crate::input::fetch;
use crate::select::names;

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
        /// How to store the geometries of nodes and ways, from then on; SQL reads either with `osm.geom(geom)`.
        ///
        /// `twkb` is TWKB with OSM's 7 decimal digits, in a `bytea`: about a quarter of a line's WKB. `wkb` is
        /// PostGIS's `geometry`, read without decoding.
        #[arg(long, default_value_t, value_parser = names::<GeometryFormat>())]
        geometry: GeometryFormat,
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
            geometry,
        } => {
            let mut opts = ImportOpts {
                replace: *replace,
                replication_url: replication.clone(),
                sequence: *sequence,
                geometry: *geometry,
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
            out(format_args!(
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
            out(format_args!(
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
            out(format_args!(
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
            let mut o = io::stdout().lock();
            write!(o, "database      revision {rev}, sequence {sequence}")?;
            if let Some(t) = timestamp {
                write!(o, " at {t}")?;
            }
            writeln!(o, " of {}", url.as_deref().unwrap_or("no stream"))?;
            writeln!(o, "node file     {}", file(&s.nodes, &s.db))?;
            Ok(writeln!(o, "reverse index {}", file(&s.index, &s.db))?)
        }
    }
}

fn out(line: fmt::Arguments<'_>) -> Result<()> {
    writeln!(io::stdout(), "{line}")?;
    Ok(())
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

#[cfg(test)]
mod tests {
    use std::{env, fs};

    use clap::Parser as _;
    use osmflow_sync::NodeWay;

    use super::*;

    /// The fixture, whose header gives replication sequence 1708.
    const PBF: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../osmflow/fixtures/osmpbfreader/clipperton.osm.pbf"
    );
    /// A node of way 178300745, which diff 1709 moves.
    const NODE: i64 = 1_886_651_627;

    #[derive(clap::Parser)]
    struct Sync {
        #[command(flatten)]
        args: Args,
    }

    /// Runs `osmflow sync` with `args`.
    fn sync(args: &[&str]) {
        let cli = Sync::try_parse_from(["sync"].iter().chain(args)).unwrap();
        run(&cli.args).unwrap();
    }

    /// Every subcommand of `osmflow sync`, against the database `OSMFLOW_SYNC_TEST_DB` names (`just test-with-db`
    /// starts one), whose `osm` schema is replaced; without it, the test does nothing.
    #[test]
    fn import_update_status_rebuild() {
        let Ok(url) = env::var("OSMFLOW_SYNC_TEST_DB") else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        // A copy of the fixture's stream: its state 1708, then diff 1709, which moves NODE, and is the latest
        let diffs = dir.path().join("diffs");
        fs::create_dir_all(diffs.join("000/001")).unwrap();
        let state = |n: u32, at: &str| format!("sequenceNumber={n}\ntimestamp={at}\n");
        fs::write(
            diffs.join("000/001/708.state.txt"),
            state(1708, "2025-04-23T20\\:21\\:01Z"),
        )
        .unwrap();
        for latest in ["000/001/709.state.txt", "state.txt"] {
            fs::write(diffs.join(latest), state(1709, "2026-11-01T00\\:00\\:30Z")).unwrap();
        }
        fs::write(
            diffs.join("000/001/709.osc.gz"),
            format!(
                r#"<osmChange version="0.6"><modify><node id="{NODE}" version="9" timestamp="2026-11-01T00:00:00Z" lat="10.3" lon="-109.2"/></modify></osmChange>"#
            ),
        )
        .unwrap();
        let files = dir.path().join("files");
        let place = ["--db", &url, "--dir", files.to_str().unwrap()];
        let diffs = diffs.to_str().unwrap();
        let mirror = Mirror::new(&url, &files);
        let at = |rev: i64| {
            let s = mirror.status().unwrap();
            let stamp = FileStamp {
                id: Some(s.db.id),
                rev,
            };
            assert_eq!(
                (s.db.rev, s.nodes.unwrap(), s.index.unwrap()),
                (rev, stamp, stamp)
            );
            s.db.sequence
        };
        for geometry in ["twkb", "wkb"] {
            sync(
                &[
                    &["import"],
                    &place[..],
                    &[
                        PBF,
                        "--replace",
                        "--replication",
                        diffs,
                        "--geometry",
                        geometry,
                    ],
                ]
                .concat(),
            );
            assert_eq!(at(1), 1708, "{geometry}");
            assert_eq!(mirror.state().unwrap().geometry.to_string(), geometry);
            sync(&[&["update"], &place[..]].concat());
            assert_eq!(at(2), 1709, "{geometry}");
            let way = NodeWay {
                node: NODE,
                way: 178_300_745,
            };
            let through = mirror.open().unwrap().ways_through(&[NODE]).unwrap();
            assert!(through.contains(&way), "{through:?}");
            sync(&[&["status"], &place[..]].concat());
            for from in [&[][..], &["--pbf", PBF][..]] {
                fs::remove_file(mirror.index_path()).unwrap();
                sync(&[&["rebuild"], &place[..], from].concat());
                assert_eq!(at(2), 1709, "{geometry} {from:?}");
            }
        }
    }
}
