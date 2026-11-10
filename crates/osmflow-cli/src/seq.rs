//! `osmflow seq`: the sequence of a replication stream at a time, or where a file leaves off, from its state files
//! alone.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::{panic, thread};

use anyhow::{Result, bail};
use osmflow::Timestamp;
use osmflow::osc::{COMMIT_DELAY, Located, Start, State, StateSource, locate, plan};

use crate::input::{
    After, Input, Since, after_file, ago, is_no_stream, parse_since, stream_states,
};
use crate::output::{SummaryFormat, count, json_str};

#[derive(Debug, clap::Args)]
pub struct Args {
    /// A replication directory or server URL.
    ///
    /// A directory laid out as the servers lay theirs out, or a server's URL: one stream, as
    /// `https://planet.openstreetmap.org/replication/minute`, or a root of `day`, `hour` and `minute` streams, as
    /// `https://planet.openstreetmap.org/replication`, which `--stream` chooses from.
    input: PathBuf,
    /// A time, a PBF file, a state file, or `latest`.
    ///
    /// A time is `2026-11-09T21:00:00Z` or `2026-11-09`. A PBF file gives the sequence in its header when its base
    /// URL is this stream's, as a Geofabrik extract's is, and otherwise its timestamp, as a planet file's does.
    #[arg(value_parser = parse_when)]
    when: When,
    /// Where to keep a server's downloaded state files.
    ///
    /// Without it, the few state files the search reads are downloaded each time. The streams beside it, which a
    /// catch-up reads, are read without it.
    #[arg(long, value_name = "DIR")]
    cache: Option<PathBuf>,
    /// Which stream to search, for a server laid out as `day`, `hour` and `minute` streams.
    ///
    /// Default: `auto` when the input is the directory or URL holding the three streams, and only the stream given
    /// otherwise.
    #[arg(long, value_enum)]
    stream: Option<Streams>,
    /// The output format.
    #[arg(short, long, value_enum, default_value_t)]
    format: SummaryFormat,
}

/// The streams to read, of `--stream`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum Streams {
    /// Search the `minute` stream, or the stream given, and also plan the fewest diffs to catch up through all three
    /// streams. Reads some 35 state files more, about 12 s more on the planet's server.
    Auto,
    /// Search the `day` stream only.
    Day,
    /// Search the `hour` stream only.
    Hour,
    /// Search the `minute` stream only.
    Minute,
}

impl Streams {
    /// The name of the one stream chosen, as the layout names it: `None` for `auto`.
    fn name(self) -> Option<&'static str> {
        match self {
            Self::Auto => None,
            Self::Day => Some("day"),
            Self::Hour => Some("hour"),
            Self::Minute => Some("minute"),
        }
    }
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
    /// The diff before the first to read so that no change after the point is missed.
    since: State,
    /// Where a plan from the point starts.
    start: Start,
}

impl Found {
    /// Whether `state` is exactly where the point is, rather than found by time.
    fn exact(&self) -> bool {
        matches!(self.start, Start::After { .. })
    }
}

/// What was given, for the text output.
enum Point {
    Latest,
    Time(Timestamp),
    /// A PBF file whose header gives its diff on this stream.
    Header(String),
    StateFile(String),
    /// A PBF file whose header gives a time only, as a planet file's does, or a diff of another stream.
    Snapshot(String, Timestamp),
}

/// A state source that can be read from several threads.
type Source = Box<dyn StateSource + Send + Sync>;

/// A stream opened for searching: its input, state files, latest state, and its server's URL.
struct Opened {
    input: Input,
    source: Source,
    latest: State,
    url: Option<String>,
}

/// The streams of a replication server's layout, coarsest first.
const LAYOUT: [&str; 3] = ["day", "hour", "minute"];

/// A stream of a server's layout.
struct Member {
    name: &'static str,
    input: Input,
}

/// The streams of a layout a catch-up reads through, and which of them was searched.
struct Layout {
    members: Vec<Member>,
    /// The index in `members` of the stream searched.
    searched: usize,
}

/// A catch-up from the point through the streams of the layout, as the plan reads it.
struct CatchUp {
    /// The streams it reads from, coarsest first.
    streams: Vec<&'static str>,
    legs: Vec<Leg>,
}

