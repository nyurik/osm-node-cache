//! Rows in PostgreSQL's binary `COPY` format, encoded straight into a buffer: a row is its field count, then each
//! field as its length and its bytes, big-endian. Geometries are TWKB with OSM's 7 decimal digits, which PostGIS reads
//! with `ST_GeomFromTWKB` exactly as they were, or EWKB with SRID 4326, by the database's [`GeometryFormat`].

use osmflow::{Info, Location, Member, MemberType, Timestamp, osc};
use twkb::{write_scaled_line_string, write_scaled_point};

use crate::GeometryFormat;

/// The start of a binary `COPY` stream: its signature, no flags, and no header extension.
pub(crate) const HEADER: &[u8] = b"PGCOPY\n\xff\r\n\0\0\0\0\0\0\0\0\0";
/// The end of a binary `COPY` stream: a field count of -1.
pub(crate) const TRAILER: &[u8] = &[0xff, 0xff];

/// PostgreSQL's epoch, 2000-01-01T00:00:00Z, in milliseconds since 1970.
const PG_EPOCH_MS: i64 = 946_684_800_000;
const CHAR_OID: i32 = 18;
const INT8_OID: i32 = 20;
const TEXT_OID: i32 = 25;
/// EWKB's flag for a geometry that carries its SRID.
const EWKB_SRID: u32 = 0x2000_0000;
const SRID: u32 = 4326;
/// OSM's decimal digits, which TWKB keeps exactly.
const DIGITS: i8 = 7;

/// An object's metadata, as both PBF files and osmChange files may give it.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Meta {
    pub(crate) version: Option<i32>,
    pub(crate) changeset: Option<i64>,
    pub(crate) timestamp: Option<Timestamp>,
}

impl From<Option<Info<'_>>> for Meta {
    /// A PBF file's metadata, leaving out what it marks as missing.
    fn from(info: Option<Info<'_>>) -> Self {
        info.map_or_else(Self::default, |i| Self {
            version: (i.version >= 0).then_some(i.version),
            changeset: (i.changeset != 0).then_some(i.changeset),
            timestamp: (i.timestamp != Timestamp::EPOCH).then_some(i.timestamp),
        })
    }
}

impl From<&osc::Meta> for Meta {
    fn from(m: &osc::Meta) -> Self {
        Self {
            version: m.version,
            changeset: m.changeset,
            timestamp: m.timestamp,
        }
    }
}

/// Rows of one table, in the binary `COPY` format, without the stream's header and trailer, their geometries in one
/// format.
#[derive(Debug, Default)]
pub(crate) struct Rows {
    pub(crate) buf: Vec<u8>,
    pub(crate) count: u64,
    geometry: GeometryFormat,
}

impl Rows {
    /// No rows yet, their geometries to be in `geometry`.
    pub(crate) fn new(geometry: GeometryFormat) -> Self {
        Self {
            geometry,
            ..Self::default()
        }
    }

