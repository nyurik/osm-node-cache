//! `osmflow dump`: writes a file's objects as text or GeoJSON.

use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{self, BufWriter, Write};
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use geo_types::{Coord, LineString, Polygon};
use osmflow::nodes::prelude::{NodeRead as _, NodeStore as _};
use osmflow::nodes::{MemoryReader, MemoryStore};
use osmflow::{
    AreaRules, Block, GeometryError, Header, Info, Location, Locator, MemberType, Node, PbfReader,
    Relation, Select, Way,
};

use crate::output::{self, count, degrees, json_str, timestamp, to_decimicro};
use crate::select::{Filter, Kind, TagSpec, Threads, parse_tag};

/// A file larger than this is dumped whole only with `--all`.
const WHOLE_FILE_LIMIT: u64 = 100 << 20;

#[derive(Debug, clap::Args)]
#[expect(clippy::struct_excessive_bools, reason = "independent flags")]
pub struct Args {
    /// The OSM PBF file.
    input: PathBuf,
    /// Where to write; standard output when left out, or `-`.
    output: Option<PathBuf>,
    /// The output format. By default it follows OUTPUT's extension: `.geojson` for GeoJSON, `.geojsonl`, `.geojsons`
    /// or `.ndjson` for one GeoJSON feature per line, and text otherwise.
    #[arg(short, long)]
    format: Option<Format>,
    /// In text, each way's geometry as WKT, from its nodes' locations. GeoJSON always has them.
    #[arg(long)]
    geometry: bool,
    /// In text, each block's blob, string table and groups before its objects.
    #[arg(long)]
    blocks: bool,
    /// Each object's version, timestamp, changeset and user.
    #[arg(long)]
    metadata: bool,
    /// Stop after this many objects.
    #[arg(long, value_name = "N")]
    limit: Option<u64>,
    /// Dump a file over 100 MiB whole, without --id, --tag or --limit.
    #[arg(long)]
    all: bool,
    /// Closed ways with this tag are areas too: `KEY` with any value, or `KEY=V1,V2` with one of the values. Repeat
    /// the flag for more tags.
    #[arg(long = "area", value_name = "KEY[=VALUES]", value_parser = parse_tag)]
    areas: Vec<TagSpec>,
    /// Closed ways with this tag are lines, unless another tag makes them areas: `KEY` with any value, or
    /// `KEY=V1,V2` with one of the values.
    #[arg(long = "not-area", value_name = "KEY[=VALUES]", value_parser = parse_tag)]
    not_areas: Vec<TagSpec>,
    /// Leave out the built-in tags that make closed ways areas, so only `area=yes` and --area do.
    #[arg(long)]
    no_default_areas: bool,
    #[command(flatten)]
    filter: Filter,
    #[command(flatten)]
    threads: Threads,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum Format {
    /// One line per object, with the file's header first.
    Text,
    /// One GeoJSON FeatureCollection.
    Geojson,
    /// One GeoJSON Feature per line.
    Geojsonseq,
}

impl Format {
    fn of(path: Option<&Path>) -> Self {
        let extension = path
            .and_then(Path::extension)
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase);
        match extension.as_deref() {
            Some("geojson" | "json") => Self::Geojson,
            Some("geojsonl" | "geojsons" | "ndjson") => Self::Geojsonseq,
            _ => Self::Text,
        }
    }
}

