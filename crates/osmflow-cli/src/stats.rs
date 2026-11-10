//! `osmflow stats`: a summary of a PBF file (its header, blocks, object counts, ID ranges, extent, and with
//! `--metadata` its time span), of osmChange input, or of a replication stream's latest state.

use std::fs;
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use anyhow::{Context as _, Result};
use osmflow::osc::State;
use osmflow::{Block, Header, Info, PbfReader, Select, Timestamp};

use crate::changes;
use crate::input::{Input, ago};
use crate::output::{
    self, SummaryFormat, count, extent_json, extent_line, json_str, nanodegrees, opt_num, opt_str,
    opt_time, str_array, time_span, time_span_json, widen,
};
use crate::select::{ChangeArgs, Kind, Threads};

#[derive(Debug, clap::Args)]
pub struct Args {
    /// A PBF, osmChange or state file, or replication directory or URL.
    ///
    /// An osmChange file is `.osc` or `.osc.gz`, and a replication stream a directory laid out as the servers lay
    /// theirs out, or a server's URL. What the input is is told from its contents, not its name. A stream without
    /// --seq or --since shows only its latest state.
    input: PathBuf,
    /// The output format.
    #[arg(short, long, value_enum, default_value_t)]
    format: SummaryFormat,
    /// Also read PBF metadata: time span, versions, deletions.
    ///
    /// Metadata is much of what a block holds, so it is not read by default. osmChange input always shows it, as its
    /// parser reads it anyway.
    #[arg(long)]
    metadata: bool,
    #[command(flatten)]
    pub threads: Threads,
    #[command(flatten)]
    pub changes: ChangeArgs,
}

impl Args {
    /// Whether the summary is written as JSON.
    pub fn json(&self) -> bool {
        self.format == SummaryFormat::Json
    }
}

/// Counts of one object type.
#[derive(Debug, Clone, Copy)]
struct TypeStats {
    count: u64,
    /// Objects with at least one tag.
    tagged: u64,
    tags: u64,
    min_id: i64,
    max_id: i64,
}

impl Default for TypeStats {
    fn default() -> Self {
        Self {
            count: 0,
            tagged: 0,
            tags: 0,
            min_id: i64::MAX,
            max_id: i64::MIN,
        }
    }
}

impl TypeStats {
    fn add(&mut self, id: i64, tags: usize) {
        self.count += 1;
        self.tagged += u64::from(tags > 0);
        self.tags += tags as u64;
        self.min_id = self.min_id.min(id);
        self.max_id = self.max_id.max(id);
    }

    fn merge(&mut self, o: &Self) {
        self.count += o.count;
        self.tagged += o.tagged;
        self.tags += o.tags;
        self.min_id = self.min_id.min(o.min_id);
        self.max_id = self.max_id.max(o.max_id);
    }
}

/// The first and last object of a block, by type then ID, and whether the block is in that order inside.
#[derive(Debug, Clone, Copy)]
struct Span {
    blob: u64,
    first: (Kind, i64),
    last: (Kind, i64),
    ordered: bool,
}

#[derive(Debug, Default)]
struct Stats {
    blocks: u64,
    zlib: u64,
    blob_bytes: u64,
    raw_bytes: u64,
    strings: u64,
    string_bytes: u64,
    /// Groups by kind: plain nodes, dense nodes, ways, relations, empty.
    groups: [u64; 5],
    types: [TypeStats; 3],
    refs: u64,
    members: u64,
    /// Node latitudes and longitudes, smallest and largest, in 1e-7 degrees.
    lat: Option<(i32, i32)>,
    lon: Option<(i32, i32)>,
    /// Object timestamps, earliest and latest.
    time: Option<(Timestamp, Timestamp)>,
    max_version: i32,
    deleted: u64,
    spans: Vec<Span>,
}

impl Stats {
    /// The counts of all types together; its IDs are not set.
    fn total(&self) -> TypeStats {
        self.types
            .iter()
            .fold(TypeStats::default(), |acc, t| TypeStats {
                count: acc.count + t.count,
                tagged: acc.tagged + t.tagged,
                tags: acc.tags + t.tags,
                ..acc
            })
    }