    /// A row of `osm.nodes`, its columns as `Table::Nodes` lists them.
    pub(crate) fn node<'a>(
        &mut self,
        id: i64,
        meta: Meta,
        rev: i64,
        tags: impl Iterator<Item = (&'a str, &'a str)>,
        loc: Option<Location>,
    ) {
        self.row(7)
            .i64(id)
            .meta(meta)
            .i64(rev)
            .tags(tags)
            .point(loc);
    }

    /// A row of `osm.ways`, with `minor` 0, its columns as `Table::Ways` lists them.
    pub(crate) fn way<'a>(
        &mut self,
        id: i64,
        meta: Meta,
        rev: i64,
        tags: impl Iterator<Item = (&'a str, &'a str)>,
        nodes: impl ExactSizeIterator<Item = i64>,
        line: Option<&[Location]>,
    ) {
        self.row(9)
            .i64(id)
            .meta(meta)
            .i32(0)
            .i64(rev)
            .tags(tags)
            .ids(nodes)
            .line(line);
    }

    /// A row of `osm.relations`, its columns as `Table::Relations` lists them.
    pub(crate) fn relation<'a>(
        &mut self,
        id: i64,
        meta: Meta,
        rev: i64,
        tags: impl Iterator<Item = (&'a str, &'a str)>,
        members: impl ExactSizeIterator<Item = Member<'a>> + Clone,
    ) {
        self.row(9)
            .i64(id)
            .meta(meta)
            .i64(rev)
            .tags(tags)
            .members(members);
    }

    /// Starts a row of `fields` fields.
    pub(crate) fn row(&mut self, fields: u16) -> &mut Self {
        self.count += 1;
        self.buf.extend(fields.to_be_bytes());
        self
    }

    pub(crate) fn null(&mut self) -> &mut Self {
        self.buf.extend((-1_i32).to_be_bytes());
        self
    }

    pub(crate) fn i64(&mut self, v: i64) -> &mut Self {
        self.buf.extend(8_i32.to_be_bytes());
        self.buf.extend(v.to_be_bytes());
        self
    }

    pub(crate) fn i32(&mut self, v: i32) -> &mut Self {
        self.buf.extend(4_i32.to_be_bytes());
        self.buf.extend(v.to_be_bytes());
        self
    }

    /// A `"char"`.
    pub(crate) fn char(&mut self, c: u8) -> &mut Self {
        self.buf.extend(1_i32.to_be_bytes());
        self.buf.push(c);
        self
    }

    /// The version, changeset and timestamp columns.
    pub(crate) fn meta(&mut self, meta: Meta) -> &mut Self {
        match meta.version {
            Some(v) => self.i32(v),
            None => self.null(),
        };
        match meta.changeset {
            Some(c) => self.i64(c),
            None => self.null(),
        };
        match meta.timestamp {
            Some(t) => self.i64((t.as_millis() - PG_EPOCH_MS) * 1000),
            None => self.null(),
        }
    }

    /// Tags as a `jsonb` object; of a key given twice, PostgreSQL keeps the last.
    pub(crate) fn tags<'a>(&mut self, tags: impl Iterator<Item = (&'a str, &'a str)>) -> &mut Self {
        self.var(|b| {
            // jsonb's binary format: version 1, then the JSON text
            b.extend(b"\x01{");
            for (i, (k, v)) in tags.enumerate() {
                if i > 0 {
                    b.push(b',');
                }
                json_str(b, k);
                b.push(b':');
                json_str(b, v);
            }
            b.push(b'}');
        })
    }

    /// An `int8[]`.
    pub(crate) fn ids(&mut self, ids: impl ExactSizeIterator<Item = i64>) -> &mut Self {
        self.var(|b| {
            array_header(b, INT8_OID, ids.len());
            for id in ids {
                b.extend(8_i32.to_be_bytes());
                b.extend(id.to_be_bytes());
            }
        })
    }

    /// A relation's members, as three columns: `"char"[]` types, `int8[]` IDs, `text[]` roles.
    pub(crate) fn members<'a>(
        &mut self,
        members: impl ExactSizeIterator<Item = Member<'a>> + Clone,
    ) -> &mut Self {
        let n = members.len();
        self.var(|b| {
            array_header(b, CHAR_OID, n);
            for m in members.clone() {
                b.extend(1_i32.to_be_bytes());
                b.push(match m.kind {
                    MemberType::Node => b'n',
                    MemberType::Way => b'w',
                    MemberType::Relation => b'r',
                });
            }
        });
        self.ids(members.clone().map(|m| m.id));
        self.var(|b| {
            array_header(b, TEXT_OID, n);
            for m in members {
                b.extend(len(m.role.len()).to_be_bytes());
                b.extend(m.role.as_bytes());
            }
        })
    }

    /// A point, or NULL.
    pub(crate) fn point(&mut self, loc: Option<Location>) -> &mut Self {
        let Some(loc) = loc else {
            return self.null();
        };
        let format = self.geometry;
        self.var(|b| match format {
            GeometryFormat::Twkb => {
                write_scaled_point(b, DIGITS, xy(loc)).expect("7 digits are in range");
            }
            GeometryFormat::Wkb => {
                ewkb_header(b, 1);
                push_location(b, loc);
            }
        })
    }

    /// A line string, or NULL for fewer than two points, which PostGIS does not take as a line.
    pub(crate) fn line(&mut self, locs: Option<&[Location]>) -> &mut Self {
        let Some(locs) = locs.filter(|l| l.len() >= 2) else {
            return self.null();
        };
        let format = self.geometry;
        self.var(|b| match format {
            GeometryFormat::Twkb => {
                write_scaled_line_string(b, DIGITS, locs.iter().map(|&l| xy(l)))
                    .expect("7 digits are in range");
            }
            GeometryFormat::Wkb => {
                ewkb_header(b, 2);
                b.extend(len(locs.len()).to_le_bytes());
                for &loc in locs {
                    push_location(b, loc);
                }
            }
        })
    }

    /// A field whose length is known once it is written.
    fn var(&mut self, f: impl FnOnce(&mut Vec<u8>)) -> &mut Self {
        let start = self.buf.len();
        self.buf.extend([0; 4]);
        f(&mut self.buf);
        let n = len(self.buf.len() - start - 4);
        self.buf[start..start + 4].copy_from_slice(&n.to_be_bytes());
        self
    }
}

