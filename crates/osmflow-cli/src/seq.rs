//! `osmflow seq`: the sequence of a replication stream at a time, or where a file leaves off, from its state files
//! alone.

use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use osmflow::osc::{LATE_MS, Located, Start, State, StateSource, Timestamp, locate, plan};

use crate::input::{After, Input, Since, after_file, ago, parse_since, stream_states};
use crate::output::{SummaryFormat, count, json_str, opt_time};

#[derive(Debug, clap::Args)]
pub struct Args {
    /// A replication directory or server URL.
    ///
    /// A directory laid out as the servers lay theirs out, or a server's URL, as
    /// `https://planet.openstreetmap.org/replication/minute`.
    input: PathBuf,
    /// A time, a PBF file, a state file, or `latest`.
    ///
    /// A time is `2026-11-09T21:00:00Z` or `2026-11-09`. A PBF file gives the sequence in its header when its base
    /// URL is this stream's, as a Geofabrik extract's is, and otherwise its timestamp, as a planet file's does.
    #[arg(value_parser = parse_when)]
    when: When,
    /// Where to keep a server's downloaded state files.
    ///
    /// Without it, the few state files the search reads are downloaded each time.
    #[arg(long, value_name = "DIR")]
    cache: Option<PathBuf>,
    /// The output format.
    #[arg(short, long, value_enum, default_value_t)]
    format: SummaryFormat,
}

/// The point of a stream to find.
#[derive(Debug, Clone, PartialEq, Eq)]
enum When {
    Latest,
    Since(Since),
}

fn parse_when(s: &str) -> Result<When> {
    if s == "latest" {
        return Ok(When::Latest);
    }
    parse_since(s).map(When::Since)
}

/// What the search found.
struct Found {
    latest: State,
    point: Point,
    /// The diff the point lands on: the last at or before a time, or the one a file is up to.
    state: State,
    /// Whether `state` is exactly where the point is, rather than found by time.
    exact: bool,
    /// The diff before the first to read so that no change after the point is missed.
    since: State,
}

/// What was given, for the text output.
enum Point {
    Latest,
    Time(i64),
    /// A PBF file whose header gives its diff on this stream.
    Header(String),
    StateFile(String),
    /// A PBF file whose header gives a time only, as a planet file's does, or a diff of another stream.
    Snapshot(String, i64),
}

pub fn run(args: &Args) -> Result<()> {
    let input = Input::of(&args.input)?;
    if !matches!(input, Input::Dir(_) | Input::Url(_)) {
        bail!(
            "{} is not a replication stream: give a directory laid out as the servers lay theirs out, or a \
             server's URL",
            input.name()
        );
    }
    let (source, latest, url) = stream_states(&input, args.cache.as_deref())?;
    let found = find(source.as_ref(), latest, url.as_deref(), &args.when)?;
    let mut o = io::stdout().lock();
    match args.format {
        SummaryFormat::Text => text(&mut o, &input, &found)?,
        SummaryFormat::Json => json(&mut o, &input, &found)?,
    }
    Ok(())
}

fn find(source: &dyn StateSource, latest: State, url: Option<&str>, when: &When) -> Result<Found> {
    let (start, point) = match when {
        When::Latest => (
            Start::After {
                stream: 0,
                state: latest,
            },
            Point::Latest,
        ),
        When::Since(Since::Time(ms)) => (Start::Time { timestamp_ms: *ms }, Point::Time(*ms)),
        When::Since(Since::File(path)) => {
            let after = after_file(path, source, url)?;
            let file = name(path);
            let point = match after {
                After::StateFile(_) => Point::StateFile(file),
                After::Header(_) => Point::Header(file),
                After::Snapshot(timestamp_ms) => Point::Snapshot(file, timestamp_ms),
            };
            (after.start(), point)
        }
    };
    let (state, exact) = match start {
        Start::After { state, .. } => (state, true),
        Start::Time { timestamp_ms } | Start::Snapshot { timestamp_ms } => {
            match locate(source, timestamp_ms)? {
                Located::At(state) => (state, false),
                Located::BeforeOldest(oldest) => bail!(
                    "{} is before the oldest diff the stream has, {} at {}",
                    Timestamp(timestamp_ms),
                    oldest.sequence,
                    Timestamp(oldest.timestamp_ms)
                ),
            }
        }
    };
    let since = plan(&[source], start, None)?
        .first()
        .map_or(latest, |step| step.from);
    Ok(Found {
        latest,
        point,
        state,
        exact,
        since,
    })
}