    fn block(block: &Block) -> Self {
        let mut s = Self::default();
        let layout = block.layout();
        s.blocks = 1;
        s.zlib = u64::from(layout.zlib);
        s.blob_bytes = layout.blob_bytes as u64;
        s.raw_bytes = layout.raw_bytes as u64;
        s.strings = layout.strings as u64;
        s.string_bytes = layout.string_bytes as u64;
        for g in &layout.groups {
            s.groups[output::group_index(g.kind)] += 1;
        }
        let mut order = Order::default();
        for n in block.nodes() {
            s.types[Kind::Node.index()].add(n.id(), n.tags().len());
            s.info(n.info());
            let (lat, lon) = (n.decimicro_lat(), n.decimicro_lon());
            widen(&mut s.lat, lat, lat);
            widen(&mut s.lon, lon, lon);
            order.next(Kind::Node, n.id());
        }
        for w in block.ways() {
            s.types[Kind::Way.index()].add(w.id(), w.tags().len());
            s.info(w.info());
            s.refs += w.refs().len() as u64;
            order.next(Kind::Way, w.id());
        }
        for r in block.relations() {
            s.types[Kind::Relation.index()].add(r.id(), r.tags().len());
            s.info(r.info());
            s.members += r.members().len() as u64;
            order.next(Kind::Relation, r.id());
        }
        if let (Some(first), Some(last)) = (order.first, order.last) {
            s.spans.push(Span {
                blob: block.position().blob,
                first,
                last,
                ordered: order.ordered,
            });
        }
        s
    }

    fn info(&mut self, info: Option<Info<'_>>) {
        if let Some(i) = info {
            // A timestamp of 0 is a file leaving it out, as some writers do, not an edit of 1970.
            if i.timestamp != Timestamp::EPOCH {
                widen(&mut self.time, i.timestamp, i.timestamp);
            }
            self.max_version = self.max_version.max(i.version);
            self.deleted += u64::from(!i.visible);
        }
    }

    fn merge(&mut self, o: Self) {
        self.blocks += o.blocks;
        self.zlib += o.zlib;
        self.blob_bytes += o.blob_bytes;
        self.raw_bytes += o.raw_bytes;
        self.strings += o.strings;
        self.string_bytes += o.string_bytes;
        for (a, b) in self.groups.iter_mut().zip(o.groups) {
            *a += b;
        }
        for (a, b) in self.types.iter_mut().zip(&o.types) {
            a.merge(b);
        }
        self.refs += o.refs;
        self.members += o.members;
        for (mine, theirs) in [(&mut self.lat, o.lat), (&mut self.lon, o.lon)] {
            if let Some((low, high)) = theirs {
                widen(mine, low, high);
            }
        }
        if let Some((low, high)) = o.time {
            widen(&mut self.time, low, high);
        }
        self.max_version = self.max_version.max(o.max_version);
        self.deleted += o.deleted;
        self.spans.extend(o.spans);
    }

    /// The first blob whose objects are out of type-then-ID order, or `None` if every object is in order.
    fn first_unordered(&mut self) -> Option<u64> {
        self.spans.sort_by_key(|s| s.blob);
        let mut previous: Option<(Kind, i64)> = None;
        for span in &self.spans {
            if !span.ordered || previous.is_some_and(|p| key(p) >= key(span.first)) {
                return Some(span.blob);
            }
            previous = Some(span.last);
        }
        None
    }
}

/// An object's place in type-then-ID order.
fn key((kind, id): (Kind, i64)) -> (usize, i64) {
    (kind.index(), id)
}

/// Follows the objects of one block, to tell whether they are in type-then-ID order.
#[derive(Default)]
struct Order {
    first: Option<(Kind, i64)>,
    last: Option<(Kind, i64)>,
    ordered: bool,
}

impl Order {
    fn next(&mut self, kind: Kind, id: i64) {
        match self.last {
            None => {
                self.first = Some((kind, id));
                self.ordered = true;
            }
            Some(last) => self.ordered &= key(last) < key((kind, id)),
        }
        self.last = Some((kind, id));
    }
}

pub fn run(args: &Args) -> Result<()> {
    match Input::of(&args.input)? {
        Input::Pbf(path) => pbf(args, &path),
        Input::State(path) => state(args, &path),
        input @ (Input::Osc(_) | Input::Dir(_) | Input::Url(_)) => changes::stats(args, &input),
    }
}

/// A state file: its sequence and time, and how long ago that was.
fn state(args: &Args, path: &Path) -> Result<()> {
    let state = State::read(path)?;
    let mut o = io::stdout().lock();
    if args.json() {
        writeln!(
            o,
            r#"{{"sequence":{},"timestamp":"{}"}}"#,
            state.sequence, state.timestamp
        )?;
    } else {
        writeln!(
            o,
            "state         sequence {} at {}, {} ago",
            state.sequence,
            state.timestamp,
            ago(state.timestamp)
        )?;
    }
    Ok(())
}