/// A run of diffs of one stream, in a catch-up.
struct Leg {
    stream: &'static str,
    first: u64,
    last: u64,
    /// When its last diff ends.
    until: Timestamp,
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
    let (opened, layout) = open(&input, args.stream, args.cache.as_deref())?;
    let Opened {
        input,
        source,
        latest,
        url,
    } = opened;
    let found = find(source.as_ref(), latest, url.as_deref(), &args.when)?;
    let catch_up = match layout {
        Some(layout) if !matches!(found.point, Point::Latest) => {
            catch_up(&layout, source, found.start)?
        }
        _ => None,
    };
    let mut o = io::stdout().lock();
    match args.format {
        SummaryFormat::Text => text(&mut o, &input, &found, catch_up.as_ref())?,
        SummaryFormat::Json => json(&mut o, &input, &found, catch_up.as_ref())?,
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
        When::Since(Since::Time(at)) => (Start::Time { at: *at }, Point::Time(*at)),
        When::Since(Since::File(path)) => {
            let after = after_file(path, source, url)?;
            let file = name(path);
            let point = match after {
                After::StateFile(_) => Point::StateFile(file),
                After::Header(_) => Point::Header(file),
                After::Snapshot(at) => Point::Snapshot(file, at),
            };
            (after.start(), point)
        }
    };
    let state = match start {
        Start::After { state, .. } => state,
        Start::Time { at } | Start::Snapshot { at } => match locate(source, at)? {
            Located::At(state) => state,
            Located::BeforeOldest(oldest) => bail!(
                "{at} is before the oldest diff the stream has, {} at {}",
                oldest.sequence,
                oldest.timestamp
            ),
        },
    };
    let since = plan(&[source], start, None)?
        .first()
        .map_or(latest, |step| step.from);
    Ok(Found {
        latest,
        point,
        state,
        since,
        start,
    })
}

/// The name of the layout a stream has, `day`, `hour` or `minute`, as the last part of its path or URL.
fn layout_name(input: &Input) -> Option<&'static str> {
    let last = match input {
        Input::Url(url) => url.rsplit('/').next(),
        Input::Dir(dir) => dir.file_name().and_then(|n| n.to_str()),
        Input::Pbf(_) | Input::Osc(_) | Input::State(_) => None,
    };
    LAYOUT.into_iter().find(|&name| last == Some(name))
}

/// The stream `name` inside `root`.
fn inside(root: &Input, name: &str) -> Option<Input> {
    match root {
        Input::Url(url) => Some(Input::Url(format!("{url}/{name}"))),
        Input::Dir(dir) => Some(Input::Dir(dir.join(name))),
        Input::Pbf(_) | Input::Osc(_) | Input::State(_) => None,
    }
}

/// The root of a stream named by the layout, which holds it and the others.
fn root_of(stream: &Input) -> Option<Input> {
    match stream {
        Input::Url(url) => Some(Input::Url(url.rsplit_once('/')?.0.to_owned())),
        Input::Dir(dir) => Some(Input::Dir(dir.parent()?.to_owned())),
        Input::Pbf(_) | Input::Osc(_) | Input::State(_) => None,
    }
}

/// The streams of the layout inside `root`, coarsest first.
fn members(root: &Input) -> Vec<Member> {
    LAYOUT
        .iter()
        .filter_map(|&name| {
            Some(Member {
                name,
                input: inside(root, name)?,
            })
        })
        .collect()
}

/// Opens a stream input for searching, with the layout a catch-up reads through, if `streams` asks for one.
///
/// A stream named by `streams` is the one of that name inside a root, or beside a stream the layout names. Otherwise
/// a root of streams, which is no stream itself, is searched as its `minute` stream with a catch-up through the
/// streams it holds, and a stream alone, unless `streams` is `auto` and the layout names it.
fn open(
    input: &Input,
    streams: Option<Streams>,
    cache: Option<&Path>,
) -> Result<(Opened, Option<Layout>)> {
    let opened = |input: Input| -> Result<Opened> {
        let (source, latest, url) = stream_states(&input, cache)?;
        Ok(Opened {
            input,
            source,
            latest,
            url,
        })
    };
    let in_root = |root: &Input| -> Result<(Opened, Option<Layout>)> {
        let minute = inside(root, "minute").expect("a directory or URL has streams inside it");
        let layout = Layout {
            members: members(root),
            searched: layout_index("minute"),
        };
        Ok((opened(minute)?, Some(layout)))
    };
    if let Some(name) = streams.and_then(Streams::name) {
        let root = match layout_name(input) {
            Some(_) => root_of(input).expect("a stream the layout names has a root"),
            None => input.clone(),
        };
        let stream = inside(&root, name).expect("a directory or URL has streams inside it");
        return Ok((opened(stream)?, None));
    }
    if is_root(input) {
        return in_root(input);
    }
    match opened(input.clone()) {
        Ok(stream) => {
            let layout = match (streams, layout_name(input)) {
                (None, _) => None,
                (Some(_), Some(name)) => Some(Layout {
                    members: members(
                        &root_of(input).expect("a stream the layout names has a root"),
                    ),
                    searched: layout_index(name),
                }),
                (Some(_), None) => bail!(
                    "--stream auto plans through the day, hour and minute streams, and {} is not one of them",
                    input.name()
                ),
            };
            Ok((stream, layout))
        }
        // A server's root of streams has no `state.txt` of its own.
        Err(e) if is_no_stream(&e) && matches!(input, Input::Url(_)) => in_root(input),
        Err(e) => Err(e),
    }
}

