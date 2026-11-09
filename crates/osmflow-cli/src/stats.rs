//! `osmflow stats`: a summary of a file: its header, blocks, object counts, ID ranges, extent and time span.

use std::fs;
use std::io::{self, BufWriter, Write};
use std::path::PathBuf;
use std::sync::{Mutex, PoisonError};

use anyhow::{Context as _, Result};
use osmflow::{Block, GroupKind, Header, Info, PbfReader, Select};

use crate::output::{self, count, degrees, json_str, nanodegrees, timestamp};
use crate::select::{Kind, Threads};

#[derive(Debug, clap::Args)]
pub struct Args {
    /// The OSM PBF file.
    input: PathBuf,
    /// Write the summary as one JSON object.
    #[arg(long)]
    json: bool,
    #[command(flatten)]
    threads: Threads,
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
    /// Object timestamps, earliest and latest, in milliseconds since 1970.
    time: Option<(i64, i64)>,
    max_version: i32,
    deleted: u64,
    spans: Vec<Span>,
}

fn widen<T: Ord + Copy>(range: &mut Option<(T, T)>, low: T, high: T) {
    *range = Some(match *range {
        Some((a, b)) => (a.min(low), b.max(high)),
        None => (low, high),
    });
}

impl Stats {
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
            let i = match g.kind {
                GroupKind::Nodes => 0,
                GroupKind::DenseNodes => 1,
                GroupKind::Ways => 2,
                GroupKind::Relations => 3,
                GroupKind::Empty | _ => 4,
            };
            s.groups[i] += 1;
        }
        let mut order = Order::default();
        for n in block.nodes() {
            s.types[0].add(n.id(), n.tags().len());
            s.info(n.info());
            let (lat, lon) = (n.decimicro_lat(), n.decimicro_lon());
            widen(&mut s.lat, lat, lat);
            widen(&mut s.lon, lon, lon);
            order.next(Kind::Node, n.id());
        }
        for w in block.ways() {
            s.types[1].add(w.id(), w.tags().len());
            s.info(w.info());
            s.refs += w.refs().len() as u64;
            order.next(Kind::Way, w.id());
        }
        for r in block.relations() {
            s.types[2].add(r.id(), r.tags().len());
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
            widen(&mut self.time, i.timestamp_ms, i.timestamp_ms);
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
fn key((kind, id): (Kind, i64)) -> (u8, i64) {
    let rank = match kind {
        Kind::Node => 0,
        Kind::Way => 1,
        Kind::Relation => 2,
    };
    (rank, id)
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
    let size = fs::metadata(&args.input)
        .with_context(|| format!("cannot read {}", args.input.display()))?
        .len();
    let file = PbfReader::open(&args.input)
        .with_context(|| format!("cannot read {}", args.input.display()))?;
    let file = args.threads.apply(file)?;
    let total = Mutex::new(Stats::default());
    args.threads.unordered(&file, Select::all(), |block| {
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
        path: &args.input.display().to_string(),
        size,
        header: file.header(),
        stats: &stats,
        unordered,
    };
    if args.json {
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
}

const TYPES: [&str; 3] = ["nodes", "ways", "relations"];

impl Report<'_> {
    fn text(&self, o: &mut impl Write) -> io::Result<()> {
        self.text_header(o)?;
        self.text_counts(o)
    }

    /// The file and its header.
    fn text_header(&self, o: &mut impl Write) -> io::Result<()> {
        let h = self.header;
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
        if let Some(sequence) = h.replication_sequence {
            write!(o, "replication   sequence {sequence}")?;
            if let Some(secs) = h.replication_timestamp {
                o.write_all(b" at ")?;
                timestamp(o, secs.saturating_mul(1000))?;
            }
            writeln!(o)?;
        }
        Ok(())
    }

    /// What the blocks hold.
    fn text_counts(&self, o: &mut impl Write) -> io::Result<()> {
        let (h, s) = (self.header, self.stats);
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
        let names = ["plain nodes", "dense nodes", "ways", "relations", "empty"];
        let groups: Vec<String> = s
            .groups
            .iter()
            .zip(names)
            .filter(|&(&n, _)| n > 0)
            .map(|(&n, name)| format!("{} {name}", count(n)))
            .collect();
        writeln!(o, "groups        {}", groups.join(", "))?;
        writeln!(
            o,
            "{:13} {:>15} {:>15} {:>15}   IDs",
            "", "count", "tagged", "tags"
        )?;
        for (name, t) in TYPES.iter().zip(&s.types) {
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
        writeln!(o, "way refs      {:>15}", count(s.refs))?;
        writeln!(o, "members       {:>15}", count(s.members))?;
        if let (Some((south, north)), Some((west, east))) = (s.lat, s.lon) {
            o.write_all(b"node extent   lon ")?;
            degrees(o, west)?;
            o.write_all(b" .. ")?;
            degrees(o, east)?;
            o.write_all(b", lat ")?;
            degrees(o, south)?;
            o.write_all(b" .. ")?;
            degrees(o, north)?;
            writeln!(o)?;
        }
        if let Some((first, last)) = s.time {
            o.write_all(b"timestamps    ")?;
            timestamp(o, first)?;
            o.write_all(b" .. ")?;
            timestamp(o, last)?;
            writeln!(o, "; highest version {}", s.max_version)?;
        }
        if s.deleted > 0 {
            writeln!(o, "deleted       {:>15}", count(s.deleted))?;
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
        opt_time(
            o,
            h.replication_timestamp
                .map(|secs| secs.saturating_mul(1000)),
        )?;
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
        for (name, t) in TYPES.iter().zip(&s.types) {
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
        write!(
            o,
            r#","way_refs":{},"members":{},"node_extent":"#,
            s.refs, s.members
        )?;
        match (s.lat, s.lon) {
            (Some((south, north)), Some((west, east))) => {
                o.write_all(b"[")?;
                for (i, v) in [west, south, east, north].into_iter().enumerate() {
                    if i > 0 {
                        o.write_all(b",")?;
                    }
                    degrees(o, v)?;
                }
                o.write_all(b"]")?;
            }
            _ => o.write_all(b"null")?,
        }
        o.write_all(br#","first_timestamp":"#)?;
        opt_time(o, s.time.map(|t| t.0))?;
        o.write_all(br#","last_timestamp":"#)?;
        opt_time(o, s.time.map(|t| t.1))?;
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

fn opt_str(o: &mut impl Write, s: Option<&str>) -> io::Result<()> {
    match s {
        Some(s) => json_str(o, s),
        None => o.write_all(b"null"),
    }
}

fn opt_num(o: &mut impl Write, n: Option<i64>) -> io::Result<()> {
    match n {
        Some(n) => write!(o, "{n}"),
        None => o.write_all(b"null"),
    }
}

fn opt_time(o: &mut impl Write, ms: Option<i64>) -> io::Result<()> {
    match ms {
        Some(ms) => {
            o.write_all(b"\"")?;
            timestamp(o, ms)?;
            o.write_all(b"\"")
        }
        None => o.write_all(b"null"),
    }
}

fn str_array(o: &mut impl Write, items: &[String]) -> io::Result<()> {
    o.write_all(b"[")?;
    for (i, s) in items.iter().enumerate() {
        if i > 0 {
            o.write_all(b",")?;
        }
        json_str(o, s)?;
    }
    o.write_all(b"]")
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