fn pbf(args: &Args, path: &Path) -> Result<()> {
    args.changes.reject_pbf(path)?;
    let size = fs::metadata(path)
        .with_context(|| format!("cannot read {}", path.display()))?
        .len();
    let file = PbfReader::open(path).with_context(|| format!("cannot read {}", path.display()))?;
    let file = args.threads.apply(file)?;
    let total = Mutex::new(Stats::default());
    // Metadata is decoded only when asked for: it is much of the time of a full decode.
    let select = Select::all();
    let select = if args.metadata {
        select
    } else {
        select.without_metadata()
    };
    args.threads.unordered(&file, select, |block| {
        let s = Stats::block(&block);
        total
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .merge(s);
    })?;
    let mut stats = total.into_inner().unwrap_or_else(PoisonError::into_inner);
    let unordered = stats.first_unordered();
    let mut out = BufWriter::new(io::stdout().lock());
    let report = Report {
        path: &path.display().to_string(),
        size,
        header: file.header(),
        stats: &stats,
        unordered,
        metadata: args.metadata,
    };
    if args.json() {
        report.json(&mut out)?;
    } else {
        report.text(&mut out)?;
    }
    out.flush()?;
    Ok(())
}

struct Report<'a> {
    path: &'a str,
    size: u64,
    header: &'a Header,
    stats: &'a Stats,
    unordered: Option<u64>,
    /// Whether metadata was read.
    metadata: bool,
}