/// The file name of `path`, for the text output.
fn name(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    )
}

fn text(o: &mut impl Write, input: &Input, f: &Found) -> io::Result<()> {
    let (latest, state) = (f.latest, f.state);
    writeln!(
        o,
        "The latest diff of {} is {}, at {}, {} ago.",
        input.name(),
        latest.sequence,
        Timestamp(latest.timestamp_ms),
        ago(latest.timestamp_ms)
    )?;
    let at = Timestamp(state.timestamp_ms);
    let changes = match &f.point {
        Point::Latest => return Ok(()),
        Point::Time(t) => {
            let t = Timestamp(*t);
            writeln!(
                o,
                "The last diff at or before {t} is {}, at {at}.",
                state.sequence
            )?;
            format!("The changes made after {t} are in")
        }
        Point::Header(file) => {
            writeln!(
                o,
                "{file} is up to date with diff {}, at {at}, as its header says.",
                state.sequence
            )?;
            "The changes after it are in".to_owned()
        }
        Point::StateFile(file) => {
            writeln!(
                o,
                "{file} is the state of diff {}, at {at}.",
                state.sequence
            )?;
            "The changes after it are in".to_owned()
        }
        Point::Snapshot(file, t) => {
            writeln!(
                o,
                "{file} has the changes made up to {}, as its header says; the last diff at or before that time is \
                 {}, at {at}.",
                Timestamp(*t),
                state.sequence
            )?;
            format!(
                "A change can show up in a diff {} minutes after it is made, so the changes it may not have are in",
                LATE_MS / 60_000
            )
        }
    };
    let (first, diffs) = (f.since.sequence + 1, latest.sequence - f.since.sequence);
    match diffs {
        0 => writeln!(o, "There is no diff after it yet."),
        1 => writeln!(o, "{changes} diff {first}. Read it with --seq {first}."),
        _ => writeln!(
            o,
            "{changes} diffs {first} to {} ({} diffs). Read them with --seq {first}..",
            latest.sequence,
            count(diffs)
        ),
    }
}

