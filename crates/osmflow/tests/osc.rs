//! Reads osmChange XML, written here for one case each and from the files in `fixtures/osc` (described in its README),
//! and checks every change, every error, and that reading does not depend on how the input is split into reads.

use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::{self, Read, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use flate2::Compression;
use flate2::read::MultiGzDecoder;
use flate2::write::GzEncoder;
use osmflow::osc::{self, Action, Change, Object, OscError, OscReader, XmlError};
use osmflow::{MemberType, PbfReader, Select, Timestamp};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/osc")
        .join(name)
}

/// A string with its tabs and line breaks escaped, to keep a dump to one line.
fn esc(s: &str) -> String {
    s.replace('\t', "\\t")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

fn opt<T: ToString>(v: Option<T>) -> String {
    v.map_or_else(|| "-".to_owned(), |v| v.to_string())
}

/// One line per change: the action, the object's type and ID, its metadata (`-` where missing), tags, and its
/// location, node refs or members.
fn dump(change: &Change) -> String {
    let object = &change.object;
    let meta = object.meta();
    let kind = match object.kind() {
        MemberType::Node => 'n',
        MemberType::Way => 'w',
        MemberType::Relation => 'r',
    };
    let tags: Vec<String> = object
        .tags()
        .iter()
        .map(|(k, v)| format!("{}={}", esc(k), esc(v)))
        .collect();
    let mut line = format!(
        "{} {kind}{} v{} t{} c{} i{} u{} T{}",
        change.action,
        object.id(),
        opt(meta.version),
        opt(meta.timestamp.map(Timestamp::as_millis)),
        opt(meta.changeset),
        opt(meta.uid),
        opt(meta.user.as_deref().map(esc)),
        tags.join(","),
    );
    match object {
        Object::Node(node) => match node.location {
            Some(at) => write!(line, " x{},{}", at.lat, at.lon).unwrap(),
            None => line += " x-",
        },
        Object::Way(way) => {
            let refs: Vec<String> = way.refs.iter().map(ToString::to_string).collect();
            write!(line, " N{}", refs.join(",")).unwrap();
        }
        Object::Relation(relation) => {
            let members: Vec<String> = relation
                .members()
                .map(|m| format!("{:?}{}@{}", m.kind, m.id, m.role))
                .collect();
            write!(line, " M{}", members.join(",")).unwrap();
        }
    }
    line
}

fn read(xml: &[u8]) -> Vec<String> {
    OscReader::new(xml).map(|c| dump(&c.unwrap())).collect()
}

fn read_file(path: &Path) -> Vec<String> {
    OscReader::open(path)
        .unwrap()
        .map(|c| dump(&c.unwrap()))
        .collect()
}

fn assert_dump(xml: &str, expected: &str) {
    let lines: Vec<&str> = expected
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    assert_eq!(read(xml.as_bytes()), lines);
}

/// Create, modify and delete of nodes, ways and relations, with all metadata; sections that repeat and interleave;
/// empty sections; `visible` agreeing with the section; tags before and after way nodes and relation members; every
/// member type and an empty role.
const ALL_ACTIONS: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!-- Every action on every object type, with full metadata, in interleaved sections. -->
<osmChange version="0.6" generator="hand-written">
  <create>
    <node id="1" version="1" timestamp="2026-11-08T08:00:13Z" uid="42" user="alice" changeset="100" lat="51.5073219" lon="-0.1276474">
      <tag k="amenity" v="cafe"/>
      <tag k="name" v="Corner"/>
    </node>
    <node id="2" version="1" timestamp="2026-11-08T08:00:13Z" uid="42" user="alice" changeset="100" lat="51.5" lon="-0.13"/>
    <way id="10" version="1" timestamp="2026-11-08T08:00:14Z" uid="42" user="alice" changeset="100">
      <nd ref="1"/>
      <nd ref="2"/>
      <tag k="highway" v="footway"/>
    </way>
    <relation id="100" version="1" timestamp="2026-11-08T08:00:15Z" uid="42" user="alice" changeset="100">
      <member type="node" ref="1" role="stop"/>
      <member type="way" ref="10" role=""/>
      <member type="relation" ref="101" role="sub"/>
      <tag k="type" v="route"/>
    </relation>
  </create>
  <modify>
    <node id="1" version="2" timestamp="2026-11-08T08:01:00Z" uid="7" user="bob" changeset="101" lat="51.5073220" lon="-0.1276475" visible="true">
      <tag k="amenity" v="cafe"/>
    </node>
    <way id="10" version="2" timestamp="2026-11-08T08:01:00Z" uid="7" user="bob" changeset="101">
      <tag k="highway" v="path"/>
      <nd ref="2"/>
      <nd ref="1"/>
    </way>
  </modify>
  <delete>
    <relation id="100" version="2" timestamp="2026-11-08T08:02:00Z" uid="7" user="bob" changeset="102"/>
  </delete>
  <modify>
    <relation id="101" version="5" timestamp="2026-11-08T08:02:00Z" uid="7" user="bob" changeset="102">
      <tag k="type" v="multipolygon"/>
      <member type="way" ref="10" role="outer"/>
    </relation>
  </modify>
  <delete>
    <way id="10" version="3" timestamp="2026-11-08T08:03:00Z" uid="7" user="bob" changeset="103" visible="false"/>
    <node id="2" version="2" timestamp="2026-11-08T08:03:00Z" uid="7" user="bob" changeset="103" lat="51.5" lon="-0.13"/>
  </delete>
  <create/>
  <delete>
  </delete>
</osmChange>
"#;

#[test]
fn all_actions() {
    assert_dump(
        ALL_ACTIONS,
        "
        create n1 v1 t1794124813000 c100 i42 ualice Tamenity=cafe,name=Corner x515073219,-1276474
        create n2 v1 t1794124813000 c100 i42 ualice T x515000000,-1300000
        create w10 v1 t1794124814000 c100 i42 ualice Thighway=footway N1,2
        create r100 v1 t1794124815000 c100 i42 ualice Ttype=route MNode1@stop,Way10@,Relation101@sub
        modify n1 v2 t1794124860000 c101 i7 ubob Tamenity=cafe x515073220,-1276475
        modify w10 v2 t1794124860000 c101 i7 ubob Thighway=path N2,1
        delete r100 v2 t1794124920000 c102 i7 ubob T M
        modify r101 v5 t1794124920000 c102 i7 ubob Ttype=multipolygon MWay10@outer
        delete w10 v3 t1794124980000 c103 i7 ubob T N
        delete n2 v2 t1794124980000 c103 i7 ubob T x515000000,-1300000
        ",
    );
}

/// Objects with some or none of version, timestamp, changeset, user ID and user name, as in Geofabrik's public diffs;
/// a deleted node with only its ID.
#[test]
fn no_metadata() {
    assert_dump(
        r#"<?xml version='1.0' encoding='UTF-8'?>
        <osmChange version="0.6" generator="hand-written">
          <modify>
            <node id="1" version="3" timestamp="2026-11-08T08:00:00Z" lat="1" lon="2"/>
            <node id="2" lat="1" lon="2"/>
            <way id="3"/>
            <relation id="4"/>
          </modify>
          <delete>
            <node id="5"/>
          </delete>
        </osmChange>"#,
        "
        modify n1 v3 t1794124800000 c- i- u- T x10000000,20000000
        modify n2 v- t- c- i- u- T x10000000,20000000
        modify w3 v- t- c- i- u- T N
        modify r4 v- t- c- i- u- T M
        delete n5 v- t- c- i- u- T x-
        ",
    );
}

/// Deleted nodes with and without a location, with tags, with `visible="false"`; a deleted way without nodes.
#[test]
fn deleted_nodes() {
    assert_dump(
        r#"<osmChange version="0.6"><delete>
        <node id="1" version="2" timestamp="2026-11-08T07:59:36Z" uid="1" user="u" changeset="1" lat="-31.8771988" lon="115.7608061"/>
        <node id="2" version="2" timestamp="2026-11-08T07:59:36Z" uid="1" user="u" changeset="1"/>
        <node id="3" version="4" visible="false"/>
        <node id="4" version="2" lat="0" lon="0"><tag k="note" v="tags on a deleted node are kept"/></node>
        <way id="5" version="2"/>
        </delete></osmChange>"#,
        "
        delete n1 v2 t1794124776000 c1 i1 uu T x-318771988,1157608061
        delete n2 v2 t1794124776000 c1 i1 uu T x-
        delete n3 v4 t- c- i- u- T x-
        delete n4 v2 t- c- i- u- Tnote=tags on a deleted node are kept x0,0
        delete w5 v2 t- c- i- u- T N
        ",
    );
}

/// The five predefined entities, decimal and hexadecimal character references, a reference to an ampersand that must
/// stay escaped, a literal tab and line break normalized to spaces, non-ASCII text, single-quoted values, white space
/// around `=` and before `>`, `>` inside a value, an empty key and value, comments and processing instructions, and
/// children written as start and end tags.
const ESCAPES: &str = concat!(
    r#"<?xml version="1.0" encoding="UTF-8"?>
<?xml-stylesheet type="text/xsl" href="ignored.xsl"?>
<osmChange version='0.6'>
  <!-- a comment with <markup> & "quotes" -->
  <create>
    <node id = '1' version="1" user="O&apos;Brien &amp; &quot;Co&quot;" lat='0' lon='0' >
      <tag k="name" v="a &lt;b&gt; &amp;amp; c"/>
      <tag k="name:ru" v="Москва"/>
      <tag k="chars" v="&#65;&#x42;&#x1f600;"/>
      <tag k="space" v="tab"#,
    "\t",
    r#"newline
end"/>
      <tag k="refs" v="&#9;&#10;&#13;"/>
      <tag k="quotes" v='single "double"'/>
      <tag k="" v=""/>
      <tag k="gt" v="a > b"/>
    </node>
    <node id="2" version="1" user="O&apos;Brien &amp; &quot;Co&quot;" lat="0" lon="0"><!-- a comment inside --></node>
    <node id="3" version="1" user="O&amp;apos;Brien &amp;amp; &amp;quot;Co&amp;quot;" lat="0" lon="0"/>
    <way id="4"><nd ref="1"></nd><tag k="a" v="b"></tag></way>
  </create>
</osmChange>
"#
);

#[test]
fn escapes() {
    assert_dump(
        ESCAPES,
        "
        create n1 v1 t- c- i- uO'Brien & \"Co\" Tname=a <b> &amp; c,name:ru=Москва,chars=AB😀,space=tab newline end,\
         refs=\\t\\n\\r,quotes=single \"double\",=,gt=a > b x0,0
        create n2 v1 t- c- i- uO'Brien & \"Co\" T x0,0
        create n3 v1 t- c- i- uO&apos;Brien &amp; &quot;Co&quot; T x0,0
        create w4 v- t- c- i- u- Ta=b N1
        ",
    );
}

/// Coordinates at ±90 and ±180, with fewer than seven decimals, with an eighth decimal rounding half away from zero,
/// and rounding up to ±90° and ±180°; `-0`, `.5` and `1.`; a negative ID.
const COORDINATES: &str = r#"<osmChange version="0.6">
  <create>
    <node id="1" lat="0" lon="0"/>
    <node id="2" lat="90" lon="180"/>
    <node id="3" lat="-90" lon="-180"/>
    <node id="4" lat="-90.0000000" lon="-180.0000000"/>
    <node id="5" lat="1.23456785" lon="-1.23456785"/>
    <node id="6" lat="1.23456784999" lon="-1.234567849"/>
    <node id="7" lat="0.00000005" lon="-0.00000004"/>
    <node id="8" lat="89.99999995" lon="-179.99999995"/>
    <node id="9" lat="-0" lon=".5"/>
    <node id="10" lat="12.3" lon="1."/>
    <node id="-11" lat="45.0000001" lon="-45.0000001"/>
  </create>
</osmChange>
"#;

#[test]
fn coordinates() {
    assert_dump(
        COORDINATES,
        "
        create n1 v- t- c- i- u- T x0,0
        create n2 v- t- c- i- u- T x900000000,1800000000
        create n3 v- t- c- i- u- T x-900000000,-1800000000
        create n4 v- t- c- i- u- T x-900000000,-1800000000
        create n5 v- t- c- i- u- T x12345679,-12345679
        create n6 v- t- c- i- u- T x12345678,-12345678
        create n7 v- t- c- i- u- T x1,0
        create n8 v- t- c- i- u- T x900000000,-1800000000
        create n9 v- t- c- i- u- T x0,5000000
        create n10 v- t- c- i- u- T x123000000,10000000
        create n-11 v- t- c- i- u- T x450000001,-450000001
        ",
    );
}

#[test]
fn empty() {
    assert_dump(r#"<osmChange version="0.6" generator="empty"/>"#, "");
}

#[test]
fn users_are_shared() {
    let changes: Vec<Change> = OscReader::new(ALL_ACTIONS.as_bytes())
        .collect::<Result<_, _>>()
        .unwrap();
    let user = |i: usize| Arc::clone(changes[i].object.meta().user.as_ref().unwrap());
    assert!(Arc::ptr_eq(&user(0), &user(3)));
    assert!(Arc::ptr_eq(&user(4), &user(9)));
    assert!(!Arc::ptr_eq(&user(3), &user(4)));
}

/// A user name equal, byte for byte, to the text of the previous one is still read by XML's rules.
#[test]
fn shared_users_keep_xml_rules() {
    let xml = "<osmChange><create>\
        <node id='1' lat='0' lon='0' user='a&#9;b'/><node id='2' lat='0' lon='0' user='a\tb'/>\
        <node id='3' lat='0' lon='0' user='a&lt;b'/><node id='4' lat='0' lon='0' user='a<b'/>\
        </create></osmChange>";
    let mut changes = OscReader::new(xml.as_bytes());
    let mut user = || {
        changes
            .next()
            .unwrap()
            .map(|c| c.object.meta().user.as_deref().map(str::to_owned))
    };
    assert_eq!(user().unwrap().as_deref(), Some("a\tb"));
    assert_eq!(user().unwrap().as_deref(), Some("a b"));
    assert_eq!(user().unwrap().as_deref(), Some("a<b"));
    user().unwrap_err();
}

/// The text of the first object of each type in a file, from a plain scan: (`<node `, `<way `, `<relation `, `<tag `,
/// `<nd `, `<member `) counts.
fn count_elements(xml: &str) -> [usize; 6] {
    ["<node ", "<way ", "<relation ", "<tag ", "<nd ", "<member "].map(|e| xml.matches(e).count())
}

#[test]
fn planet_minute_diff() {
    let path = fixture("planet/minute/007/363/600.osc.gz");
    let mut xml = String::new();
    MultiGzDecoder::new(File::open(&path).unwrap())
        .read_to_string(&mut xml)
        .unwrap();
    let changes: Vec<Change> = OscReader::open(&path)
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    let mut counts = [0; 6];
    for c in &changes {
        match &c.object {
            Object::Node(_) => counts[0] += 1,
            Object::Way(w) => {
                counts[1] += 1;
                counts[4] += w.refs.len();
            }
            Object::Relation(r) => {
                counts[2] += 1;
                counts[5] += r.members().len();
            }
        }
        counts[3] += c.object.tags().len();
        let m = c.object.meta();
        // The planet's diffs have every field.
        assert!(
            m.version.is_some() && m.timestamp.is_some() && m.changeset.is_some(),
            "{}",
            dump(c)
        );
        assert!(m.uid.is_some() && m.user.is_some(), "{}", dump(c));
        if let Object::Node(n) = &c.object {
            assert!(n.location.is_some(), "{}", dump(c));
        }
    }
    assert_eq!(counts, count_elements(&xml));
    let in_section = |action: &str| {
        xml.split(&format!("<{action}>"))
            .skip(1)
            .map(|s| {
                count_elements(s.split(&format!("</{action}>")).next().unwrap())[..3]
                    .iter()
                    .sum::<usize>()
            })
            .sum::<usize>()
    };
    for action in [Action::Create, Action::Modify, Action::Delete] {
        let n = changes.iter().filter(|c| c.action == action).count();
        assert_eq!(n, in_section(action.name()), "{action}");
    }
    assert_eq!(
        dump(&changes[0]),
        "modify n247756562 v10 t1794124813000 c191634392 i4887311 umapperue Tname=Valtice město,\
         network=Integrovaný dopravní systém Jihomoravského kraje,network:short=IDS JMK,network:wikidata=Q12020731,\
         operator=Správa železnic,operator:short=SŽ,operator:wikidata=Q1848025,public_transport=stop_position,\
         railway=stop,train=yes x487486381,167620403"
    );
    let deleted = changes
        .iter()
        .find(|c| c.object.id() == 4_133_149_616)
        .unwrap();
    assert_eq!(
        dump(deleted),
        "delete n4133149616 v2 t1794124776000 c191634374 i23236272 uMammothmaker T x-318771988,1157608061"
    );
}

/// A reader that hands out at most `chunk` bytes per read.
struct Chunked<'a> {
    data: &'a [u8],
    chunk: usize,
}

impl Read for Chunked<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.chunk.min(buf.len()).min(self.data.len());
        buf[..n].copy_from_slice(&self.data[..n]);
        self.data = &self.data[n..];
        Ok(n)
    }
}