/// A length as the format stores it. OSM limits keep every field far below 2 GiB.
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    reason = "fields are far below 2 GiB"
)]
fn len(n: usize) -> i32 {
    n as i32
}

/// A one-dimensional array's header, or an empty array's.
fn array_header(b: &mut Vec<u8>, oid: i32, n: usize) {
    if n == 0 {
        b.extend([0_i32, 0, oid].iter().flat_map(|v| v.to_be_bytes()));
    } else {
        b.extend(
            [1_i32, 0, oid, len(n), 1]
                .iter()
                .flat_map(|v| v.to_be_bytes()),
        );
    }
}

/// Little-endian EWKB's byte order, type with the SRID flag, and SRID.
fn ewkb_header(b: &mut Vec<u8>, kind: u32) {
    b.push(1);
    b.extend((kind | EWKB_SRID).to_le_bytes());
    b.extend(SRID.to_le_bytes());
}

fn push_location(b: &mut Vec<u8>, loc: Location) {
    b.extend(loc.lon_degrees().to_le_bytes());
    b.extend(loc.lat_degrees().to_le_bytes());
}

/// A location's x and y, as integers of 1e-7 degrees.
fn xy(loc: Location) -> [i64; 2] {
    [loc.lon.into(), loc.lat.into()]
}

/// A point or a line string of TWKB with OSM's 7 digits, as the database holds them, decoded for [`locations`];
/// `None` for anything else, or for an ordinate beyond a [`Location`]'s.
pub(crate) fn osm_twkb(buf: &[u8]) -> Option<twkb::Twkb> {
    let g = twkb::Twkb::new(buf).ok()?;
    let header = g.header();
    let osm = header.precision().xy == DIGITS && header.dimension() == twkb::Dimension::Xy;
    (osm && g.stored_coords().iter().all(|&v| i32::try_from(v).is_ok())).then_some(g)
}

/// The locations of a geometry [`osm_twkb`] decoded, every point kept.
pub(crate) fn locations(g: &twkb::Twkb) -> impl ExactSizeIterator<Item = Location> + '_ {
    // osm_twkb checked that each ordinate fits
    let ordinate = |v: i64| i32::try_from(v).unwrap_or_default();
    g.stored_coords()
        .as_chunks::<2>()
        .0
        .iter()
        .map(move |&[x, y]| Location::new(ordinate(y), ordinate(x)))
}