/// Where the layout has `name`.
fn layout_index(name: &str) -> usize {
    LAYOUT
        .iter()
        .position(|&n| n == name)
        .expect("a name of the layout")
}

/// Whether a directory is a root of streams: it holds a `minute` stream, and has no state of its own.
fn is_root(input: &Input) -> bool {
    matches!(input, Input::Dir(dir) if !dir.join("state.txt").exists() && dir.join("minute").is_dir())
}

/// The state files and latest state of a stream beside the one searched, or `None` if it is not there: a directory
/// that does not exist, or a server's stream without a `state.txt`.
fn open_member(member: &Member) -> Result<Option<(Source, State)>> {
    if let Input::Dir(dir) = &member.input
        && !dir.is_dir()
    {
        return Ok(None);
    }
    match stream_states(&member.input, None) {
        Ok((source, latest, _)) => Ok(Some((source, latest))),
        Err(e) if is_no_stream(&e) => Ok(None),
        Err(e) => Err(e.context(format!(
            "cannot read the {} stream {}",
            member.name,
            member.input.name()
        ))),
    }
}

/// The catch-up from `start` in the stream searched, read through `source`, through the streams of `layout` that are
/// there: `None` if the stream searched is the only one. The others' latest states are read at once.
fn catch_up(layout: &Layout, source: Source, start: Start) -> Result<Option<CatchUp>> {
    let opened: Vec<Option<Source>> = thread::scope(|scope| {
        let reading: Vec<_> = layout
            .members
            .iter()
            .enumerate()
            .map(|(i, member)| (i != layout.searched).then(|| scope.spawn(|| open_member(member))))
            .collect();
        let mut source = Some(source);
        reading
            .into_iter()
            .map(|reading| match reading {
                None => Ok(source.take()),
                Some(handle) => handle
                    .join()
                    .unwrap_or_else(|panic| panic::resume_unwind(panic))
                    .map(|opened| opened.map(|(source, _)| source)),
            })
            .collect::<Result<_>>()
    })?;
    // The stream searched is there, so it is after as many as are there before it.
    let searched = opened[..layout.searched]
        .iter()
        .filter(|s| s.is_some())
        .count();
    let streams: Vec<(&'static str, Source)> = layout
        .members
        .iter()
        .zip(opened)
        .filter_map(|(member, source)| Some((member.name, source?)))
        .collect();
    if streams.len() < 2 {
        return Ok(None);
    }
    let start = match start {
        Start::After { state, .. } => Start::After {
            stream: searched,
            state,
        },
        start @ (Start::Time { .. } | Start::Snapshot { .. }) => start,
    };
    let sources: Vec<&dyn StateSource> = streams
        .iter()
        .map(|(_, source)| source.as_ref() as &dyn StateSource)
        .collect();
    let legs = plan(&sources, start, None)?
        .iter()
        .map(|step| Leg {
            stream: streams[step.stream].0,
            first: step.from.sequence + 1,
            last: step.to.sequence,
            until: step.to.timestamp,
        })
        .collect();
    Ok(Some(CatchUp {
        streams: streams.iter().map(|(name, _)| *name).collect(),
        legs,
    }))
}

/// The file name of `path`, for the text output.
fn name(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    )
}