fn dump_all(input: impl Read) -> Vec<String> {
    OscReader::new(input)
        .map(|c| c.map_or_else(|e| e.to_string(), |c| dump(&c)))
        .collect()
}

#[test]
fn reads_do_not_matter() {
    let valid = [ALL_ACTIONS, ESCAPES, COORDINATES].map(str::as_bytes);
    let malformed = MALFORMED.iter().map(|m| m.1);
    for xml in valid.into_iter().chain(malformed) {
        let expected = dump_all(xml);
        for chunk in [1, 2, 3, 7, 64] {
            assert_eq!(
                dump_all(Chunked { data: xml, chunk }),
                expected,
                "{} in reads of {chunk}",
                String::from_utf8_lossy(xml)
            );
        }
    }
}

fn gzip(members: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::new();
    for member in members {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(member).unwrap();
        out.extend(encoder.finish().unwrap());
    }
    out
}

#[test]
fn gzip_files() {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("osc-gzip");
    fs::create_dir_all(&dir).unwrap();
    let xml = ALL_ACTIONS.as_bytes();
    let expected = read(xml);

    let one = dir.join("one.osc.gz");
    fs::write(&one, gzip(&[xml])).unwrap();
    assert_eq!(read_file(&one), expected);

    // A file of several gzip members is read whole, as `zcat` reads it.
    let (head, tail) = xml.split_at(xml.len() / 2);
    let several = dir.join("several.osc.gz");
    fs::write(&several, gzip(&[head, tail])).unwrap();
    assert_eq!(read_file(&several), expected);

    // A file named .osc is not decompressed.
    let plain = dir.join("plain.osc");
    fs::write(&plain, xml).unwrap();
    assert_eq!(read_file(&plain), expected);

    // A cut gzip file is an I/O error naming it.
    let cut = dir.join("cut.osc.gz");
    let gz = gzip(&[xml]);
    fs::write(&cut, &gz[..gz.len() - 10]).unwrap();
    let error = OscReader::open(&cut)
        .unwrap()
        .find_map(Result::err)
        .unwrap();
    assert!(
        matches!(&error, OscError::File { path, .. } if path == &cut),
        "{error}"
    );

    let missing = dir.join("missing.osc");
    let Err(OscError::File { path, source }) = OscReader::open(&missing) else {
        panic!("a missing file opened");
    };
    assert_eq!((path, source.kind()), (missing, io::ErrorKind::NotFound));

    // An error in a file names it.
    let bad = dir.join("bad.osc");
    fs::write(&bad, MALFORMED[0].1).unwrap();
    let error = OscReader::open(&bad)
        .unwrap()
        .find_map(Result::err)
        .unwrap();
    assert!(
        error
            .to_string()
            .starts_with(&format!("{}: byte ", bad.display())),
        "{error}"
    );
}

