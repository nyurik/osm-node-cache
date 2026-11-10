//! `dump` and `stats` of osmChange input: one file, or the diffs of a replication stream in a directory or on a server.

use std::fs;
use std::io::{self, BufWriter, Write};
use std::ops::ControlFlow;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Result, bail};
use osmflow::osc::{Action, Change, Object, OscReader, State};

use crate::input::{Input, Stream, ago, local_sequences, remote_state, sibling_state};
use crate::output::{
    count, extent_json, extent_line, json_str, opt_time, size, time_span, time_span_json,
    timestamp, widen,
};
use crate::select::{ChangeArgs, Kind, Matcher, Threads};
use crate::write::Props;
use crate::{dump, stats};

/// What reading osmChange input gives, in order.
enum Event<'a> {
    /// A diff starts, with its own state: for every diff of a stream, and for a file beside its state file.
    Diff(&'a State),
    Change(&'a Change),
}

/// osmChange input, ready to read.
enum Opened {
    File(PathBuf),
    Stream(Stream),
}

/// Opens osmChange input: a file, which takes no stream flag, or a stream. Opened before anything is written, so a
/// flag that does not fit fails first.
fn open(input: &Input, changes: &ChangeArgs, threads: &Threads) -> Result<Opened> {
    if let Input::Osc(path) = input {
        if let Some(flag) = changes.stream_flag() {
            bail!(
                "{flag} is for replication streams, and {} is one osmChange file",
                input.name()
            );
        }
        return Ok(Opened::File(path.clone()));
    }
    Stream::open(input, changes.cache.as_deref(), threads.count()).map(Opened::Stream)
}

/// Reads the changes of `opened`, until `visit` breaks.
fn read(
    opened: &Opened,
    changes: &ChangeArgs,
    mut visit: impl FnMut(Event<'_>) -> Result<ControlFlow<()>>,
) -> Result<()> {
    let stream = match opened {
        Opened::Stream(stream) => stream,
        Opened::File(path) => {
            if let Some(state) = sibling_state(path)
                && visit(Event::Diff(&state))?.is_break()
            {
                return Ok(());
            }
            for change in OscReader::open(path)? {
                if visit(Event::Change(&change?))?.is_break() {
                    break;
                }
            }
            return Ok(());
        }
    };
    for diff in stream.chosen(changes)? {
        let diff = diff?;
        if visit(Event::Diff(&diff.state))?.is_break() {
            return Ok(());
        }
        for change in &diff.changes {
            if visit(Event::Change(change))?.is_break() {
                return Ok(());
            }
        }
    }
    Ok(())
}

/// Whether the flags choose `change`: by type, ID and tag as for PBF objects, and by action, changeset and user.
fn chosen(args: &dump::Args, matcher: &Matcher, change: &Change) -> bool {
    let kind = Kind::from(change.object.kind());
    args.filter.wants(kind)
        && matcher.matches(kind, change.object.id(), change.object.tags().iter())
        && args.changes.matches(change)
}

pub fn dump(args: &dump::Args, input: &Input) -> Result<()> {
    if let Some(flag) = args.pbf_flag() {
        bail!(
            "{flag} is for PBF files, and {} is osmChange input",
            input.name()
        );
    }
    let opened = open(input, &args.changes, &args.threads)?;
    let mut w = args.writer()?;
    let stream = match &opened {
        Opened::File(path) => {
            let bytes = fs::metadata(path).map_or(0, |m| m.len());
            w.comment(|o| write!(o, "file: {} ({})", path.display(), size(bytes)))?;
            false
        }
        Opened::Stream(_) => {
            w.comment(|o| write!(o, "replication: {}", input.name()))?;
            true
        }
    };
    let matcher = args.filter.matcher();
    let mut left = args.limit.unwrap_or(u64::MAX);
    let mut sequence = None;
    if left > 0 {
        read(&opened, &args.changes, |event| {
            let change = match event {
                Event::Diff(state) => {
                    sequence = stream.then_some(state.sequence);
                    w.comment(|o| {
                        write!(o, "sequence {} at ", state.sequence)?;
                        timestamp(o, state.timestamp_ms)
                    })?;
                    return Ok(ControlFlow::Continue(()));
                }
                Event::Change(change) => change,
            };
            if !chosen(args, &matcher, change) {
                return Ok(ControlFlow::Continue(()));
            }
            let o = &change.object;
            let mut props = if args.metadata {
                Props::of_meta(o.meta())
            } else {
                Props::default()
            };
            props.action = Some(change.action);
            props.sequence = sequence;
            let tags = o.tags().iter();
            match o {
                Object::Node(n) => w.node(n.id, n.location, &props, tags)?,
                Object::Way(way) => w.way(way.id, way.refs.iter().copied(), None, &props, tags)?,
                Object::Relation(r) => w.relation(r.id, r.members(), &props, tags)?,
            }
            left -= 1;
            Ok(if left == 0 {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            })
        })?;
    }
    w.end()?;
    Ok(())
}

fn action_index(action: Action) -> usize {
    match action {
        Action::Create => 0,
        Action::Modify => 1,
        Action::Delete => 2,
    }
}

/// Created and modified versions with tags, and their tags.
#[derive(Debug, Default, Clone, Copy)]
struct Tagged {
    tagged: u64,
    tags: u64,
}

/// What `stats` counts in osmChange input.
#[derive(Default)]
struct ChangeStats {
    /// Changes by type, then action.
    changes: [[u64; 3]; 3],
    /// The IDs of the objects changed, by [`Kind::index`], each once however often it changes.
    objects: [Distinct<i64>; 3],
    /// By type, the created and modified versions with tags, and their tags. A deletion is left out: the planet's
    /// diffs, and most others, give it no tags.
    tags: [Tagged; 3],
    /// Node latitudes and longitudes, smallest and largest, in 1e-7 degrees.
    lat: Option<(i32, i32)>,
    lon: Option<(i32, i32)>,
    time: Option<(i64, i64)>,
    max_version: Option<i32>,
    changesets: Distinct<i64>,
    uids: Distinct<i32>,
    /// User names, counted when the input has names without IDs.
    users: Distinct<Arc<str>>,
    /// The states of the diffs read: the first and the last, and how many.
    states: Option<(State, State)>,
    diffs: u64,
}

impl ChangeStats {
    fn add(&mut self, change: &Change) {
        let o = &change.object;
        let kind = Kind::from(o.kind());
        self.changes[kind.index()][action_index(change.action)] += 1;
        self.objects[kind.index()].insert(o.id());
        if change.action != Action::Delete {
            let t = &mut self.tags[kind.index()];
            let n = o.tags().len() as u64;
            t.tagged += u64::from(n > 0);
            t.tags += n;
        }
        if let Object::Node(n) = o
            && let Some(loc) = n.location
        {
            widen(&mut self.lat, loc.lat, loc.lat);
            widen(&mut self.lon, loc.lon, loc.lon);
        }
        let meta = o.meta();
        if let Some(ms) = meta.timestamp_ms {
            widen(&mut self.time, ms, ms);
        }
        self.max_version = self.max_version.max(meta.version);
        if let Some(changeset) = meta.changeset {
            self.changesets.insert(changeset);
        }
        if let Some(uid) = meta.uid {
            self.uids.insert(uid);
        }
        if meta.uid.is_none()
            && let Some(user) = &meta.user
        {
            self.users.insert(Arc::clone(user));
        }
    }

    fn diff(&mut self, state: &State) {
        self.diffs += 1;
        self.states = Some(match self.states {
            Some((first, _)) => (first, *state),
            None => (*state, *state),
        });
    }

    /// The number of distinct users: by ID, or by name when the input has names alone.
    fn user_count(&mut self) -> usize {
        self.uids.count() + self.users.count()
    }

    /// The changes of all types together, by action, and their tags.
    fn total(&self) -> ([u64; 3], Tagged) {
        let mut total = [0; 3];
        for by_action in &self.changes {
            for (t, n) in total.iter_mut().zip(by_action) {
                *t += n;
            }
        }
        let tags = self.tags.iter().fold(Tagged::default(), |acc, t| Tagged {
            tagged: acc.tagged + t.tagged,
            tags: acc.tags + t.tags,
        });
        (total, tags)
    }

    fn distinct(&mut self, kind: Kind) -> usize {
        self.objects[kind.index()].count()
    }

    /// The number of objects changed.
    fn object_count(&mut self) -> usize {
        self.objects.iter_mut().map(Distinct::count).sum()
    }
}

/// Values seen, to count each once: kept in a `Vec`, sorted and deduplicated each time it doubles, so it holds at most
/// about twice as many values as are distinct, and hashes none.
#[derive(Debug, Default)]
struct Distinct<T> {
    values: Vec<T>,
    /// How many values were distinct at the last deduplication.
    unique: usize,
}

impl<T: Ord> Distinct<T> {
    fn insert(&mut self, value: T) {
        self.values.push(value);
        if self.values.len() >= (2 * self.unique).max(1 << 16) {
            self.dedup();
        }
    }

    /// Sorts and deduplicates the values. They are a sorted run and the values added since, which a stable sort
    /// would merge, but with a buffer of half of them, for no measured gain.
    fn dedup(&mut self) {
        self.values.sort_unstable();
        self.values.dedup();
        self.unique = self.values.len();
    }

    /// The number of distinct values.
    fn count(&mut self) -> usize {
        if self.values.len() != self.unique {
            self.dedup();
        }
        self.unique
    }
}

pub fn stats(args: &stats::Args, input: &Input) -> Result<()> {
    let mut out = BufWriter::new(io::stdout().lock());
    let stream = matches!(input, Input::Dir(_) | Input::Url(_));
    if stream && !args.changes.chooses() {
        stream_state(&mut out, input, &args.changes, &args.threads, args.json())?;
        return Ok(out.flush()?);
    }
    let opened = open(input, &args.changes, &args.threads)?;
    let mut s = ChangeStats::default();
    read(&opened, &args.changes, |event| {
        match event {
            Event::Diff(state) => s.diff(state),
            Event::Change(change) if args.changes.matches(change) => s.add(change),
            Event::Change(_) => {}
        }
        Ok(ControlFlow::Continue(()))
    })?;
    let bytes = if let Input::Osc(path) = input {
        fs::metadata(path).ok().map(|m| m.len())
    } else {
        None
    };
    if args.json() {
        json(&mut out, input, bytes, &mut s)?;
    } else {
        text(&mut out, input, bytes, &mut s)?;
    }
    Ok(out.flush()?)
}

fn text(
    o: &mut impl Write,
    input: &Input,
    bytes: Option<u64>,
    s: &mut ChangeStats,
) -> io::Result<()> {
    match bytes {
        Some(bytes) => writeln!(o, "file          {} ({})", input.name(), size(bytes))?,
        None => writeln!(o, "stream        {}", input.name())?,
    }
    if let Some((first, last)) = s.states {
        if s.diffs > 1 {
            write!(
                o,
                "sequences     {} .. {} ({} diffs), ",
                first.sequence,
                last.sequence,
                count(s.diffs)
            )?;
            timestamp(o, first.timestamp_ms)?;
            o.write_all(b" .. ")?;
        } else {
            write!(o, "sequence      {} at ", last.sequence)?;
        }
        timestamp(o, last.timestamp_ms)?;
        writeln!(o)?;
    }
    extent_line(o, s.lat, s.lon)?;
    if let Some(span) = s.time {
        time_span(o, span)?;
        if let Some(version) = s.max_version {
            write!(o, "; highest version {version}")?;
        }
        writeln!(o)?;
    }
    for (name, n) in [
        ("changesets", s.changesets.count()),
        ("users", s.user_count()),
    ] {
        if n > 0 || s.object_count() == 0 {
            writeln!(o, "{name:13} {}", count(n as u64))?;
        } else {
            writeln!(o, "{name:13} not in the input")?;
        }
    }
    // The first four columns are of the versions created and modified: a deletion has no tags in most diffs.
    writeln!(o, "{:13} {:-^51}", "", " created or modified ")?;
    writeln!(
        o,
        "{:13} {:>12} {:>12} {:>12} {:>12} {:>12} {:>12}",
        "", "create", "modify", "tagged", "tags", "delete", "objects"
    )?;
    for kind in Kind::ALL {
        let name = kind.plural();
        let i = kind.index();
        text_row(o, name, s.changes[i], s.distinct(kind), s.tags[i])?;
    }
    let (changes, tags) = s.total();
    text_row(o, "total", changes, s.object_count(), tags)?;
    Ok(())
}

/// One row of the table of changes: the versions created and modified, with their tags, then the deletions and the
/// objects changed.
fn text_row(
    o: &mut impl Write,
    name: &str,
    [create, modify, delete]: [u64; 3],
    objects: usize,
    t: Tagged,
) -> io::Result<()> {
    writeln!(
        o,
        "{name:13} {:>12} {:>12} {:>12} {:>12} {:>12} {:>12}",
        count(create),
        count(modify),
        count(t.tagged),
        count(t.tags),
        count(delete),
        count(objects as u64)
    )
}

/// [`text_row`] as a JSON member.
fn json_row(
    o: &mut impl Write,
    name: &str,
    [create, modify, delete]: [u64; 3],
    objects: usize,
    t: Tagged,
) -> io::Result<()> {
    write!(
        o,
        r#","{name}":{{"create":{create},"modify":{modify},"delete":{delete},"objects":{objects},"tagged":{},"tags":{}}}"#,
        t.tagged, t.tags
    )
}

fn json(
    o: &mut impl Write,
    input: &Input,
    bytes: Option<u64>,
    s: &mut ChangeStats,
) -> io::Result<()> {
    o.write_all(br#"{"input":"#)?;
    json_str(o, &input.name())?;
    if let Some(bytes) = bytes {
        write!(o, r#","bytes":{bytes}"#)?;
    }
    if let Some((first, last)) = s.states {
        write!(
            o,
            r#","diffs":{},"first_sequence":{},"last_sequence":{},"first_state":"#,
            s.diffs, first.sequence, last.sequence
        )?;
        opt_time(o, Some(first.timestamp_ms))?;
        o.write_all(br#","last_state":"#)?;
        opt_time(o, Some(last.timestamp_ms))?;
    }
    for kind in Kind::ALL {
        let name = kind.plural();
        let i = kind.index();
        json_row(o, name, s.changes[i], s.distinct(kind), s.tags[i])?;
    }
    let (changes, tags) = s.total();
    json_row(o, "total", changes, s.object_count(), tags)?;
    o.write_all(br#","node_extent":"#)?;
    extent_json(o, s.lat, s.lon)?;
    o.write_all(b",")?;
    time_span_json(o, s.time)?;
    o.write_all(br#","max_version":"#)?;
    match s.max_version {
        Some(v) => write!(o, "{v}")?,
        None => o.write_all(b"null")?,
    }
    writeln!(
        o,
        r#","changesets":{},"users":{}}}"#,
        s.changesets.count(),
        s.user_count()
    )
}

/// The latest state of a stream, how long ago it was, and for a directory, the diffs it has.
fn stream_state(
    o: &mut impl Write,
    input: &Input,
    changes: &ChangeArgs,
    threads: &Threads,
    json: bool,
) -> Result<()> {
    let state = match (input, &changes.cache) {
        (Input::Url(url), None) => remote_state(url)?,
        _ => Stream::open(input, changes.cache.as_deref(), threads.count())?.latest()?,
    };
    let local = if let Input::Dir(dir) = input {
        Some(local_sequences(dir)?)
    } else {
        None
    };
    if json {
        o.write_all(br#"{"input":"#)?;
        json_str(o, &input.name())?;
        write!(
            o,
            r#","latest_sequence":{},"latest_state":"#,
            state.sequence
        )?;
        opt_time(o, Some(state.timestamp_ms))?;
        if let Some(local) = &local {
            write!(
                o,
                r#","local_diffs":{},"gaps":{}"#,
                local.len(),
                gaps(local)
            )?;
            for (key, value) in [("first_local", local.first()), ("last_local", local.last())] {
                match value {
                    Some(v) => write!(o, r#","{key}":{v}"#)?,
                    None => write!(o, r#","{key}":null"#)?,
                }
            }
        }
        writeln!(o, "}}")?;
        return Ok(());
    }
    writeln!(o, "stream        {}", input.name())?;
    write!(o, "latest        sequence {} at ", state.sequence)?;
    timestamp(o, state.timestamp_ms)?;
    writeln!(o, ", {} ago", ago(state.timestamp_ms))?;
    if let Some(local) = &local {
        match (local.first(), local.last()) {
            (Some(first), Some(last)) => writeln!(
                o,
                "local diffs   {}: {first} .. {last}, {} missing between",
                count(local.len() as u64),
                count(gaps(local))
            )?,
            _ => writeln!(o, "local diffs   none")?,
        }
    }
    Ok(())
}

/// The sequences missing between the first and the last of sorted `sequences`.
fn gaps(sequences: &[u64]) -> u64 {
    match (sequences.first(), sequences.last()) {
        (Some(first), Some(last)) => last - first + 1 - sequences.len() as u64,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::io::Cursor;
    use std::path::Path;

    use super::*;

    #[test]
    fn gaps_are_the_missing_sequences() {
        assert_eq!(gaps(&[]), 0);
        assert_eq!(gaps(&[5]), 0);
        assert_eq!(gaps(&[5, 6, 9]), 2);
    }

    #[test]
    fn changes_are_counted_by_type_action_and_object() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../osmflow/fixtures/osc/planet/minute/007/363/600.osc.gz");
        let mut s = ChangeStats::default();
        let mut total = 0;
        for change in OscReader::open(&path).unwrap() {
            s.add(&change.unwrap());
            total += 1;
        }
        let counted: u64 = s.changes.iter().flatten().sum();
        assert_eq!(counted, total);
        // Each object once, however many versions of it the diff has.
        let objects: HashSet<(Kind, i64)> = OscReader::open(&path)
            .unwrap()
            .map(|c| {
                let c = c.unwrap();
                (Kind::from(c.object.kind()), c.object.id())
            })
            .collect();
        assert!(
            objects.len() < usize::try_from(total).unwrap(),
            "the diff has objects changed twice"
        );
        assert_eq!(s.object_count(), objects.len());
        let nodes = objects.iter().filter(|(k, _)| *k == Kind::Node).count();
        assert_eq!(s.distinct(Kind::Node), nodes);
        // A planet diff has changesets and users.
        assert!(s.changesets.count() > 0 && s.uids.count() > 0);
        assert!(s.time.is_some() && s.max_version.is_some());
    }

    #[test]
    fn tags_are_counted_for_created_and_modified_versions_only() {
        let xml = r#"<osmChange version="0.6">
            <create>
                <node id="1" version="1" lat="1" lon="2"><tag k="a" v="1"/><tag k="b" v="2"/></node>
                <way id="2" version="1"><nd ref="1"/><tag k="highway" v="road"/></way>
            </create>
            <modify>
                <node id="1" version="2" lat="1" lon="2"/>
                <node id="3" version="4" lat="1" lon="2"><tag k="c" v="3"/></node>
            </modify>
            <delete><node id="4" version="2" lat="1" lon="2"><tag k="d" v="4"/></node></delete>
        </osmChange>"#;
        let mut s = ChangeStats::default();
        for change in OscReader::new(Cursor::new(xml)) {
            s.add(&change.unwrap());
        }
        let node = s.tags[Kind::Node.index()];
        assert_eq!((node.tagged, node.tags), (2, 3));
        let way = s.tags[Kind::Way.index()];
        assert_eq!((way.tagged, way.tags), (1, 1));
        let (changes, total) = s.total();
        assert_eq!(changes, [2, 2, 1]);
        assert_eq!((total.tagged, total.tags), (3, 4));
    }
}
