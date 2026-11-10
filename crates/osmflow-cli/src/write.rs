//! Writing objects and changes as text lines, GeoJSON, or GeoJSON features one per line.

use std::io::{self, BufWriter, Write};
use std::path::Path;

use geo_types::{Coord, LineString, Polygon};
use osmflow::osc::{Action, Meta};
use osmflow::{Block, GeometryError, Info, Location, Member, MemberType};

use crate::output::{self, count, degrees, json_str, timestamp, to_decimicro};
use crate::select::Kind;

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Format {
    /// One line per object, with `#` lines about the input first.
    Text,
    /// One GeoJSON FeatureCollection.
    Geojson,
    /// One GeoJSON Feature per line.
    Geojsonseq,
}

impl Format {
    /// The format an output file's extension asks for: `.geojson` or `.json`, `.geojsonl`, `.geojsons` or `.ndjson`,
    /// and text otherwise.
    pub fn of(path: Option<&Path>) -> Self {
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

/// A way's geometry: what [`Locator::area_or_line`](osmflow::Locator::area_or_line) makes.
pub enum Geometry {
    Line(LineString),
    Area(Polygon),
}

/// What is written about an object besides its type, ID, location, nodes or members, and tags. Each field is left
/// out when it is `None`.
#[derive(Debug, Clone, Copy, Default)]
pub struct Props<'a> {
    /// For a change: the section of the osmChange file it is in.
    pub action: Option<Action>,
    /// For a change of a replication stream: the diff it is in.
    pub sequence: Option<u64>,
    pub version: Option<i32>,
    pub timestamp_ms: Option<i64>,
    pub changeset: Option<i64>,
    pub uid: Option<i32>,
    pub user: Option<&'a str>,
    /// A deleted version, in a history file.
    pub deleted: bool,
}

impl<'a> Props<'a> {
    /// A PBF object's metadata. The values PBF files write for a field they leave out (a version of -1, a timestamp,
    /// changeset or user ID of 0, an empty user name) are left out.
    pub fn of_info(info: Option<Info<'a>>) -> Self {
        let Some(i) = info else {
            return Self::default();
        };
        Self {
            version: Some(i.version).filter(|&v| v != -1),
            timestamp_ms: Some(i.timestamp_ms).filter(|&t| t != 0),
            changeset: Some(i.changeset).filter(|&c| c != 0),
            uid: Some(i.uid).filter(|&u| u != 0),
            user: Some(i.user).filter(|u| !u.is_empty()),
            deleted: !i.visible,
            ..Self::default()
        }
    }