/// Malformed osmChange XML, one error each: a name, the XML, the error it gives, the element named, and the text the
/// error's offset points at (empty for the end of the input).
type Malformed = (
    &'static str,
    &'static [u8],
    XmlError,
    Option<&'static str>,
    &'static str,
);

const MALFORMED: &[Malformed] = &[
    (
        "doctype",
        br#"<?xml version="1.0"?><!DOCTYPE osmChange [<!ENTITY lol "lol">]><osmChange/>"#,
        XmlError::Declaration,
        None,
        "<!DOCTYPE",
    ),
    (
        "entity",
        br#"<osmChange><!ENTITY x "y"></osmChange>"#,
        XmlError::Declaration,
        None,
        "<!ENTITY",
    ),
    (
        "cdata",
        br"<osmChange><create><![CDATA[x]]></create></osmChange>",
        XmlError::Declaration,
        None,
        "<![CDATA[",
    ),
    (
        "undefined-entity",
        br#"<osmChange><create><node id="1" lat="0" lon="0"><tag k="a" v="&lol;"/></node></create></osmChange>"#,
        XmlError::Escape,
        Some("tag"),
        "<tag",
    ),
    (
        "truncated",
        br#"<osmChange><create><node id="1" lat="0" lon="0"/><node id="2" lat="0" lo"#,
        XmlError::Truncated,
        None,
        "<node id=\"2\"",
    ),
    (
        "unclosed",
        br#"<osmChange><create><node id="1" lat="0" lon="0"/></create>"#,
        XmlError::Truncated,
        None,
        "",
    ),
    (
        "no-root",
        br"",
        XmlError::Truncated,
        None,
        "",
    ),
    (
        "wrong-root",
        br#"<osm version="0.6"/>"#,
        XmlError::Unexpected("as the root element, which must be <osmChange>"),
        Some("osm"),
        "<osm",
    ),
    (
        "bounds",
        br#"<osmChange><bounds minlat="0" minlon="0" maxlat="1" maxlon="1"/></osmChange>"#,
        XmlError::Unexpected("in <osmChange>, which holds only <create>, <modify> and <delete>"),
        Some("bounds"),
        "<bounds",
    ),
    (
        "changeset",
        br#"<osmChange><modify><changeset id="1"/></modify></osmChange>"#,
        XmlError::Unexpected("in an action section, which holds only <node>, <way> and <relation>"),
        Some("changeset"),
        "<changeset",
    ),
    (
        "nd-in-node",
        br#"<osmChange><modify><node id="1" lat="0" lon="0"><nd ref="2"/></node></modify></osmChange>"#,
        XmlError::Unexpected("in <node>, which holds only <tag>"),
        Some("nd"),
        "<nd",
    ),
    (
        "member-in-way",
        br#"<osmChange><modify><way id="1"><member type="node" ref="2" role=""/></way></modify></osmChange>"#,
        XmlError::Unexpected("in <way>, which holds only <tag> and <nd>"),
        Some("member"),
        "<member",
    ),
    (
        "nested-tag",
        br#"<osmChange><modify><way id="1"><tag k="a" v="b"><tag k="c" v="d"/></tag></way></modify></osmChange>"#,
        XmlError::Unexpected("where an end tag was expected"),
        Some("tag"),
        "<tag k=\"c\"",
    ),
    (
        "text",
        br"<osmChange><modify>text</modify></osmChange>",
        XmlError::Text,
        None,
        "text",
    ),
    (
        "mismatched",
        br#"<osmChange><modify><way id="1"></node></modify></osmChange>"#,
        XmlError::Mismatched("way"),
        Some("node"),
        "</node>",
    ),
    (
        "wrong-section-end",
        br"<osmChange><modify></create></osmChange>",
        XmlError::Mismatched("modify"),
        Some("create"),
        "</create>",
    ),
    (
        "after-root",
        br#"<osmChange></osmChange><osmChange version="again"/>"#,
        XmlError::Unexpected("after the end of <osmChange>"),
        Some("osmChange"),
        "<osmChange version=\"again\"",
    ),
    (
        "missing-id",
        br#"<osmChange><create><way version="1"/></create></osmChange>"#,
        XmlError::MissingAttribute("id"),
        Some("way"),
        "<way",
    ),
    (
        "bad-id",
        br#"<osmChange><create><way id="12a"/></create></osmChange>"#,
        XmlError::Value { attribute: "id", value: String::new() },
        Some("way"),
        "<way",
    ),
    (
        "id-overflow",
        br#"<osmChange><create><way id="9223372036854775808"/></create></osmChange>"#,
        XmlError::Value { attribute: "id", value: String::new() },
        Some("way"),
        "<way",
    ),
    (
        "bad-version",
        br#"<osmChange><create><way id="1" version="2147483648"/></create></osmChange>"#,
        XmlError::Value { attribute: "version", value: String::new() },
        Some("way"),
        "<way",
    ),
    (
        "bad-ref",
        br#"<osmChange><create><way id="1"><nd ref=""/></way></create></osmChange>"#,
        XmlError::Value { attribute: "ref", value: String::new() },
        Some("nd"),
        "<nd",
    ),
    (
        "bad-lat",
        br#"<osmChange><create><node id="1" lat="1e5" lon="0"/></create></osmChange>"#,
        XmlError::Value { attribute: "lat", value: String::new() },
        Some("node"),
        "<node",
    ),
    (
        "lon-overflow",
        br#"<osmChange><create><node id="1" lat="0" lon="214.7483648"/></create></osmChange>"#,
        XmlError::Value { attribute: "lon", value: String::new() },
        Some("node"),
        "<node",
    ),
    (
        "lat-past-90",
        br#"<osmChange><create><node id="1" lat="90.0000001" lon="0"/></create></osmChange>"#,
        XmlError::Value { attribute: "lat", value: String::new() },
        Some("node"),
        "<node",
    ),
    (
        "lon-past-180",
        br#"<osmChange><create><node id="1" lat="0" lon="-180.0000001"/></create></osmChange>"#,
        XmlError::Value { attribute: "lon", value: String::new() },
        Some("node"),
        "<node",
    ),
    (
        "bad-timestamp",
        br#"<osmChange><create><way id="1" timestamp="2026-02-29T00:00:00Z"/></create></osmChange>"#,
        XmlError::Value { attribute: "timestamp", value: String::new() },
        Some("way"),
        "<way",
    ),
    (
        "bad-member-type",
        br#"<osmChange><create><relation id="1"><member type="area" ref="1" role=""/></relation></create></osmChange>"#,
        XmlError::Value { attribute: "type", value: String::new() },
        Some("member"),
        "<member",
    ),
    (
        "bad-visible",
        br#"<osmChange><delete><way id="1" visible="no"/></delete></osmChange>"#,
        XmlError::Value { attribute: "visible", value: String::new() },
        Some("way"),
        "<way",
    ),
    (
        "missing-tag-value",
        br#"<osmChange><create><way id="1"><tag k="a"/></way></create></osmChange>"#,
        XmlError::MissingAttribute("v"),
        Some("tag"),
        "<tag",
    ),
    (
        "duplicate-attribute",
        br#"<osmChange><create><way id="1" id="2"/></create></osmChange>"#,
        XmlError::DuplicateAttribute("id"),
        Some("way"),
        "<way",
    ),
    (
        "no-location",
        br#"<osmChange><modify><node id="1"><tag k="a" v="b"/><tag k="c" v="d"/></node></modify></osmChange>"#,
        XmlError::NoLocation,
        Some("node"),
        "<node",
    ),
    (
        "half-location",
        br#"<osmChange><delete><node id="1" lat="1"/></delete></osmChange>"#,
        XmlError::HalfLocation,
        Some("node"),
        "<node",
    ),
    (
        "unquoted",
        br"<osmChange><create><way id=1/></create></osmChange>",
        XmlError::Syntax("expected a quoted attribute value"),
        Some("way"),
        "<way",
    ),
    (
        "no-space-between-attributes",
        br#"<osmChange><create><way id="1"version="1"/></create></osmChange>"#,
        XmlError::Syntax("expected white space before an attribute"),
        Some("way"),
        "<way",
    ),
    (
        "invalid-utf8",
        b"<osmChange><create><way id=\"1\"><tag k=\"a\" v=\"\xFF\"/></way></create></osmChange>",
        XmlError::Utf8,
        Some("tag"),
        "<tag",
    ),
    (
        "control-char",
        b"<osmChange><create><way id=\"1\"><tag k=\"a\" v=\"x\x01y\"/></way></create></osmChange>",
        XmlError::Escape,
        Some("tag"),
        "<tag",
    ),
    (
        "lt-in-value",
        br#"<osmChange><create><way id="1"><tag k="a" v="a<b"/></way></create></osmChange>"#,
        XmlError::Escape,
        Some("tag"),
        "<tag",
    ),
];