/// Writes `s` as a JSON string.
fn json_str(b: &mut Vec<u8>, s: &str) {
    b.push(b'"');
    for &c in s.as_bytes() {
        match c {
            b'"' => b.extend(b"\\\""),
            b'\\' => b.extend(b"\\\\"),
            0..0x20 => {
                const HEX: &[u8; 16] = b"0123456789abcdef";
                b.extend(b"\\u00");
                b.push(HEX[usize::from(c >> 4)]);
                b.push(HEX[usize::from(c & 15)]);
            }
            _ => b.push(c),
        }
    }
    b.push(b'"');
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Table;

    /// The locations TWKB holds, as `rebuild` reads them.
    fn twkb_locations(buf: &[u8]) -> Vec<Location> {
        osm_twkb(buf).iter().flat_map(locations).collect()
    }

    /// The fields of each row of `rows`, walked by their lengths.
    fn fields(rows: &Rows) -> Vec<usize> {
        let mut b = &rows.buf[..];
        let mut out = Vec::new();
        while let Some((n, rest)) = b.split_first_chunk::<2>() {
            let n = u16::from_be_bytes(*n);
            b = rest;
            for _ in 0..n {
                let (len, rest) = b.split_first_chunk::<4>().unwrap();
                let len = usize::try_from(i32::from_be_bytes(*len)).unwrap_or(0);
                b = &rest[len..];
            }
            out.push(usize::from(n));
        }
        out
    }

    #[test]
    fn rows_have_their_tables_columns() {
        let tags = [("a", "b")];
        let loc = Location::new(1, 2);
        let mut rows = Rows::default();
        rows.node(1, Meta::default(), 1, tags.into_iter(), Some(loc));
        assert_eq!(fields(&rows), [Table::Nodes.columns().split(", ").count()]);
        let mut rows = Rows::default();
        rows.way(
            1,
            Meta::default(),
            1,
            tags.into_iter(),
            [1, 2].into_iter(),
            Some(&[loc, loc]),
        );
        assert_eq!(fields(&rows), [Table::Ways.columns().split(", ").count()]);
        let mut rows = Rows::default();
        let members = [Member {
            kind: MemberType::Way,
            id: 3,
            role: "outer",
        }];
        rows.relation(1, Meta::default(), 1, tags.into_iter(), members.into_iter());
        assert_eq!(
            fields(&rows),
            [Table::Relations.columns().split(", ").count()]
        );
    }

    #[test]
    fn json_strings_are_escaped() {
        let mut b = Vec::new();
        json_str(&mut b, "a\"b\\c\n\u{1}é");
        assert_eq!(String::from_utf8(b).unwrap(), r#""a\"b\\c\u000a\u0001é""#);
    }

    #[test]
    fn rows_are_encoded() {
        let mut rows = Rows::default();
        rows.row(3).i64(-2).null().ids([7_i64, 8].into_iter());
        assert_eq!(rows.count, 1);
        let mut expected = vec![0, 3];
        expected.extend([
            0, 0, 0, 8, 255, 255, 255, 255, 255, 255, 255, 254, 255, 255, 255, 255,
        ]);
        expected.extend(44_i32.to_be_bytes());
        expected.extend([1_i32, 0, 20, 2, 1, 8].iter().flat_map(|v| v.to_be_bytes()));
        expected.extend(7_i64.to_be_bytes());
        expected.extend(8_i32.to_be_bytes());
        expected.extend(8_i64.to_be_bytes());
        assert_eq!(rows.buf, expected);
    }

    #[test]
    fn lines_are_twkb() {
        // A way of three nodes, and its bytes from PostGIS's ST_AsTWKB(geom, 7)
        let locs = [
            Location::new(423_734_493, -710_366_718),
            Location::new(423_733_120, -710_367_619),
            Location::new(423_733_207, -710_367_827),
        ];
        let mut rows = Rows::default();
        rows.line(Some(&locs));
        let twkb = &rows.buf[4..];
        let postgis = [
            0xe2, 0x00, 0x03, 0xfb, 0xd7, 0xba, 0xa5, 0x05, 0xba, 0xb3, 0x8d, 0x94, 0x03, 0x89,
            0x0e, 0xb9, 0x15, 0x9f, 0x03, 0xae, 0x01,
        ];
        assert_eq!(twkb, postgis);
        assert_eq!(twkb_locations(twkb), locs);
        let far = [
            Location::new(389_000_001, -770_000_002),
            Location::new(-900_000_000, 1_800_000_000),
        ];
        let mut rows = Rows::default();
        rows.line(Some(&far));
        assert_eq!(twkb_locations(&rows.buf[4..]), far);
        assert_eq!(twkb_locations(&[1, 2, 3]), []);
        // A repeated point is kept, as PostGIS's ST_AsTWKB would not; an ordinate beyond a location's is refused
        let repeated = [locs[0], locs[0], locs[1]];
        let mut rows = Rows::default();
        rows.line(Some(&repeated));
        assert_eq!(twkb_locations(&rows.buf[4..]), repeated);
        let mut big = Vec::new();
        write_scaled_line_string(&mut big, DIGITS, [[0, 0], [1 << 31, 0]].into_iter()).unwrap();
        assert_eq!(twkb_locations(&big), []);
    }

    #[test]
    fn lines_are_ewkb() {
        // Byte order, line string with an SRID, 4326, 2 points, then each point's x and y
        let locs = [Location::new(10, -20), Location::new(-30, 40)];
        let mut rows = Rows::new(GeometryFormat::Wkb);
        rows.line(Some(&locs));
        let mut ewkb = vec![1, 2, 0, 0, 0x20, 0xe6, 0x10, 0, 0, 2, 0, 0, 0];
        for loc in locs {
            ewkb.extend(loc.lon_degrees().to_le_bytes());
            ewkb.extend(loc.lat_degrees().to_le_bytes());
        }
        assert_eq!(rows.buf[4..], ewkb);
    }

    #[test]
    fn points_read_back_from_twkb() {
        // A point from PostGIS's ST_AsTWKB(geom, 7), and lines too short to be one
        let twkb = [
            0xe1, 0, 0xfb, 0xd7, 0xba, 0xa5, 0x05, 0xba, 0xb3, 0x8d, 0x94, 0x03,
        ];
        assert_eq!(
            twkb_locations(&twkb),
            [Location::new(423_734_493, -710_366_718)]
        );
        let mut short = Rows::default();
        short.line(Some(&[Location::default()]));
        assert_eq!(short.buf, (-1_i32).to_be_bytes());
    }
}
