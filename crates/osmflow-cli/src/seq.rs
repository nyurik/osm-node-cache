//! `osmflow seq`: the sequence of a replication stream at a time, or where a file leaves off, from its state files
//! alone.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::thread;

use anyhow::{Result, bail};
use osmflow::osc::{Located, Start, State, StateSource, Timestamp, locate, plan_with_late};

use crate::input::{
    After, Input, Overlap, Since, after_file, ago, parse_overlap, parse_since, stream_states,
};
use crate::output::{SummaryFormat, count, json_str, opt_time};

#[derive(Debug, clap::Args)]
pub struct Args {
    /// A replication directory or server URL.
    ///
    /// A directory laid out as the servers lay theirs out, or a server's URL, as
    /// `https://planet.openstreetmap.org/replication/minute`. A stream named `day`, `hour` or `minute` is read with
    /// the others beside it, to plan a catch-up through them all; a directory or URL that holds such streams, as
    /// `https://planet.openstreetmap.org/replication`, is read as its `minute` stream with the others.
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
    /// Plan the catch-up exactly, rather than estimate it.
    ///
    /// The estimate reads each stream's latest state only. The exact plan finds where each coarser diff ends among
    /// the finest, reading some state files around each place it changes streams, a few at a time, and gives the
    /// diffs of each leg.
    #[arg(long)]
    exact: bool,
    /// How late a change may show up in a diff: `3m`, `90s` or `1h`.
    ///
    /// After a planet file, the diffs start this much before its time, and the exact plan looks this far around each
    /// place it changes streams. A smaller overlap reads fewer state files and repeats fewer changes, and misses a
    /// change that shows up later than that.
    #[arg(long, value_name = "DURATION", default_value = "3m", value_parser = parse_overlap)]
    overlap: Overlap,
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
    /// Where a plan from the point starts.
    start: Start,
    /// Whether the catch-up is planned exactly, rather than estimated.
    planned: bool,
    /// How late a change may show up in a diff.
    late: Overlap,
    /// A catch-up through the streams beside this one, estimated from their latest states; empty with none beside it.
    catch_up: Vec<Leg>,
    /// The streams of the catch-up, coarsest first.
    streams: Vec<&'static str>,
}

/// The streams of a replication server's layout, coarsest first, with how long each one's diffs are.
const LAYOUT: [(&str, i64); 3] = [("day", 86_400_000), ("hour", 3_600_000), ("minute", 60_000)];

/// One of the streams of a server's layout, and its latest state.
#[derive(Debug, Clone, Copy)]
struct Layer {
    name: &'static str,
    interval_ms: i64,
    latest: State,
}

/// A run of diffs of one stream, in a catch-up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Leg {
    stream: &'static str,
    diffs: u64,
    /// When its last diff ends, in milliseconds since 1970.
    until_ms: i64,
    /// The first and last diffs, when the catch-up is planned exactly.
    sequences: Option<(u64, u64)>,
}

/// A state source that can be read from several threads.
type Source = Box<dyn StateSource + Send + Sync>;

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
    let late = args.overlap;
    let (stream, layout, (source, latest, url)) = open(input, args.cache.as_deref())?;
    let mut found = find(source.as_ref(), latest, url.as_deref(), &args.when, late)?;
    if let Some(from) = found.catch_up_from(late) {
        let layers = layers(&layout, &stream, (source, latest));
        // Up to the finest stream's latest state, which is the newest.
        if let [_, .., (finest, _)] = layers.as_slice() {
            if args.exact {
                found.catch_up =
                    exact_legs(&layers, stream_name(&layout, &stream), found.start, late)?;
                found.planned = true;
            } else {
                let plain: Vec<Layer> = layers.iter().map(|(layer, _)| *layer).collect();
                legs(
                    &plain,
                    from,
                    finest.latest.timestamp_ms,
                    &mut found.catch_up,
                );
            }
            found.streams = layers.iter().map(|(layer, _)| layer.name).collect();
        }
    }
    let input = stream;
    let mut o = io::stdout().lock();
    match args.format {
        SummaryFormat::Text => text(&mut o, &input, &found)?,
        SummaryFormat::Json => json(&mut o, &input, &found)?,
    }
    Ok(())
}

fn find(
    source: &dyn StateSource,
    latest: State,
    url: Option<&str>,
    when: &When,
    late: Overlap,
) -> Result<Found> {
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
    let since = plan_with_late(&[source], start, None, late.0)?
        .first()
        .map_or(latest, |step| step.from);
    Ok(Found {
        latest,
        point,
        state,
        exact,
        since,
        start,
        planned: false,
        late,
        catch_up: Vec::new(),
        streams: Vec::new(),
    })
}