pub fn run(args: &Args) -> Result<()> {
    let output = args.output.as_deref().filter(|p| *p != Path::new("-"));
    let format = args.format.unwrap_or_else(|| Format::of(output));
    if args.blocks && format != Format::Text {
        bail!("--blocks shows blocks in text output only");
    }
    let size = fs::metadata(&args.input)
        .with_context(|| format!("cannot read {}", args.input.display()))?
        .len();
    if size > WHOLE_FILE_LIMIT && !args.filter.picks() && args.limit.is_none() && !args.all {
        bail!(
            "{} is {}; give --id, --tag or --limit to choose what to dump, or --all to dump everything",
            args.input.display(),
            output::size(size)
        );
    }
    let file = args.threads.apply(open(&args.input)?)?;
    let store = if (format != Format::Text || args.geometry) && args.filter.wants(Kind::Way) {
        Some(node_locations(args, &file)?)
    } else {
        None
    };
    let out: Box<dyn Write> = match output {
        Some(path) => Box::new(
            File::create(path).with_context(|| format!("cannot create {}", path.display()))?,
        ),
        None => Box::new(io::stdout().lock()),
    };
    let mut w = Writer {
        out: BufWriter::with_capacity(1 << 20, out),
        format,
        first: true,
    };
    w.begin(&args.input, size, file.header())?;

    let mut stop = args.filter.id_stop(file.header().is_sorted());
    let file = args.filter.skip_blocks(file);
    let select = args.filter.select(Select::all());
    let select = if args.metadata {
        select
    } else {
        select.without_metadata()
    };
    let matcher = args.filter.matcher();
    let mut locator = store.as_ref().map(|s| Locator::new(s.reader()));
    let areas = args.area_rules();
    let mut left = args.limit.unwrap_or(u64::MAX);
    args.threads.ordered(&file, select, |block| {
        if args.blocks {
            w.block(&block)?;
        }
        for node in block.nodes() {
            if left == 0 {
                return Ok(ControlFlow::Break(()));
            }
            if matcher.matches(Kind::Node, node.id(), node.tags()) {
                w.node(&node)?;
                left -= 1;
            }
        }
        for way in block.ways() {
            if left == 0 {
                return Ok(ControlFlow::Break(()));
            }
            if matcher.matches(Kind::Way, way.id(), way.tags()) {
                let geometry = locator.as_mut().map(|l| way_geometry(l, &way, &areas));
                w.way(&way, geometry)?;
                left -= 1;
            }
        }
        for relation in block.relations() {
            if left == 0 {
                return Ok(ControlFlow::Break(()));
            }
            if matcher.matches(Kind::Relation, relation.id(), relation.tags()) {
                w.relation(&relation)?;
                left -= 1;
            }
        }
        Ok(if stop.as_mut().is_some_and(|s| s.block(&block)) {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        })
    })?;
    w.end()?;
    let split = file.split_string_tables();
    if split > 0 {
        writeln!(
            io::stderr(),
            "osmflow: note: {split} blocks of {} have a string table that goes on after their first group, which \
             --tag may have misjudged in the blocks it skipped before finding the first one. Please report the file at \
             https://github.com/nyurik/osmflow/issues",
            args.input.display()
        )?;
    }
    Ok(())
}

fn open(path: &Path) -> Result<PbfReader> {
    PbfReader::open(path).with_context(|| format!("cannot read {}", path.display()))
}

/// The locations of the nodes the dumped ways need. A whole file loads every node; otherwise one pass finds the ways
/// to dump and the nodes they need, and a second keeps only those nodes, so a few ways of the planet need little
/// memory.
fn node_locations(args: &Args, file: &PbfReader) -> Result<MemoryStore> {
    let mut store = MemoryStore::new();
    if args.limit == Some(0) {
        // Nothing is dumped, so no way needs its nodes; the countdown below would also underflow.
        return Ok(store);
    }
    if !args.filter.picks() && args.limit.is_none() {
        file.load_nodes(&mut store)?;
        return Ok(store);
    }
    let matcher = args.filter.matcher();
    let mut needed = HashSet::new();
    let mut left = args.limit.unwrap_or(u64::MAX);
    let mut stop = args.filter.id_stop(file.header().is_sorted());
    let ways = args.filter.skip_blocks(file.clone());
    let select = Select::all()
        .without_nodes()
        .without_relations()
        .without_metadata();
    args.threads.ordered(&ways, select, |block| {
        for way in block.ways() {
            if matcher.matches(Kind::Way, way.id(), way.tags()) {
                needed.extend(way.refs());
                left -= 1;
                if left == 0 {
                    return Ok(ControlFlow::Break(()));
                }
            }
        }
        // Only ways are read here, so a stop waits for nothing but the ways.
        let done = stop.as_mut().is_some_and(|s| {
            s.passed(Kind::Node, i64::MAX);
            s.passed(Kind::Relation, i64::MAX);
            s.block(&block)
        });
        Ok(if done {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        })
    })?;
    if needed.is_empty() {
        return Ok(store);
    }
    args.threads
        .ordered(file, Select::node_locations(), |block| {
            for node in block.nodes() {
                let location = Location::new(node.decimicro_lat(), node.decimicro_lon());
                if needed.remove(&node.id())
                    && location.is_valid()
                    && let Ok(id) = u64::try_from(node.id())
                {
                    store.set(id, location)?;
                }
            }
            Ok(if needed.is_empty() {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            })
        })?;
    Ok(store)
}

impl Args {
    /// Which closed ways are areas: the built-in rules, or none, changed by `--area` and `--not-area`.
    fn area_rules(&self) -> AreaRules {
        let mut rules = if self.no_default_areas {
            AreaRules::none()
        } else {
            AreaRules::default()
        };
        for spec in &self.areas {
            rules.add(&spec.key, &values(spec));
        }
        for spec in &self.not_areas {
            rules.remove(&spec.key, &values(spec));
        }
        rules
    }
}