    /// An osmChange object's metadata.
    pub fn of_meta(meta: &'a Meta) -> Self {
        Self {
            version: meta.version,
            timestamp_ms: meta.timestamp_ms,
            changeset: meta.changeset,
            uid: meta.uid,
            user: meta.user.as_deref(),
            ..Self::default()
        }
    }
}

/// Where a [`Writer`] writes.
pub type Out = BufWriter<Box<dyn Write>>;

/// Writes objects in one format.
pub struct Writer {
    out: Out,
    format: Format,
    /// Whether no feature was written yet, to separate GeoJSON features.
    first: bool,
}

impl Writer {
    /// Starts writing: a GeoJSON FeatureCollection opens here.
    pub fn new(out: Box<dyn Write>, format: Format) -> io::Result<Self> {
        let mut w = Self {
            out: BufWriter::with_capacity(1 << 20, out),
            format,
            first: true,
        };
        if format == Format::Geojson {
            w.out
                .write_all(br#"{"type":"FeatureCollection","features":["#)?;
        }
        Ok(w)
    }

    /// Ends the output, and flushes it.
    pub fn end(&mut self) -> io::Result<()> {
        if self.format == Format::Geojson {
            self.out.write_all(b"\n]}\n")?;
        }
        self.out.flush()
    }

    /// Writes a `#` line about the input, in text only.
    pub fn comment(&mut self, line: impl FnOnce(&mut Out) -> io::Result<()>) -> io::Result<()> {
        if self.format != Format::Text {
            return Ok(());
        }
        self.out.write_all(b"# ")?;
        line(&mut self.out)?;
        writeln!(self.out)
    }

    /// A PBF block's layout, as one `##` line.
    pub fn block<T: osmflow::TagMode>(&mut self, block: &Block<T>) -> io::Result<()> {
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

    /// A node, with its location if it has one: a deleted node of a diff may not.
    pub fn node<'a>(
        &mut self,
        id: i64,
        location: Option<Location>,
        props: &Props<'_>,
        tags: impl Iterator<Item = (&'a str, &'a str)>,
    ) -> io::Result<()> {
        if self.format == Format::Text {
            self.text_start(props, 'n', id)?;
            if let Some(Location { lat, lon }) = location {
                let o = &mut self.out;
                o.write_all(b" lat=")?;
                degrees(o, lat)?;
                o.write_all(b" lon=")?;
                degrees(o, lon)?;
            }
            return self.text_rest(props, tags);
        }
        self.feature_start('n', id)?;
        match location {
            Some(Location { lat, lon }) => {
                self.out
                    .write_all(br#""geometry":{"type":"Point","coordinates":"#)?;
                self.position(lat, lon)?;
                self.out.write_all(b"}")?;
            }
            None => self.out.write_all(br#""geometry":null"#)?,
        }
        self.properties(Kind::Node, id, props, tags)?;
        self.out.write_all(b"}")?;
        self.feature_end()
    }

    /// A way, with its geometry when one was asked for. GeoJSON lists the way's node IDs in `@nodes` when it has no
    /// geometry, so a way never loses them.
    pub fn way<'a>(
        &mut self,
        id: i64,
        refs: impl Iterator<Item = i64>,
        geometry: Option<Result<Geometry, GeometryError>>,
        props: &Props<'_>,
        tags: impl Iterator<Item = (&'a str, &'a str)>,
    ) -> io::Result<()> {
        if self.format == Format::Text {
            self.text_start(props, 'w', id)?;
            let o = &mut self.out;
            o.write_all(b" nodes=")?;
            ids(o, refs)?;
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
            return self.text_rest(props, tags);
        }
        self.feature_start('w', id)?;
        self.out.write_all(br#""geometry":"#)?;
        match &geometry {
            Some(Ok(g)) => self.geojson_geometry(g)?,
            _ => self.out.write_all(b"null")?,
        }
        self.properties(Kind::Way, id, props, tags)?;
        match geometry {
            Some(Ok(_)) => {}
            Some(Err(e)) => {
                self.out.write_all(br#","@error":"#)?;
                json_str(&mut self.out, &e.to_string())?;
                self.out.write_all(br#","@nodes":"#)?;
                ids(&mut self.out, refs)?;
            }
            None => {
                self.out.write_all(br#","@nodes":"#)?;
                ids(&mut self.out, refs)?;
            }
        }
        self.out.write_all(b"}")?;
        self.feature_end()
    }

    pub fn relation<'a, 'm>(
        &mut self,
        id: i64,
        members: impl Iterator<Item = Member<'m>>,
        props: &Props<'_>,
        tags: impl Iterator<Item = (&'a str, &'a str)>,
    ) -> io::Result<()> {
        if self.format == Format::Text {
            self.text_start(props, 'r', id)?;
            let o = &mut self.out;
            o.write_all(b" members=[")?;
            for (i, m) in members.enumerate() {
                if i > 0 {
                    o.write_all(b", ")?;
                }
                write!(o, "{}{} ", member_letter(m.kind), m.id)?;
                json_str(o, m.role)?;
            }
            o.write_all(b"]")?;
            return self.text_rest(props, tags);
        }
        self.feature_start('r', id)?;
        self.out.write_all(br#""geometry":null"#)?;
        self.properties(Kind::Relation, id, props, tags)?;
        self.out.write_all(br#","@members":["#)?;
        for (i, m) in members.enumerate() {
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

    /// The start of a text line: the action of a change, then the object's type letter and ID.
    fn text_start(&mut self, props: &Props<'_>, letter: char, id: i64) -> io::Result<()> {
        if let Some(action) = props.action {
            write!(self.out, "{action} ")?;
        }
        write!(self.out, "{letter}{id}")
    }

    /// The end of a text line: metadata, if read, and tags as a JSON object.
    fn text_rest<'a>(
        &mut self,
        props: &Props<'_>,
        tags: impl Iterator<Item = (&'a str, &'a str)>,
    ) -> io::Result<()> {
        let o = &mut self.out;
        if let Some(version) = props.version {
            write!(o, " v{version}")?;
        }
        if let Some(ms) = props.timestamp_ms {
            o.write_all(b" ")?;
            timestamp(o, ms)?;
        }
        if let Some(changeset) = props.changeset {
            write!(o, " c{changeset}")?;
        }
        if let Some(uid) = props.uid {
            write!(o, " u{uid}")?;
        }
        if let Some(user) = props.user {
            o.write_all(b" ")?;
            json_str(o, user)?;
        }
        if props.deleted {
            o.write_all(b" deleted")?;
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

    /// `,"properties":{...` without its closing brace, which ways and relations still add to.
    fn properties<'a>(
        &mut self,
        kind: Kind,
        id: i64,
        props: &Props<'_>,
        tags: impl Iterator<Item = (&'a str, &'a str)>,
    ) -> io::Result<()> {
        let o = &mut self.out;
        write!(o, r#","properties":{{"@type":"{}","@id":{id}"#, kind.name())?;
        if let Some(action) = props.action {
            write!(o, r#","@action":"{action}""#)?;
        }
        if let Some(sequence) = props.sequence {
            write!(o, r#","@sequence":{sequence}"#)?;
        }
        if let Some(version) = props.version {
            write!(o, r#","@version":{version}"#)?;
        }
        if let Some(ms) = props.timestamp_ms {
            o.write_all(br#","@timestamp":""#)?;
            timestamp(o, ms)?;
            o.write_all(b"\"")?;
        }
        if let Some(changeset) = props.changeset {
            write!(o, r#","@changeset":{changeset}"#)?;
        }
        if let Some(uid) = props.uid {
            write!(o, r#","@uid":{uid}"#)?;
        }
        if let Some(user) = props.user {
            o.write_all(br#","@user":"#)?;
            json_str(o, user)?;
        }
        if props.deleted {
            o.write_all(br#","@deleted":true"#)?;
        }
        for (k, v) in tags {
            o.write_all(b",")?;
            json_str(o, k)?;
            o.write_all(b":")?;
            json_str(o, v)?;
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

/// Writes IDs as a JSON array: `[1,2,3]`.
fn ids(o: &mut impl Write, ids: impl Iterator<Item = i64>) -> io::Result<()> {
    o.write_all(b"[")?;
    for (i, id) in ids.enumerate() {
        if i > 0 {
            o.write_all(b",")?;
        }
        write!(o, "{id}")?;
    }
    o.write_all(b"]")
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
    fn points(o: &mut impl Write, coords: &[Coord]) -> io::Result<()> {
        for (i, c) in coords.iter().enumerate() {
            if i > 0 {
                o.write_all(b", ")?;
            }
            degrees(o, to_decimicro(c.x))?;
            o.write_all(b" ")?;
            degrees(o, to_decimicro(c.y))?;
        }
        Ok(())
    }
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
    use std::sync::{Arc, Mutex, PoisonError};

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

    /// A `Write` that keeps what it is given, readable after the writer is done with it.
    #[derive(Clone, Default)]
    struct Shared(Arc<Mutex<Vec<u8>>>);

    impl Write for Shared {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn written(format: Format, f: impl FnOnce(&mut Writer) -> io::Result<()>) -> String {
        let out = Shared::default();
        let mut w = Writer::new(Box::new(out.clone()), format).unwrap();
        f(&mut w).unwrap();
        w.end().unwrap();
        let bytes = out.0.lock().unwrap().clone();
        String::from_utf8(bytes).unwrap()
    }

    #[test]
    fn changes_have_their_action_and_ways_their_nodes() {
        let props = Props {
            action: Some(Action::Modify),
            sequence: Some(4956),
            version: Some(6),
            ..Props::default()
        };
        let tags = [("highway", "primary")];
        let text = written(Format::Text, |w| {
            w.way(7, [1, 2].into_iter(), None, &props, tags.into_iter())?;
            w.node(
                8,
                None,
                &Props {
                    action: Some(Action::Delete),
                    ..Props::default()
                },
                [].into_iter(),
            )
        });
        assert_eq!(
            text,
            "modify w7 nodes=[1,2] v6 {\"highway\":\"primary\"}\ndelete n8\n"
        );
        let json = written(Format::Geojsonseq, |w| {
            w.way(7, [1, 2].into_iter(), None, &props, tags.into_iter())
        });
        assert_eq!(
            json,
            r#"{"type":"Feature","id":"w7","geometry":null,"properties":{"@type":"way","@id":7,"@action":"modify","@sequence":4956,"@version":6,"highway":"primary","@nodes":[1,2]}}"#
                .to_owned()
                + "\n"
        );
    }

    #[test]
    fn metadata_a_pbf_file_leaves_out_is_left_out() {
        let info = Info {
            version: -1,
            timestamp_ms: 0,
            changeset: 0,
            uid: 0,
            user: "",
            visible: true,
        };
        let text = written(Format::Text, |w| {
            w.node(
                1,
                Some(Location::new(10, 20)),
                &Props::of_info(Some(info)),
                [].into_iter(),
            )
        });
        assert_eq!(text, "n1 lat=0.000001 lon=0.000002\n");
    }
}