impl Found {
    /// The time after which changes are to be read, for a catch-up: none for `latest`.
    fn catch_up_from(&self, late: Overlap) -> Option<i64> {
        match self.point {
            Point::Latest => None,
            Point::Time(t) => Some(t),
            Point::Snapshot(_, t) => Some(t - late.0),
            Point::Header(_) | Point::StateFile(_) => Some(self.state.timestamp_ms),
        }
    }
}

/// The layout name of a stream: `day`, `hour` or `minute`, as the last part of its path or URL.
fn layout_name(input: &Input) -> Option<&'static str> {
    let last = match input {
        Input::Url(url) => url.rsplit('/').next()?.to_owned(),
        Input::Dir(dir) => dir.file_name()?.to_str()?.to_owned(),
        Input::Pbf(_) | Input::Osc(_) | Input::State(_) => return None,
    };
    LAYOUT
        .iter()
        .find(|(name, _)| *name == last)
        .map(|(name, _)| *name)
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

/// A stream input as a stream: its state files, latest state and URL, with the streams of its server's layout,
/// coarsest first. A stream named `day`, `hour` or `minute` has the others beside it, a root of streams, which is
/// no stream itself, is read as its `minute` stream, and any other stream has none.
#[expect(clippy::type_complexity, reason = "used once, and taken apart at once")]
fn open(
    input: Input,
    cache: Option<&Path>,
) -> Result<(
    Input,
    Vec<(&'static str, i64, Input)>,
    (Source, State, Option<String>),
)> {
    match stream_states(&input, cache) {
        Ok(states) => {
            let layout = layout_name(&input)
                .and_then(|_| root_of(&input))
                .map_or_else(Vec::new, |root| layout_of(&root));
            Ok((input, layout, states))
        }
        Err(e) => match inside(&input, "minute").map(|m| (stream_states(&m, cache), m)) {
            Some((Ok(states), minute)) => Ok((minute, layout_of(&input), states)),
            _ => Err(e),
        },
    }
}

/// The streams of a server's layout inside `root`, coarsest first, with how long each one's diffs are.
fn layout_of(root: &Input) -> Vec<(&'static str, i64, Input)> {
    LAYOUT
        .iter()
        .filter_map(|&(name, interval)| Some((name, interval, inside(root, name)?)))
        .collect()
}

/// The streams of `layout` that exist, with their latest states and state files: `stream`'s are `main`, and the
/// others' latest states are read now, all at once.
fn layers(
    layout: &[(&'static str, i64, Input)],
    stream: &Input,
    main: (Source, State),
) -> Vec<(Layer, Source)> {
    let mut main = Some(main);
    let opened: Vec<Option<(Source, State)>> = thread::scope(|scope| {
        let reading: Vec<_> = layout
            .iter()
            .map(|(_, _, input)| {
                (input != stream).then(|| {
                    scope.spawn(move || stream_states(input, None).ok().map(|(s, l, _)| (s, l)))
                })
            })
            .collect();
        reading
            .into_iter()
            .map(|r| match r {
                Some(handle) => handle.join().ok().flatten(),
                None => main.take(),
            })
            .collect()
    });
    layout
        .iter()
        .zip(opened)
        .filter_map(|((name, interval_ms, _), opened)| {
            let (source, latest) = opened?;
            let layer = Layer {
                name,
                interval_ms: *interval_ms,
                latest,
            };
            Some((layer, source))
        })
        .collect()
}

/// The name of `stream` in `layout`, which the exact plan starts in for a point after one of its states.
fn stream_name(layout: &[(&'static str, i64, Input)], stream: &Input) -> Option<&'static str> {
    layout
        .iter()
        .find(|(_, _, i)| i == stream)
        .map(|(name, _, _)| *name)
}

/// The legs of the exact plan from `start` through `layers`, whose stream `stream` the start is in.
fn exact_legs(
    layers: &[(Layer, Source)],
    stream: Option<&'static str>,
    start: Start,
    late: Overlap,
) -> Result<Vec<Leg>> {
    let sources: Vec<&dyn StateSource> = layers
        .iter()
        .map(|(_, source)| source.as_ref() as &dyn StateSource)
        .collect();
    let start = match start {
        Start::After { state, .. } => Start::After {
            stream: layers
                .iter()
                .position(|(layer, _)| Some(layer.name) == stream)
                .unwrap_or(layers.len() - 1),
            state,
        },
        start @ (Start::Time { .. } | Start::Snapshot { .. }) => start,
    };
    Ok(plan_with_late(&sources, start, None, late.0)?
        .iter()
        .map(|step| Leg {
            stream: layers[step.stream].0.name,
            diffs: step.to.sequence - step.from.sequence,
            until_ms: step.to.timestamp_ms,
            sequences: Some((step.from.sequence + 1, step.to.sequence)),
        })
        .collect())
}

/// Appends the legs of a catch-up from `from` to `to`, in milliseconds since 1970, through `layers`, coarsest first:
/// finer diffs up to where a coarser diff starts, as many coarser ones as fit, and finer ones again. The states of a
/// coarser stream fall on its interval, so its diffs are counted exactly; the finest are about one an interval.
fn legs(layers: &[Layer], from: i64, to: i64, out: &mut Vec<Leg>) {
    if from >= to {
        return;
    }
    let Some((layer, finer)) = layers.split_first() else {
        return;
    };
    let interval = layer.interval_ms;
    if finer.is_empty() {
        let diffs = u64::try_from((to - from + interval - 1) / interval).unwrap_or(0);
        out.push(Leg {
            stream: layer.name,
            diffs,
            until_ms: to,
            sequences: None,
        });
        return;
    }
    let first = from.div_euclid(interval) * interval
        + if from.rem_euclid(interval) == 0 {
            0
        } else {
            interval
        };
    let last = to.min(layer.latest.timestamp_ms).div_euclid(interval) * interval;
    if last > first {
        legs(finer, from, first, out);
        out.push(Leg {
            stream: layer.name,
            diffs: u64::try_from((last - first) / interval).unwrap_or(0),
            until_ms: last,
            sequences: None,
        });
        legs(finer, last, to, out);
    } else {
        legs(finer, from, to, out);
    }
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
                "A change can show up in a diff {} after it is made, so the changes it may not have are in",
                f.late.words()
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
    catch_up(o, &f.streams, &f.catch_up, f.planned)
}

/// The legs of a catch-up through the server's `streams`, if it has more than one.
fn catch_up(o: &mut impl Write, streams: &[&str], legs: &[Leg], planned: bool) -> io::Result<()> {
    let Some((last, rest)) = streams.split_last() else {
        return Ok(());
    };
    if rest.is_empty() || legs.is_empty() {
        return Ok(());
    }
    let total: u64 = legs.iter().map(|l| l.diffs).sum();
    writeln!(
        o,
        "Through the {} and {last} streams, catching up to the latest diff takes {}{} diffs, in this order:",
        rest.join(", "),
        if planned { "" } else { "about " },
        count(total)
    )?;
    let counts: Vec<String> = legs.iter().map(|l| count(l.diffs)).collect();
    let width = counts.iter().map(String::len).max().unwrap_or(0);
    let stream_width = legs.iter().map(|l| l.stream.len()).max().unwrap_or(0);
    for (leg, n) in legs.iter().zip(&counts) {
        write!(o, "  {n:>width$} {:stream_width$} diffs", leg.stream)?;
        if let Some((first, last)) = leg.sequences {
            write!(o, ", --seq {first}..{last}")?;
        }
        writeln!(o, ", up to {}", Timestamp(leg.until_ms))?;
    }
    if !planned {
        writeln!(
            o,
            "The day and hour diffs are counted exactly, and the minutes at one a minute; --exact plans them."
        )?;
    }
    Ok(())
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
    write!(
        o,
        r#","exact":{},"read_from":{},"diffs":{},"catch_up_planned":{},"catch_up":["#,
        f.exact,
        f.since.sequence + 1,
        f.latest.sequence - f.since.sequence,
        f.planned
    )?;
    for (i, leg) in f.catch_up.iter().enumerate() {
        if i > 0 {
            o.write_all(b",")?;
        }
        write!(
            o,
            r#"{{"stream":"{}","diffs":{},"until":"#,
            leg.stream, leg.diffs
        )?;
        opt_time(o, Some(leg.until_ms))?;
        if let Some((first, last)) = leg.sequences {
            write!(o, r#","first":{first},"last":{last}"#)?;
        }
        o.write_all(b"}")?;
    }
    writeln!(o, "]}}")
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
        find(&stream, minute(1000), None, when, Overlap::default())
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

    const HOUR: i64 = 3_600_000;
    const DAY: i64 = 24 * HOUR;

    fn layer(name: &'static str, interval_ms: i64, latest_ms: i64) -> Layer {
        let latest = State {
            sequence: 1,
            timestamp_ms: latest_ms,
        };
        Layer {
            name,
            interval_ms,
            latest,
        }
    }

    #[test]
    fn a_catch_up_steps_up_to_days_and_down_to_minutes() {
        // From 10 days, 5 hours and 7 minutes back, to minute 40 of hour 3 of the latest day.
        let (day, latest) = (100 * DAY, 100 * DAY + 3 * HOUR + 40 * 60_000);
        let layers = [
            layer("day", DAY, day),
            layer("hour", HOUR, day + 3 * HOUR),
            layer("minute", 60_000, latest),
        ];
        let mut legs_found = Vec::new();
        legs(
            &layers,
            90 * DAY - 5 * HOUR - 7 * 60_000,
            latest,
            &mut legs_found,
        );
        let shape: Vec<_> = legs_found.iter().map(|l| (l.stream, l.diffs)).collect();
        assert_eq!(
            shape,
            [
                ("minute", 7),
                ("hour", 5),
                ("day", 10),
                ("hour", 3),
                ("minute", 40)
            ]
        );
        assert_eq!(legs_found.last().unwrap().until_ms, latest);
        // Within an hour, only minutes; at a day's end exactly, days and no finer lead-in.
        let mut short = Vec::new();
        legs(&layers, latest - 20 * 60_000, latest, &mut short);
        assert_eq!(
            short
                .iter()
                .map(|l| (l.stream, l.diffs))
                .collect::<Vec<_>>(),
            [("minute", 20)]
        );
        let mut exact = Vec::new();
        legs(&layers, 99 * DAY, latest, &mut exact);
        assert_eq!(exact[0].stream, "day");
    }

    #[test]
    fn a_root_of_streams_is_read_as_its_minute_stream_with_the_others() {
        let root = env::temp_dir().join(format!("osmflow-seq-root-{}", process::id()));
        for (name, sequence, time) in [
            ("day", 5172, "2026-11-10T00\\:00\\:00Z"),
            ("hour", 124_124, "2026-11-10T03\\:00\\:00Z"),
            ("minute", 7_366_177, "2026-11-10T03\\:46\\:50Z"),
        ] {
            fs::create_dir_all(root.join(name)).unwrap();
            fs::write(
                root.join(name).join("state.txt"),
                format!("sequenceNumber={sequence}\ntimestamp={time}\n"),
            )
            .unwrap();
        }
        let (stream, layout, (source, latest, _)) = open(Input::Dir(root.clone()), None).unwrap();
        assert_eq!(stream, Input::Dir(root.join("minute")));
        assert_eq!(latest.sequence, 7_366_177);
        let names: Vec<_> = layers(&layout, &stream, (source, latest))
            .iter()
            .map(|(l, _)| l.name)
            .collect();
        assert_eq!(names, ["day", "hour", "minute"]);
        // A stream of the layout has the others beside it.
        let (_, beside, _) = open(Input::Dir(root.join("hour")), None).unwrap();
        assert_eq!(beside.len(), 3);
        fs::remove_dir_all(&root).unwrap();
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
    use std::sync::{Arc, Mutex};
    use std::{env, fs, io, process};

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
            self.find_late(when, Overlap::default())
        }

        /// [`find`](Self::find), allowing changes to be `late`.
        fn find_late(self: &Arc<Self>, when: &When, late: Overlap) -> Result<Found> {
            let states = ServerStates::new(self.base, Mock(Arc::clone(self)));
            let latest = states.latest()?;
            find(&states, latest, Some(self.base), when, late)
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
    fn a_file_on_another_stream_is_found_by_its_time_from_the_overlap_before() {
        // Minute 2000 ends 30 seconds before the file's time.
        let minute = Server::new("https://planet.test/replication/minute", 1000, 3000, |n| {
            at("2021-04-21T20:21:46Z") + (i64::try_from(n).unwrap() - 2000) * MINUTE - 30_000
        });
        let found = minute.find(&monaco()).unwrap();
        assert!(!found.exact);
        assert_eq!(found.state, minute.state(2000));
        // The first minute after the file's time less 3 minutes is 1998, at 2 minutes 30 seconds before it.
        assert_eq!(found.since, minute.state(1997));
        // With an overlap of 10 minutes, 1991, at 9 minutes 30 seconds before it.
        let found = minute.find_late(&monaco(), Overlap(10 * MINUTE)).unwrap();
        assert_eq!(found.since, minute.state(1990));
    }

    #[test]
    fn a_state_file_of_another_stream_is_an_error() {
        let day = Server::new("https://planet.test/replication/day", 1, 5172, |n| {
            i64::try_from(n).unwrap() * DAY
        });
        let dir = env::temp_dir().join(format!("osmflow-seq-state-{}", process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.txt");
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
                Timestamp(5000 * DAY).to_string().replace(':', "\\:")
            ),
        )
        .unwrap();
        assert!(day.find(&When::Since(Since::File(path))).unwrap().exact);
        fs::remove_dir_all(&dir).unwrap();
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