fn values(spec: &TagSpec) -> Vec<&str> {
    spec.values.iter().map(String::as_str).collect()
}

/// A way's geometry: what [`Locator::area_or_line`] makes.
enum Geometry {
    Line(LineString),
    Area(Polygon),
}

#[expect(
    clippy::wildcard_enum_match_arm,
    reason = "area_or_line makes only lines and polygons"
)]
fn way_geometry(
    locator: &mut Locator<MemoryReader<'_>>,
    way: &Way<'_>,
    areas: &AreaRules,
) -> Result<Geometry, GeometryError> {
    Ok(match locator.area_or_line(way, areas)? {
        geo_types::Geometry::LineString(line) => Geometry::Line(line),
        geo_types::Geometry::Polygon(polygon) => Geometry::Area(polygon),
        other => unreachable!("a way's geometry is a line or a polygon, not {other:?}"),
    })
}

/// Writes objects in one format.
struct Writer {
    out: BufWriter<Box<dyn Write>>,
    format: Format,
    /// Whether no feature was written yet, to separate GeoJSON features.
    first: bool,
}

impl Writer {
    fn begin(&mut self, path: &Path, size: u64, header: &Header) -> io::Result<()> {
        match self.format {
            Format::Text => self.header(path, size, header),
            Format::Geojson => self
                .out
                .write_all(br#"{"type":"FeatureCollection","features":["#),
            Format::Geojsonseq => Ok(()),
        }
    }

    fn end(&mut self) -> io::Result<()> {
        if self.format == Format::Geojson {
            self.out.write_all(b"\n]}\n")?;
        }
        self.out.flush()
    }

    /// The file's header, as `#` lines.
    fn header(&mut self, path: &Path, size: u64, h: &Header) -> io::Result<()> {
        let o = &mut self.out;
        writeln!(o, "# file: {} ({})", path.display(), output::size(size))?;
        if let Some(program) = &h.writing_program {
            writeln!(o, "# writing program: {program}")?;
        }
        if let Some(source) = &h.source {
            writeln!(o, "# source: {source}")?;
        }
        writeln!(o, "# required features: {}", h.required_features.join(", "))?;
        if !h.optional_features.is_empty() {
            writeln!(o, "# optional features: {}", h.optional_features.join(", "))?;
        }
        if let Some(b) = h.bbox {
            write!(o, "# bbox (west, south, east, north): ")?;
            for (i, nano) in [b.left, b.bottom, b.right, b.top].into_iter().enumerate() {
                if i > 0 {
                    o.write_all(b", ")?;
                }
                output::nanodegrees(o, nano)?;
            }
            writeln!(o)?;
        }
        if let Some(sequence) = h.replication_sequence {
            write!(o, "# replication: sequence {sequence}")?;
            if let Some(secs) = h.replication_timestamp {
                o.write_all(b", ")?;
                timestamp(o, secs.saturating_mul(1000))?;
            }
            if let Some(url) = &h.replication_base_url {
                write!(o, ", {url}")?;
            }
            writeln!(o)?;
        }
        Ok(())
    }

    /// A block's layout, as one `##` line.
    fn block(&mut self, block: &Block) -> io::Result<()> {
        let (position, l) = (block.position(), block.layout());
        let o = &mut self.out;
        write!(
            o,
            "## blob {} at byte {}: {} bytes {}, {} raw, {} strings in {} bytes",
            position.blob,
            count(position.offset),
            count(l.blob_bytes as u64),
            if l.zlib { "zlib" } else { "uncompressed" },
            count(l.raw_bytes as u64),
            count(l.strings as u64),
            count(l.string_bytes as u64),
        )?;
        if (
            l.granularity,
            l.lat_offset,
            l.lon_offset,
            l.date_granularity,
        ) != (100, 0, 0, 1000)
        {
            write!(
                o,
                ", granularity {} nd, offsets {} {} nd, date granularity {} ms",
                l.granularity, l.lat_offset, l.lon_offset, l.date_granularity
            )?;
        }
        o.write_all(b"; groups:")?;
        for (i, g) in l.groups.iter().enumerate() {
            let kind = output::GROUP_NAMES[output::group_index(g.kind)];
            let sep = if i == 0 { " " } else { ", " };
            match g.objects {
                Some(n) => write!(o, "{sep}{} {kind}", count(n as u64))?,
                None => write!(o, "{sep}{kind}")?,
            }
        }
        writeln!(o)
    }

    fn node(&mut self, node: &Node<'_>) -> io::Result<()> {
        let (lat, lon) = (node.decimicro_lat(), node.decimicro_lon());
        if self.format == Format::Text {
            let o = &mut self.out;
            write!(o, "n{} lat=", node.id())?;
            degrees(o, lat)?;
            o.write_all(b" lon=")?;
            degrees(o, lon)?;
            return self.text_rest(node.info(), node.tags());
        }
        self.feature_start('n', node.id())?;
        self.out
            .write_all(br#""geometry":{"type":"Point","coordinates":"#)?;
        self.position(lat, lon)?;
        self.out.write_all(b"}")?;
        self.properties(Kind::Node, node.id(), node.info(), node.tags())?;
        self.feature_end()
    }

    /// A way, with its geometry when one was asked for.
    fn way(
        &mut self,
        way: &Way<'_>,
        geometry: Option<Result<Geometry, GeometryError>>,
    ) -> io::Result<()> {
        if self.format == Format::Text {
            let o = &mut self.out;
            write!(o, "w{} nodes=[", way.id())?;
            for (i, id) in way.refs().enumerate() {
                if i > 0 {
                    o.write_all(b",")?;
                }
                write!(o, "{id}")?;
            }
            o.write_all(b"]")?;
            match geometry {
                Some(Ok(g)) => {
                    o.write_all(b" ")?;
                    wkt(o, &g)?;
                }
                Some(Err(e)) => {
                    o.write_all(b" geometry_error=")?;
                    json_str(o, &e.to_string())?;
                }
                None => {}
            }
            return self.text_rest(way.info(), way.tags());
        }
        self.feature_start('w', way.id())?;
        self.out.write_all(br#""geometry":"#)?;
        match &geometry {
            Some(Ok(g)) => self.geojson_geometry(g)?,
            _ => self.out.write_all(b"null")?,
        }
        self.properties(Kind::Way, way.id(), way.info(), way.tags())?;
        if let Some(Err(e)) = geometry {
            self.out.write_all(br#","@error":"#)?;
            json_str(&mut self.out, &e.to_string())?;
        }
        self.out.write_all(b"}")?;
        self.feature_end()
    }

    fn relation(&mut self, relation: &Relation<'_>) -> io::Result<()> {
        if self.format == Format::Text {
            let o = &mut self.out;
            write!(o, "r{} members=[", relation.id())?;
            for (i, m) in relation.members().enumerate() {
                if i > 0 {
                    o.write_all(b", ")?;
                }
                write!(o, "{}{} ", member_letter(m.kind), m.id)?;
                json_str(o, m.role)?;
            }
            o.write_all(b"]")?;
            return self.text_rest(relation.info(), relation.tags());
        }
        self.feature_start('r', relation.id())?;
        self.out.write_all(br#""geometry":null"#)?;
        self.properties(
            Kind::Relation,
            relation.id(),
            relation.info(),
            relation.tags(),
        )?;
        self.out.write_all(br#","@members":["#)?;
        for (i, m) in relation.members().enumerate() {
            if i > 0 {
                self.out.write_all(b",")?;
            }
            write!(
                self.out,
                r#"{{"type":"{}","ref":{},"role":"#,
                member_letter(m.kind),
                m.id
            )?;
            json_str(&mut self.out, m.role)?;
            self.out.write_all(b"}")?;
        }
        self.out.write_all(b"]}")?;
        self.feature_end()
    }

    /// The end of a text line: metadata, if read, and tags as a JSON object.
    fn text_rest<'a>(
        &mut self,
        info: Option<Info<'_>>,
        tags: impl Iterator<Item = (&'a str, &'a str)>,
    ) -> io::Result<()> {
        let o = &mut self.out;
        if let Some(info) = info {
            write!(o, " v{} ", info.version)?;
            timestamp(o, info.timestamp_ms)?;
            write!(o, " c{} u{} ", info.changeset, info.uid)?;
            json_str(o, info.user)?;
            if !info.visible {
                o.write_all(b" deleted")?;
            }
        }
        let mut tags = tags.peekable();
        if tags.peek().is_some() {
            o.write_all(b" ")?;
            json_object(o, tags)?;
        }
        writeln!(o)
    }

    fn feature_start(&mut self, letter: char, id: i64) -> io::Result<()> {
        if self.format == Format::Geojson {
            self.out
                .write_all(if self.first { b"\n" } else { b",\n" })?;
        }
        self.first = false;
        write!(self.out, r#"{{"type":"Feature","id":"{letter}{id}","#)
    }

    fn feature_end(&mut self) -> io::Result<()> {
        self.out.write_all(b"}")?;
        if self.format == Format::Geojsonseq {
            self.out.write_all(b"\n")?;
        }
        Ok(())
    }

    /// `,"properties":{...` without its closing brace, which relations and failed ways still add to.
    fn properties<'a>(
        &mut self,
        kind: Kind,
        id: i64,
        info: Option<Info<'_>>,
        tags: impl Iterator<Item = (&'a str, &'a str)>,
    ) -> io::Result<()> {
        let o = &mut self.out;
        write!(o, r#","properties":{{"@type":"{}","@id":{id}"#, kind.name())?;
        if let Some(info) = info {
            write!(o, r#","@version":{},"@timestamp":"#, info.version)?;
            o.write_all(b"\"")?;
            timestamp(o, info.timestamp_ms)?;
            write!(
                o,
                r#"","@changeset":{},"@uid":{},"@user":"#,
                info.changeset, info.uid
            )?;
            json_str(o, info.user)?;
            if !info.visible {
                o.write_all(br#","@deleted":true"#)?;
            }
        }
        for (k, v) in tags {
            o.write_all(b",")?;
            json_str(o, k)?;
            o.write_all(b":")?;
            json_str(o, v)?;
        }
        if kind == Kind::Node {
            o.write_all(b"}")?;
        }
        Ok(())
    }

    fn position(&mut self, lat: i32, lon: i32) -> io::Result<()> {
        self.out.write_all(b"[")?;
        degrees(&mut self.out, lon)?;
        self.out.write_all(b",")?;
        degrees(&mut self.out, lat)?;
        self.out.write_all(b"]")
    }

    fn coords(&mut self, line: &LineString) -> io::Result<()> {
        self.out.write_all(b"[")?;
        for (i, c) in line.0.iter().enumerate() {
            if i > 0 {
                self.out.write_all(b",")?;
            }
            self.position(to_decimicro(c.y), to_decimicro(c.x))?;
        }
        self.out.write_all(b"]")
    }

    fn geojson_geometry(&mut self, g: &Geometry) -> io::Result<()> {
        match g {
            Geometry::Line(line) => {
                self.out
                    .write_all(br#"{"type":"LineString","coordinates":"#)?;
                self.coords(line)?;
            }
            Geometry::Area(polygon) => {
                self.out
                    .write_all(br#"{"type":"Polygon","coordinates":["#)?;
                self.coords(polygon.exterior())?;
                self.out.write_all(b"]")?;
            }
        }
        self.out.write_all(b"}")
    }
}

fn member_letter(kind: MemberType) -> char {
    match kind {
        MemberType::Node => 'n',
        MemberType::Way => 'w',
        MemberType::Relation => 'r',
    }
}

fn json_object<'a>(
    o: &mut impl Write,
    pairs: impl Iterator<Item = (&'a str, &'a str)>,
) -> io::Result<()> {
    o.write_all(b"{")?;
    for (i, (k, v)) in pairs.enumerate() {
        if i > 0 {
            o.write_all(b",")?;
        }
        json_str(o, k)?;
        o.write_all(b":")?;
        json_str(o, v)?;
    }
    o.write_all(b"}")
}

/// Writes a geometry as WKT: `LINESTRING(lon lat, ...)` or `POLYGON((...))`.
fn wkt(o: &mut impl Write, g: &Geometry) -> io::Result<()> {
    let points = |o: &mut dyn Write, coords: &[Coord]| -> io::Result<()> {
        for (i, c) in coords.iter().enumerate() {
            if i > 0 {
                o.write_all(b", ")?;
            }
            let mut buf = Vec::with_capacity(24);
            degrees(&mut buf, to_decimicro(c.x))?;
            buf.push(b' ');
            degrees(&mut buf, to_decimicro(c.y))?;
            o.write_all(&buf)?;
        }
        Ok(())
    };
    match g {
        Geometry::Line(line) => {
            o.write_all(b"LINESTRING(")?;
            points(o, &line.0)?;
            o.write_all(b")")
        }
        Geometry::Area(polygon) => {
            o.write_all(b"POLYGON((")?;
            points(o, &polygon.exterior().0)?;
            o.write_all(b"))")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_follow_the_extension() {
        assert_eq!(Format::of(Some(Path::new("dc.geojson"))), Format::Geojson);
        assert_eq!(
            Format::of(Some(Path::new("dc.geojsonl"))),
            Format::Geojsonseq
        );
        assert_eq!(Format::of(Some(Path::new("dc.txt"))), Format::Text);
        assert_eq!(Format::of(Some(Path::new("DC.GeoJSON"))), Format::Geojson);
        assert_eq!(Format::of(None), Format::Text);
    }
}