/// A [`XmlError::Value`] is compared by its attribute only.
fn same_kind(a: &XmlError, b: &XmlError) -> bool {
    match (a, b) {
        (XmlError::Value { attribute: a, .. }, XmlError::Value { attribute: b, .. }) => a == b,
        _ => a == b,
    }
}

#[test]
fn malformed() {
    for (name, xml, kind, element, at) in MALFORMED {
        let mut reader = OscReader::new(*xml);
        let error = reader
            .find_map(Result::err)
            .unwrap_or_else(|| panic!("{name} has no error"));
        let OscError::Xml {
            position,
            kind: found,
        } = &error
        else {
            panic!("{name}: {error}");
        };
        assert!(
            same_kind(found, kind),
            "{name}: {found:?}, expected {kind:?}"
        );
        assert_eq!(position.file, None, "{name}");
        assert_eq!(position.element.as_deref(), *element, "{name}");
        let offset = if at.is_empty() {
            xml.len()
        } else {
            xml.windows(at.len())
                .position(|w| w == at.as_bytes())
                .unwrap()
        };
        assert_eq!(position.offset, offset as u64, "{name}: {error}");
        assert!(
            reader.next().is_none(),
            "{name}: the reader stops after an error"
        );
        assert!(
            error.to_string().starts_with(&format!("byte {offset}")),
            "{error}"
        );
    }
}