fn text(
    o: &mut impl Write,
    input: &Input,
    f: &Found,
    catch_up: Option<&CatchUp>,
) -> io::Result<()> {
    let (latest, state) = (f.latest, f.state);
    writeln!(
        o,
        "The latest diff of {} is {}, at {}, {} ago.",
        input.name(),
        latest.sequence,
        latest.timestamp,
        ago(latest.timestamp)
    )?;
    let at = state.timestamp;
    let changes = match &f.point {
        Point::Latest => return Ok(()),
        Point::Time(t) => {
            let t = *t;
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
                *t, state.sequence
            )?;
            format!(
                "A change can show up in a diff {} minutes after it is made, so the changes it may not have are in",
                COMMIT_DELAY.as_secs() / 60
            )
        }
    };
    let (first, diffs) = (f.since.sequence + 1, latest.sequence - f.since.sequence);
    match diffs {
        0 => writeln!(o, "There is no diff after it yet.")?,
        1 => writeln!(o, "{changes} diff {first}. Read it with --seq {first}.")?,
        _ => writeln!(
            o,
            "{changes} diffs {first} to {} ({} diffs). Read them with --seq {first}..",
            latest.sequence,
            count(diffs)
        )?,
    }
    match catch_up {
        Some(catch_up) if !catch_up.legs.is_empty() => catch_up_text(o, catch_up),
        _ => Ok(()),
    }
}

/// The legs of a catch-up through the server's streams.
fn catch_up_text(o: &mut impl Write, catch_up: &CatchUp) -> io::Result<()> {
    let Some((last, rest)) = catch_up.streams.split_last() else {
        return Ok(());
    };
    let legs = &catch_up.legs;
    let total: u64 = legs.iter().map(|l| l.last + 1 - l.first).sum();
    writeln!(
        o,
        "Through the {} and {last} streams, catching up to the latest diff takes {} diffs, in this order:",
        rest.join(", "),
        count(total)
    )?;
    let counts: Vec<String> = legs.iter().map(|l| count(l.last + 1 - l.first)).collect();
    let width = counts.iter().map(String::len).max().unwrap_or(0);
    let stream_width = legs.iter().map(|l| l.stream.len()).max().unwrap_or(0);
    for (leg, n) in legs.iter().zip(&counts) {
        writeln!(
            o,
            "  {n:>width$} {:stream_width$} diffs, --seq {}..{}, up to {}",
            leg.stream, leg.first, leg.last, leg.until
        )?;
    }
    Ok(())
}