impl Report<'_> {
    fn text(&self, o: &mut impl Write) -> io::Result<()> {
        self.text_file(o)?;
        self.text_blocks(o)?;
        self.text_table(o)
    }

    /// The file, its header, and what its objects span: their extent, time and order.
    fn text_file(&self, o: &mut impl Write) -> io::Result<()> {
        let (h, s) = (self.header, self.stats);
        writeln!(
            o,
            "file          {} ({})",
            self.path,
            output::size(self.size)
        )?;
        if let Some(program) = &h.writing_program {
            writeln!(o, "written by    {program}")?;
        }
        writeln!(o, "features      {}", h.required_features.join(", "))?;
        if !h.optional_features.is_empty() {
            writeln!(o, "optional      {}", h.optional_features.join(", "))?;
        }
        if let Some(b) = h.bbox {
            o.write_all(b"header bbox   lon ")?;
            nanodegrees(o, b.left)?;
            o.write_all(b" .. ")?;
            nanodegrees(o, b.right)?;
            o.write_all(b", lat ")?;
            nanodegrees(o, b.bottom)?;
            o.write_all(b" .. ")?;
            nanodegrees(o, b.top)?;
            writeln!(o)?;
        }
        extent_line(o, s.lat, s.lon)?;
        // Planet files give a timestamp alone; Geofabrik's extracts the sequence of their own stream, and its URL.
        if h.replication_sequence.is_some() || h.replication_timestamp.is_some() {
            o.write_all(b"replication  ")?;
            if let Some(sequence) = h.replication_sequence {
                write!(o, " sequence {sequence}")?;
            }
            if let Some(at) = h.replication_timestamp {
                write!(o, " at {at}")?;
            }
            writeln!(o)?;
        }
        if let Some(url) = &h.replication_base_url {
            writeln!(o, "updates       {url}")?;
        }
        if let Some(span) = s.time {
            time_span(o, span)?;
            writeln!(o, "; highest version {}", s.max_version)?;
        } else if !self.metadata {
            writeln!(o, "timestamps    not read; --metadata reads them")?;
        }
        if s.deleted > 0 {
            writeln!(
                o,
                "deleted       {} versions of deleted objects",
                count(s.deleted)
            )?;
        }
        let claims = if h.is_sorted() {
            "the header says sorted"
        } else {
            "the header does not say sorted"
        };
        match self.unordered {
            None => writeln!(o, "order         by type, then ID ({claims})"),
            Some(blob) => writeln!(
                o,
                "order         not by type, then ID, from blob {blob} ({claims})"
            ),
        }
    }

    /// The blocks, and the references their ways and relations hold.
    fn text_blocks(&self, o: &mut impl Write) -> io::Result<()> {
        let s = self.stats;
        writeln!(
            o,
            "blocks        {} ({} zlib): {} in blobs, {} uncompressed; {} strings in {}",
            count(s.blocks),
            count(s.zlib),
            output::size(s.blob_bytes),
            output::size(s.raw_bytes),
            count(s.strings),
            output::size(s.string_bytes),
        )?;
        let groups: Vec<String> = s
            .groups
            .iter()
            .zip(output::GROUP_NAMES)
            .filter(|&(&n, _)| n > 0)
            .map(|(&n, name)| format!("{} {name}", count(n)))
            .collect();
        writeln!(o, "groups        {}", groups.join(", "))?;
        writeln!(o, "way refs      {} node IDs in all ways", count(s.refs))?;
        writeln!(
            o,
            "members       {} members of all relations",
            count(s.members)
        )
    }

    /// Objects by type: how many, how many have tags, how many tags, and their IDs.
    fn text_table(&self, o: &mut impl Write) -> io::Result<()> {
        writeln!(
            o,
            "{:13} {:>15} {:>15} {:>15}   IDs",
            "", "count", "tagged", "tags"
        )?;
        for (kind, t) in Kind::ALL.into_iter().zip(&self.stats.types) {
            let name = kind.plural();
            write!(
                o,
                "{name:13} {:>15} {:>15} {:>15}",
                count(t.count),
                count(t.tagged),
                count(t.tags)
            )?;
            if t.count > 0 {
                write!(o, "   {} .. {}", t.min_id, t.max_id)?;
            }
            writeln!(o)?;
        }
        let total = self.stats.total();
        writeln!(
            o,
            "{:13} {:>15} {:>15} {:>15}",
            "total",
            count(total.count),
            count(total.tagged),
            count(total.tags)
        )
    }

    fn json(&self, o: &mut impl Write) -> io::Result<()> {
        let (h, s) = (self.header, self.stats);
        o.write_all(br#"{"file":"#)?;
        json_str(o, self.path)?;
        write!(o, r#","bytes":{},"header":{{"#, self.size)?;
        o.write_all(br#""writing_program":"#)?;
        opt_str(o, h.writing_program.as_deref())?;
        o.write_all(br#","required_features":"#)?;
        str_array(o, &h.required_features)?;
        o.write_all(br#","optional_features":"#)?;
        str_array(o, &h.optional_features)?;
        o.write_all(br#","bbox":"#)?;
        match h.bbox {
            Some(b) => {
                o.write_all(b"[")?;
                for (i, nano) in [b.left, b.bottom, b.right, b.top].into_iter().enumerate() {
                    if i > 0 {
                        o.write_all(b",")?;
                    }
                    nanodegrees(o, nano)?;
                }
                o.write_all(b"]")?;
            }
            None => o.write_all(b"null")?,
        }
        o.write_all(br#","replication_sequence":"#)?;
        opt_num(o, h.replication_sequence)?;
        o.write_all(br#","replication_timestamp":"#)?;
        opt_time(o, h.replication_timestamp)?;
        o.write_all(br#","replication_base_url":"#)?;
        opt_str(o, h.replication_base_url.as_deref())?;
        write!(
            o,
            r#"}},"blocks":{},"zlib_blocks":{},"blob_bytes":{},"raw_bytes":{},"strings":{},"string_bytes":{}"#,
            s.blocks, s.zlib, s.blob_bytes, s.raw_bytes, s.strings, s.string_bytes
        )?;
        let [plain, dense, ways, relations, empty] = s.groups;
        write!(
            o,
            r#","groups":{{"nodes":{plain},"dense_nodes":{dense},"ways":{ways},"relations":{relations},"empty":{empty}}}"#
        )?;
        for (kind, t) in Kind::ALL.into_iter().zip(&s.types) {
            let name = kind.plural();
            write!(
                o,
                r#","{name}":{{"count":{},"tagged":{},"tags":{}"#,
                t.count, t.tagged, t.tags
            )?;
            if t.count > 0 {
                write!(o, r#","min_id":{},"max_id":{}"#, t.min_id, t.max_id)?;
            }
            o.write_all(b"}")?;
        }
        let total = s.total();
        write!(
            o,
            r#","total":{{"count":{},"tagged":{},"tags":{}}}"#,
            total.count, total.tagged, total.tags
        )?;
        write!(
            o,
            r#","way_refs":{},"members":{},"node_extent":"#,
            s.refs, s.members
        )?;
        extent_json(o, s.lat, s.lon)?;
        o.write_all(b",")?;
        time_span_json(o, s.time)?;
        write!(
            o,
            r#","max_version":{},"deleted":{}"#,
            s.max_version, s.deleted
        )?;
        write!(
            o,
            r#","sorted":{},"header_sorted":{}"#,
            self.unordered.is_none(),
            h.is_sorted()
        )?;
        o.write_all(b"}\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn order_is_type_then_id() {
        let mut o = Order::default();
        o.next(Kind::Node, 5);
        o.next(Kind::Node, 9);
        o.next(Kind::Way, 1);
        assert!(o.ordered);
        o.next(Kind::Node, 10);
        assert!(!o.ordered);

        let span = |blob, first: (Kind, i64), last: (Kind, i64)| Span {
            blob,
            first,
            last,
            ordered: true,
        };
        let mut s = Stats {
            spans: vec![
                span(2, (Kind::Way, 1), (Kind::Way, 9)),
                span(1, (Kind::Node, 1), (Kind::Node, 9)),
            ],
            ..Stats::default()
        };
        assert_eq!(s.first_unordered(), None);
        s.spans.push(span(3, (Kind::Way, 5), (Kind::Relation, 1)));
        assert_eq!(s.first_unordered(), Some(3));
    }
}