fn json(o: &mut impl Write, input: &Input, f: &Found) -> io::Result<()> {
    o.write_all(br#"{"stream":"#)?;
    json_str(o, &input.name())?;
    write!(
        o,
        r#","latest_sequence":{},"latest_state":"#,
        f.latest.sequence
    )?;
    opt_time(o, Some(f.latest.timestamp_ms))?;
    write!(o, r#","sequence":{},"state":"#, f.state.sequence)?;
    opt_time(o, Some(f.state.timestamp_ms))?;
    writeln!(
        o,
        r#","exact":{},"read_from":{},"diffs":{}}}"#,
        f.exact,
        f.since.sequence + 1,
        f.latest.sequence - f.since.sequence
    )
}

#[cfg(test)]
mod tests {
    use std::{env, fs, process};

    use osmflow::osc::OscError;

    use super::*;

    /// Diffs `first..=last`, one a minute.
    struct Minutes(u64, u64);

    impl StateSource for Minutes {
        fn latest(&self) -> Result<State, OscError> {
            Ok(minute(self.1))
        }

        fn get(&self, sequence: u64) -> Result<Option<State>, OscError> {
            Ok((self.0..=self.1)
                .contains(&sequence)
                .then(|| minute(sequence)))
        }
    }

    fn minute(sequence: u64) -> State {
        State {
            sequence,
            timestamp_ms: i64::try_from(sequence).unwrap() * 60_000,
        }
    }

    fn find_in(when: &When) -> Result<Found> {
        let stream = Minutes(100, 1000);
        find(&stream, minute(1000), None, when)
    }

    #[test]
    fn a_time_finds_the_diff_before_it_and_reads_on_from_there() {
        let found = find_in(&When::Since(Since::Time(500 * 60_000 + 30_000))).unwrap();
        assert_eq!((found.state, found.since), (minute(500), minute(500)));
        assert!(!found.exact);
    }

    #[test]
    fn latest_and_state_files_are_exact() {
        let found = find_in(&When::Latest).unwrap();
        assert_eq!((found.state, found.since), (minute(1000), minute(1000)));
        let dir = env::temp_dir().join(format!("osmflow-seq-{}", process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.txt");
        fs::write(
            &path,
            "sequenceNumber=700\ntimestamp=1970-01-01T11\\:40\\:00Z\n",
        )
        .unwrap();
        let found = find_in(&When::Since(Since::File(path))).unwrap();
        fs::remove_dir_all(&dir).unwrap();
        assert!(found.exact);
        assert_eq!((found.state, found.since), (minute(700), minute(700)));
    }

    #[test]
    fn a_time_before_the_stream_is_an_error() {
        let e = find_in(&When::Since(Since::Time(50 * 60_000)))
            .err()
            .unwrap();
        assert!(e.to_string().contains("before the oldest diff"), "{e}");
    }
}

/// Sequence discovery against a mocked replication server, through the same uncached HTTP reads as a server URL.
#[cfg(all(test, feature = "http"))]
mod server_tests {
    use std::io;
    use std::sync::{Arc, Mutex};

    use osmflow::osc::{Fetch, OscError, ServerStates, parse_timestamp, path_sequence};

    use super::*;

    const MINUTE: i64 = 60_000;
    const DAY: i64 = 1440 * MINUTE;

    /// A replication server at `base`, with diffs `first..=last` whose states are at `time(n)`, keeping the URLs asked
    /// for.
    struct Server {
        base: &'static str,
        first: u64,
        last: u64,
        time: Box<dyn Fn(u64) -> i64 + Send + Sync>,
        asked: Mutex<Vec<String>>,
    }

    impl Server {
        fn new(
            base: &'static str,
            first: u64,
            last: u64,
            time: impl Fn(u64) -> i64 + Send + Sync + 'static,
        ) -> Arc<Self> {
            Arc::new(Self {
                base,
                first,
                last,
                time: Box::new(time),
                asked: Mutex::default(),
            })
        }

        fn state(&self, sequence: u64) -> State {
            State {
                sequence,
                timestamp_ms: (self.time)(sequence),
            }
        }

        /// Finds `when` on this server, as `osmflow seq` does with its URL.
        fn find(self: &Arc<Self>, when: &When) -> Result<Found> {
            let states = ServerStates::new(self.base, Mock(Arc::clone(self)));
            let latest = states.latest()?;
            find(&states, latest, Some(self.base), when)
        }

        /// The URLs asked for, and checks that each was a state file.
        fn asked(&self) -> usize {
            let asked = self.asked.lock().unwrap();
            assert!(asked.iter().all(|u| u.ends_with("state.txt")), "{asked:?}");
            asked.len()
        }
    }

    /// A [`Server`] as a [`Fetch`], which [`ServerStates`] owns.
    struct Mock(Arc<Server>);

    impl Fetch for Mock {
        fn fetch(&self, url: &str) -> io::Result<Option<Vec<u8>>> {
            let server = &self.0;
            server.asked.lock().unwrap().push(url.to_owned());
            let Some(relative) = url.strip_prefix(server.base) else {
                return Ok(None);
            };
            let sequence = if relative == "/state.txt" {
                server.last
            } else {
                match path_sequence(url) {
                    Some(n) if (server.first..=server.last).contains(&n) => n,
                    _ => return Ok(None),
                }
            };
            let time = Timestamp((server.time)(sequence))
                .to_string()
                .replace(':', "\\:");
            Ok(Some(
                format!("#written by a test\nsequenceNumber={sequence}\ntimestamp={time}\n")
                    .into_bytes(),
            ))
        }
    }

    fn at(s: &str) -> i64 {
        parse_timestamp(s).unwrap()
    }

    fn after(s: &str) -> When {
        When::Since(Since::Time(at(s)))
    }

    /// A PBF file of Monaco from Geofabrik, whose header gives sequence 2947 of
    /// `http://download.geofabrik.de/europe/monaco-updates`, at 2021-04-21T20:21:46Z.
    fn monaco() -> When {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../osmflow/fixtures/planetiler/monaco-latest.osm.pbf");
        When::Since(Since::File(path))
    }

    #[test]
    fn a_time_on_a_day_stream_reads_from_the_next_day() {
        let latest = at("2026-11-10T00:00:00Z");
        let day = Server::new("https://planet.test/replication/day", 1, 5172, move |n| {
            latest - i64::try_from(5172 - n).unwrap() * DAY
        });
        let found = day.find(&after("2026-01-01T00:00:00Z")).unwrap();
        assert_eq!(found.state, day.state(4859));
        assert_eq!(
            found.since,
            day.state(4859),
            "no day before it is read again"
        );
        assert!(!found.exact);
        assert!(day.asked() <= 12, "{} state files", day.asked());
    }

    #[test]
    fn a_time_on_a_minute_stream_reads_every_diff_whose_state_is_after_it() {
        // Minutes cut some seconds into the minute.
        let minute = Server::new(
            "https://planet.test/replication/minute",
            100,
            100_000,
            |n| i64::try_from(n * 60 + n * 7919 % 50).unwrap() * 1000,
        );
        for t in (0..100).map(|i| 200 * MINUTE + i * 997 * MINUTE + i * 1777) {
            let found = minute.find(&When::Since(Since::Time(t))).unwrap();
            let first_after = (100..=100_000)
                .find(|&n| minute.state(n).timestamp_ms > t)
                .unwrap();
            assert_eq!(found.since.sequence + 1, first_after, "time {t}");
            assert!(found.state.timestamp_ms <= t);
        }
    }

    #[test]
    fn an_extract_on_its_own_stream_is_exact() {
        // The header gives the server as `http://`.
        let geofabrik = Server::new(
            "https://download.geofabrik.de/europe/monaco-updates",
            2900,
            2960,
            |n| at("2021-04-21T20:21:46Z") + (i64::try_from(n).unwrap() - 2947) * DAY,
        );
        let found = geofabrik.find(&monaco()).unwrap();
        assert!(found.exact);
        assert_eq!(
            (found.state, found.since),
            (geofabrik.state(2947), geofabrik.state(2947))
        );
        // The latest state, and the one the file is up to.
        assert_eq!(geofabrik.asked(), 2);
    }

    #[test]
    fn a_file_on_another_stream_is_found_by_its_time_from_ten_minutes_before() {
        // Minute 2000 ends 30 seconds before the file's time.
        let minute = Server::new("https://planet.test/replication/minute", 1000, 3000, |n| {
            at("2021-04-21T20:21:46Z") + (i64::try_from(n).unwrap() - 2000) * MINUTE - 30_000
        });
        let found = minute.find(&monaco()).unwrap();
        assert!(!found.exact);
        assert_eq!(found.state, minute.state(2000));
        // The first minute after the file's time less ten minutes is 1991, at 9 minutes 30 seconds before it.
        assert_eq!(found.since, minute.state(1990));
    }

    #[test]
    fn an_extract_whose_diff_is_gone_and_a_wrong_url_are_errors() {
        let geofabrik = Server::new(
            "https://download.geofabrik.de/europe/monaco-updates",
            2950,
            2960,
            |n| i64::try_from(n).unwrap() * DAY,
        );
        let e = geofabrik.find(&monaco()).err().unwrap();
        assert!(e.to_string().contains("no longer has"), "{e}");

        let nowhere = Server::new("https://planet.test/nothing", 1, 1, |_| 0);
        let states = ServerStates::new("https://planet.test/elsewhere", Mock(nowhere));
        assert!(matches!(states.latest(), Err(OscError::NoStream { .. })));
    }
}