fn json(
    o: &mut impl Write,
    input: &Input,
    f: &Found,
    catch_up: Option<&CatchUp>,
) -> io::Result<()> {
    o.write_all(br#"{"stream":"#)?;
    json_str(o, &input.name())?;
    write!(
        o,
        r#","latest_sequence":{},"latest_state":"{}","sequence":{},"state":"{}","exact":{},"read_from":{},"diffs":{},"catch_up":["#,
        f.latest.sequence,
        f.latest.timestamp,
        f.state.sequence,
        f.state.timestamp,
        f.exact(),
        f.since.sequence + 1,
        f.latest.sequence - f.since.sequence,
    )?;
    for (i, leg) in catch_up.iter().flat_map(|c| &c.legs).enumerate() {
        if i > 0 {
            o.write_all(b",")?;
        }
        write!(
            o,
            r#"{{"stream":"{}","diffs":{},"first":{},"last":{},"until":"{}"}}"#,
            leg.stream,
            leg.last + 1 - leg.first,
            leg.first,
            leg.last,
            leg.until
        )?;
    }
    writeln!(o, "]}}")
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::ops::RangeInclusive;

    use osmflow::osc::{OscError, sequence_path};

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
            timestamp: Timestamp::from_millis(i64::try_from(sequence).unwrap() * 60_000),
        }
    }

    fn find_in(when: &When) -> Result<Found> {
        let stream = Minutes(100, 1000);
        find(&stream, minute(1000), None, when)
    }

    #[test]
    fn a_time_finds_the_diff_before_it_and_reads_on_from_there() {
        let found = find_in(&When::Since(Since::Time(Timestamp::from_millis(
            500 * 60_000 + 30_000,
        ))))
        .unwrap();
        assert_eq!((found.state, found.since), (minute(500), minute(500)));
        assert!(!found.exact());
    }

    #[test]
    fn latest_and_state_files_are_exact() {
        let found = find_in(&When::Latest).unwrap();
        assert_eq!((found.state, found.since), (minute(1000), minute(1000)));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.txt");
        fs::write(
            &path,
            "sequenceNumber=700\ntimestamp=1970-01-01T11\\:40\\:00Z\n",
        )
        .unwrap();
        let found = find_in(&When::Since(Since::File(path))).unwrap();
        assert!(found.exact());
        assert_eq!((found.state, found.since), (minute(700), minute(700)));
    }

    const HOUR: i64 = 3_600_000;

    /// Writes a stream into `dir`: the state of each of `sequences` at `time(n)`, and the latest as `state.txt`.
    fn write_stream(dir: &Path, sequences: RangeInclusive<u64>, time: impl Fn(u64) -> i64) {
        let escaped = |n| {
            Timestamp::from_millis(time(n))
                .to_string()
                .replace(':', "\\:")
        };
        for n in sequences.clone() {
            let path = dir.join(format!("{}.state.txt", sequence_path(n)));
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(
                &path,
                format!("sequenceNumber={n}\ntimestamp={}\n", escaped(n)),
            )
            .unwrap();
        }
        let last = *sequences.end();
        fs::write(
            dir.join("state.txt"),
            format!("sequenceNumber={last}\ntimestamp={}\n", escaped(last)),
        )
        .unwrap();
    }

    #[test]
    fn a_root_of_streams_is_planned_through_them_all() {
        // Three hours of minutes and hours, each state on its time, and a day stream with one state.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let t0 = 20_000 * 24 * HOUR;
        write_stream(&root.join("minute"), 0..=180, |n| {
            t0 + i64::try_from(n).unwrap() * 60_000
        });
        write_stream(&root.join("hour"), 0..=3, |n| {
            t0 + i64::try_from(n).unwrap() * HOUR
        });
        write_stream(&root.join("day"), 0..=0, |_| t0);

        let (opened, layout) = open(&Input::Dir(root.to_owned()), None, None).unwrap();
        assert_eq!(opened.input, Input::Dir(root.join("minute")));
        assert_eq!(opened.latest.sequence, 180);
        let layout = layout.unwrap();
        assert_eq!(
            layout.members.iter().map(|m| m.name).collect::<Vec<_>>(),
            LAYOUT
        );
        assert_eq!(layout.searched, 2);
        // Half an hour in: minutes to the first hour's end, then the two hours after it.
        let start = Start::Time {
            at: Timestamp::from_millis(t0 + 30 * 60_000),
        };
        let plan = catch_up(&layout, opened.source, start).unwrap().unwrap();
        assert_eq!(plan.streams, LAYOUT);
        let legs: Vec<_> = plan
            .legs
            .iter()
            .map(|l| (l.stream, l.first, l.last))
            .collect();
        assert_eq!(legs, [("minute", 31, 60), ("hour", 2, 3)]);
        // From a diff of the hour stream: the hours after it, then the minutes after the last hour.
        let (hour, layout) =
            open(&Input::Dir(root.join("hour")), Some(Streams::Auto), None).unwrap();
        let layout = layout.unwrap();
        assert_eq!(layout.searched, 1);
        let start = Start::After {
            stream: 0,
            state: hour.source.get(1).unwrap().unwrap(),
        };
        let plan = catch_up(&layout, hour.source, start).unwrap().unwrap();
        let legs: Vec<_> = plan
            .legs
            .iter()
            .map(|l| (l.stream, l.first, l.last))
            .collect();
        assert_eq!(legs, [("hour", 2, 3)]);
    }

    #[test]
    fn stream_chooses_one_stream_or_the_layout() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for name in LAYOUT {
            write_stream(&root.join(name), 0..=2, |n| {
                i64::try_from(n).unwrap() * HOUR
            });
        }
        let open_in = |path: &Path, streams| {
            let (opened, layout) = open(&Input::Dir(path.to_owned()), streams, None).unwrap();
            (opened.input, layout.map(|l| l.searched))
        };
        let (hour, minute) = (root.join("hour"), root.join("minute"));
        assert_eq!(open_in(root, None), (Input::Dir(minute.clone()), Some(2)));
        assert_eq!(
            open_in(root, Some(Streams::Auto)),
            (Input::Dir(minute.clone()), Some(2))
        );
        assert_eq!(
            open_in(root, Some(Streams::Hour)),
            (Input::Dir(hour.clone()), None)
        );
        // A stream is read alone, unless `auto` asks for the others beside it, or `--stream` names one of them.
        assert_eq!(open_in(&hour, None), (Input::Dir(hour.clone()), None));
        assert_eq!(
            open_in(&hour, Some(Streams::Auto)),
            (Input::Dir(hour.clone()), Some(1))
        );
        assert_eq!(
            open_in(&hour, Some(Streams::Day)),
            (Input::Dir(root.join("day")), None)
        );
        // A stream the layout does not name has none beside it.
        let other = root.join("other");
        write_stream(&other, 0..=2, |n| i64::try_from(n).unwrap() * HOUR);
        assert_eq!(open_in(&other, None), (Input::Dir(other.clone()), None));
        let e = open(&Input::Dir(other), Some(Streams::Auto), None)
            .err()
            .unwrap();
        assert!(e.to_string().contains("is not one of them"), "{e}");
    }

    #[test]
    fn a_stream_beside_that_cannot_be_read_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write_stream(&root.join("minute"), 0..=10, |n| {
            i64::try_from(n).unwrap() * 60_000
        });
        // The hour stream's state.txt is not a state file.
        fs::create_dir_all(root.join("hour")).unwrap();
        fs::write(root.join("hour").join("state.txt"), "not a state\n").unwrap();
        let (opened, layout) = open(&Input::Dir(root.to_owned()), None, None).unwrap();
        let start = Start::Time {
            at: Timestamp::EPOCH,
        };
        let e = catch_up(&layout.unwrap(), opened.source, start)
            .err()
            .unwrap();
        assert!(
            format!("{e:#}").contains("cannot read the hour stream"),
            "{e:#}"
        );
    }

    #[test]
    fn a_time_before_the_stream_is_an_error() {
        let e = find_in(&When::Since(Since::Time(Timestamp::from_millis(
            50 * 60_000,
        ))))
        .err()
        .unwrap();
        assert!(e.to_string().contains("before the oldest diff"), "{e}");
    }
}