/// An object with more node refs or tags than any real one is an error, before it takes much memory.
#[test]
fn objects_with_too_many_items() {
    let too_many = |item: &str| {
        let items = item.repeat((1 << 20) + 1);
        format!("<osmChange><create><way id='1'>{items}</way></create></osmChange>")
    };
    for xml in [too_many("<nd ref='1'/>"), too_many("<tag k='' v=''/>")] {
        let error = OscReader::new(xml.as_bytes())
            .find_map(Result::err)
            .unwrap();
        assert!(
            matches!(
                error,
                OscError::Xml {
                    kind: XmlError::TooLarge,
                    ..
                }
            ),
            "{error}"
        );
    }
}

#[test]
fn large_elements() {
    // An element larger than the read buffer grows it, up to 16 MiB.
    let doc = |len: usize| {
        format!(
            r#"<osmChange><create><way id="1"><tag k="k" v="{}"/></way></create></osmChange>"#,
            "x".repeat(len)
        )
    };
    let big = doc(1 << 20);
    let changes: Vec<Change> = OscReader::new(big.as_bytes())
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        changes[0].object.tags().get("k").map(str::len),
        Some(1 << 20)
    );

    let huge = doc(16 << 20);
    let error = OscReader::new(huge.as_bytes())
        .find_map(Result::err)
        .unwrap();
    assert!(
        matches!(
            error,
            OscError::Xml {
                kind: XmlError::TooLarge,
                ..
            }
        ),
        "{error}"
    );
}