/// Sequence discovery against a mocked replication server, through the same uncached HTTP reads as a server URL.
#[cfg(all(test, feature = "http"))]
mod server_tests {
    use std::sync::{Arc, Mutex};
    use std::{env, fs, io};

    use osmflow::osc::{Fetch, OscError, ServerStates, path_sequence};

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
                timestamp: Timestamp::from_millis((self.time)(sequence)),
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
            let time = Timestamp::from_millis((server.time)(sequence))
                .to_string()
                .replace(':', "\\:");
            Ok(Some(
                format!("#written by a test\nsequenceNumber={sequence}\ntimestamp={time}\n")
                    .into_bytes(),
            ))
        }
    }

    fn at(s: &str) -> i64 {
        s.parse::<Timestamp>().unwrap().as_millis()
    }

    fn after(s: &str) -> When {
        When::Since(Since::Time(Timestamp::from_millis(at(s))))
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
        assert!(!found.exact());
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
            let t = Timestamp::from_millis(t);
            let found = minute.find(&When::Since(Since::Time(t))).unwrap();
            let first_after = (100..=100_000)
                .find(|&n| minute.state(n).timestamp > t)
                .unwrap();
            assert_eq!(found.since.sequence + 1, first_after, "time {t}");
            assert!(found.state.timestamp <= t);
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
        assert!(found.exact());
        assert_eq!(
            (found.state, found.since),
            (geofabrik.state(2947), geofabrik.state(2947))
        );
        // The latest state, and the one the file is up to.
        assert_eq!(geofabrik.asked(), 2);
    }

    #[test]
    fn a_file_on_another_stream_is_found_by_its_time_from_the_commit_delay_before() {
        // Minute 2000 ends 30 seconds before the file's time.
        let minute = Server::new("https://planet.test/replication/minute", 1000, 3000, |n| {
            at("2021-04-21T20:21:46Z") + (i64::try_from(n).unwrap() - 2000) * MINUTE - 30_000
        });
        let found = minute.find(&monaco()).unwrap();
        assert!(!found.exact());
        assert_eq!(found.state, minute.state(2000));
        // The first minute after the file's time less 6 minutes is 1995, at 5 minutes 30 seconds before it.
        assert_eq!(found.since, minute.state(1994));
    }

    #[test]
    fn a_state_file_of_another_stream_is_an_error() {
        let day = Server::new("https://planet.test/replication/day", 1, 5172, |n| {
            i64::try_from(n).unwrap() * DAY
        });
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.txt");
        // A minute state: day 5000 is at another time.
        fs::write(
            &path,
            "sequenceNumber=5000\ntimestamp=1970-01-04T11\\:20\\:00Z\n",
        )
        .unwrap();
        let e = day
            .find(&When::Since(Since::File(path.clone())))
            .err()
            .unwrap();
        assert!(e.to_string().contains("not a state of this stream"), "{e}");
        // Day 5000's own state is.
        fs::write(
            &path,
            format!(
                "sequenceNumber=5000\ntimestamp={}\n",
                Timestamp::from_millis(5000 * DAY)
                    .to_string()
                    .replace(':', "\\:")
            ),
        )
        .unwrap();
        assert!(day.find(&When::Since(Since::File(path))).unwrap().exact());
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