#[test]
fn locations_equal_pbf() {
    // Every node of a PBF file, written in degrees with 9 decimals (nanodegrees, as PBF stores them), reads back as
    // the same 1e-7 degrees.
    let pbf =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/planetiler/monaco-latest.osm.pbf");
    let degrees = |nano: i64| {
        let sign = if nano < 0 { "-" } else { "" };
        format!(
            "{sign}{}.{:09}",
            nano.unsigned_abs() / 1_000_000_000,
            nano.unsigned_abs() % 1_000_000_000
        )
    };
    let mut xml = String::from("<osmChange><create>");
    let mut expected = Vec::new();
    for block in PbfReader::open(pbf)
        .unwrap()
        .blocks(Select::node_locations())
        .unwrap()
    {
        for node in block.unwrap().nodes() {
            let (lat, lon) = (degrees(node.lat_nano()), degrees(node.lon_nano()));
            write!(xml, r#"<node id="{}" lat="{lat}" lon="{lon}"/>"#, node.id()).unwrap();
            expected.push((node.id(), node.decimicro_lat(), node.decimicro_lon()));
        }
    }
    xml += "</create></osmChange>";
    let found: Vec<(i64, i32, i32)> = OscReader::new(xml.as_bytes())
        .map(|c| match c.unwrap().object {
            Object::Node(n) => (n.id, n.location.unwrap().lat, n.location.unwrap().lon),
            Object::Way(_) | Object::Relation(_) => unreachable!(),
        })
        .collect();
    assert!(expected.len() > 10_000);
    assert_eq!(found, expected);
}

/// The change files and state files of other projects and of the replication servers, under `fixtures/osc/<source>/`.
fn others(suffixes: &[&str]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut dirs = vec![Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/osc")];
    while let Some(dir) = dirs.pop() {
        for entry in fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            if path.is_dir() {
                dirs.push(path);
            } else if suffixes.iter().any(|s| name.ends_with(s)) {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

#[test]
fn change_files_from_elsewhere_read() {
    let files = others(&[".osc", ".osc.gz"]);
    assert!(files.len() > 10);
    for path in files {
        let changes = OscReader::open(&path)
            .unwrap()
            .collect::<Result<Vec<_>, OscError>>()
            .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let empty =
            path.to_string_lossy().contains("empty") || path.to_string_lossy().contains("niue");
        assert!(
            empty || !changes.is_empty(),
            "{} has no changes",
            path.display()
        );
    }
}

#[test]
fn state_files_from_elsewhere_read() {
    let files = others(&["state.txt"]);
    assert!(files.len() > 5);
    for path in files {
        osc::State::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    }
}

/// Gives its bytes one at a time.
struct OneByte<'a>(&'a [u8]);

impl Read for OneByte<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let Some((&first, rest)) = self.0.split_first() else {
            return Ok(0);
        };
        buf[0] = first;
        self.0 = rest;
        Ok(1)
    }
}

#[test]
fn a_byte_order_mark_is_skipped() {
    let xml = b"\xEF\xBB\xBF<?xml version='1.0'?><osmChange><create><node id='1' lat='1' lon='2'/></create></osmChange>";
    for changes in [
        OscReader::new(&xml[..]).collect::<Result<Vec<_>, OscError>>(),
        OscReader::new(OneByte(xml)).collect(),
    ] {
        assert_eq!(changes.unwrap().len(), 1);
    }
}
