//! Canonical text dumps of a PBF file, made with osmflow and with other crates, so their results can be compared line
//! by line. One line per object, in file order:
//!
//! - `n<id> <meta> T<tags> x<lon> y<lat>` for a node,
//! - `w<id> <meta> T<tags> N<refs>` and, with locations on ways, ` L<lon>/<lat>,...` for a way,
//! - `r<id> <meta> T<tags> M<type><id>@<role>,...` for a relation,
//!
//! where `<meta>` is `v<version> d<V|D> c<changeset> t<milliseconds> i<uid> u<user>` and coordinates are in
//! nanodegrees. Missing metadata is shown with the defaults osmflow uses: version -1, zeros, an empty user, visible.
//! Strings are escaped as in OPL: any character other than a letter, a digit or one of `-_.:/` becomes `%<hex>%`.
//!
//! The basic dump, for crates without metadata or with only 1e-7 degree precision, leaves out `<meta>`, sorts tags by
//! key, and gives coordinates in 1e-7 degrees.

#![allow(dead_code, reason = "each test uses a part")]

use std::fmt::{Display, Write as _};
use std::path::Path;

use osmflow::{Block, Info, MemberType, PbfReader, Select};

/// How an object without metadata is shown.
const NO_INFO: Info<'static> = Info {
    version: -1,
    timestamp_ms: 0,
    changeset: 0,
    uid: 0,
    user: "",
    visible: true,
};

pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_ascii_alphanumeric() || "-_.:/".contains(c) {
            out.push(c);
        } else {
            write!(out, "%{:x}%", u32::from(c)).unwrap();
        }
    }
    out
}

fn join<T: Display>(items: impl IntoIterator<Item = T>) -> String {
    let mut out = String::new();
    for (i, item) in items.into_iter().enumerate() {
        let sep = if i == 0 { "" } else { "," };
        write!(out, "{sep}{item}").unwrap();
    }
    out
}

/// The start of an object's line: its type and ID, its metadata unless `basic`, and its tags, sorted if `basic`.
fn head<'a>(
    kind: char,
    id: i64,
    info: Option<Info<'_>>,
    tags: impl Iterator<Item = (&'a str, &'a str)>,
    basic: bool,
) -> String {
    let mut tags: Vec<String> = tags
        .map(|(k, v)| format!("{}={}", escape(k), escape(v)))
        .collect();
    if basic {
        tags.sort();
        return format!("{kind}{id} T{}", tags.join(","));
    }
    let i = info.unwrap_or(NO_INFO);
    format!(
        "{kind}{id} v{} d{} c{} t{} i{} u{} T{}",
        i.version,
        if i.visible { 'V' } else { 'D' },
        i.changeset,
        i.timestamp_ms,
        i.uid,
        escape(i.user),
        tags.join(",")
    )
}

fn member(kind: char, id: i64, role: &str) -> String {
    format!("{kind}{id}@{}", escape(role))
}

/// The lines of one osmflow block.
pub fn osmflow_block(block: &Block, basic: bool) -> Vec<String> {
    let nodes = block.nodes().map(|n| {
        let head = head('n', n.id(), n.info(), n.tags(), basic);
        if basic {
            format!("{head} x{} y{}", n.decimicro_lon(), n.decimicro_lat())
        } else {
            format!("{head} x{} y{}", n.lon_nano(), n.lat_nano())
        }
    });
    let ways = block.ways().map(|w| {
        let mut line = format!(
            "{} N{}",
            head('w', w.id(), w.info(), w.tags(), basic),
            join(w.refs())
        );
        if !basic && let Some(locs) = w.locations() {
            write!(
                line,
                " L{}",
                join(locs.map(|(lat, lon)| format!("{lon}/{lat}")))
            )
            .unwrap();
        }
        line
    });
    let relations = block.relations().map(|r| {
        let members = r.members().map(|m| {
            let kind = match m.kind {
                MemberType::Node => 'n',
                MemberType::Way => 'w',
                MemberType::Relation => 'r',
            };
            member(kind, m.id, m.role)
        });
        format!(
            "{} M{}",
            head('r', r.id(), r.info(), r.tags(), basic),
            join(members)
        )
    });
    nodes.chain(ways).chain(relations).collect()
}

/// The file's header, as a few lines.
pub fn osmflow_header(file: &PbfReader) -> Vec<String> {
    let h = file.header();
    vec![
        format!("# required: {}", h.required_features.join(", ")),
        format!("# optional: {}", h.optional_features.join(", ")),
        format!(
            "# writing program: {:?}, source: {:?}",
            h.writing_program, h.source
        ),
        format!(
            "# bbox: {:?}",
            h.bbox.map(|b| (b.left, b.bottom, b.right, b.top))
        ),
        format!(
            "# replication: {:?} {:?} {:?}",
            h.replication_timestamp, h.replication_sequence, h.replication_base_url
        ),
    ]
}

/// The whole file, read on one thread.
pub fn osmflow(file: &PbfReader, basic: bool) -> osmflow::Result<Vec<String>> {
    let mut out = Vec::new();
    for block in file.blocks(Select::all())? {
        out.extend(osmflow_block(&block?, basic));
    }
    Ok(out)
}

/// The whole file, decoded in parallel on the default pool and read in order.
pub fn osmflow_ordered(file: &PbfReader) -> osmflow::Result<Vec<String>> {
    let mut out = Vec::new();
    for block in file.par_ordered(Select::all())? {
        out.extend(osmflow_block(&block?, false));
    }
    Ok(out)
}

fn osmpbf_info<'a>(i: &osmpbf::Info<'a>) -> Info<'a> {
    Info {
        version: i.version().unwrap_or(-1),
        timestamp_ms: i.milli_timestamp().unwrap_or(0),
        changeset: i.changeset().unwrap_or(0),
        uid: i.uid().unwrap_or(0),
        user: i.user().transpose().unwrap().unwrap_or(""),
        visible: i.visible(),
    }
}

/// The whole file, read with the `osmpbf` crate: elements come in file order, nodes, ways and relations of each block
/// in the order of their groups. osmflow returns a block's nodes, then its ways, then its relations, so each block's
/// lines are sorted by type.
pub fn osmpbf(path: &Path) -> Result<Vec<String>, osmpbf::Error> {
    use osmpbf::{BlobDecode, BlobReader, Element, RelMemberType};
    let mut out = Vec::new();
    for blob in BlobReader::from_path(path)? {
        let BlobDecode::OsmData(block) = blob?.decode()? else {
            continue;
        };
        let mut lines = Vec::new();
        block.for_each_element(|e| {
            lines.push(match e {
                Element::Node(n) => format!(
                    "{} x{} y{}",
                    head('n', n.id(), Some(osmpbf_info(&n.info())), n.tags(), false),
                    n.nano_lon(),
                    n.nano_lat()
                ),
                Element::DenseNode(n) => {
                    let info = n.info().map(|i| Info {
                        version: i.version(),
                        timestamp_ms: i.milli_timestamp(),
                        changeset: i.changeset(),
                        uid: i.uid(),
                        user: i.user().unwrap(),
                        visible: i.visible(),
                    });
                    format!(
                        "{} x{} y{}",
                        head('n', n.id(), info, n.tags(), false),
                        n.nano_lon(),
                        n.nano_lat()
                    )
                }
                Element::Way(w) => {
                    let head = head('w', w.id(), Some(osmpbf_info(&w.info())), w.tags(), false);
                    let mut line = format!("{head} N{}", join(w.refs()));
                    let mut locs = w
                        .node_locations()
                        .map(|l| format!("{}/{}", l.nano_lon(), l.nano_lat()))
                        .peekable();
                    if locs.peek().is_some() {
                        write!(line, " L{}", join(locs)).unwrap();
                    }
                    line
                }
                Element::Relation(r) => {
                    let members = r.members().map(|m| {
                        let kind = match m.member_type {
                            RelMemberType::Node => 'n',
                            RelMemberType::Way => 'w',
                            RelMemberType::Relation => 'r',
                        };
                        member(kind, m.member_id, m.role().unwrap())
                    });
                    let head = head('r', r.id(), Some(osmpbf_info(&r.info())), r.tags(), false);
                    format!("{head} M{}", join(members))
                }
            });
        });
        lines.sort_by_key(|l| match l.as_bytes()[0] {
            b'n' => 0,
            b'w' => 1,
            _ => 2,
        });
        out.extend(lines);
    }
    Ok(out)
}

/// The whole file, read with the `osmpbfreader` crate, as a basic dump.
pub fn osmpbfreader(path: &Path) -> Result<Vec<String>, osmpbfreader::Error> {
    use osmpbfreader::{OsmId, OsmObj, OsmPbfReader, Tags};
    let head = |kind, id, tags: &Tags| {
        head(
            kind,
            id,
            None,
            tags.iter().map(|(k, v)| (k.as_str(), v.as_str())),
            true,
        )
    };
    let mut reader = OsmPbfReader::new(std::fs::File::open(path)?);
    let mut out = Vec::new();
    for obj in reader.iter() {
        out.push(match obj? {
            OsmObj::Node(n) => format!(
                "{} x{} y{}",
                head('n', n.id.0, &n.tags),
                n.decimicro_lon,
                n.decimicro_lat
            ),
            OsmObj::Way(w) => format!(
                "{} N{}",
                head('w', w.id.0, &w.tags),
                join(w.nodes.iter().map(|n| n.0))
            ),
            OsmObj::Relation(r) => {
                let members = r.refs.iter().map(|m| match m.member {
                    OsmId::Node(id) => member('n', id.0, &m.role),
                    OsmId::Way(id) => member('w', id.0, &m.role),
                    OsmId::Relation(id) => member('r', id.0, &m.role),
                });
                format!("{} M{}", head('r', r.id.0, &r.tags), join(members))
            }
        });
    }
    Ok(out)
}

/// A stable 64-bit FNV-1a hash of the lines, to snapshot large dumps without storing them.
pub fn fnv(lines: &[String]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for line in lines {
        for &b in line.as_bytes().iter().chain(b"\n") {
            hash ^= u64::from(b);
            hash = hash.wrapping_mul(0x0100_0000_01b3);
        }
    }
    hash
}

/// The object types a block holds, as bits: 1 nodes, 2 ways, 4 relations.
fn types(block: &Block) -> u8 {
    u8::from(block.nodes().len() > 0)
        | u8::from(block.ways().len() > 0) << 1
        | u8::from(block.relations().len() > 0) << 2
}

/// The whole file, decoded in parallel on a pool of the caller's, unordered within each run of one object type, then
/// sorted back into file order by blob. Checks the barriers: consecutive blocks with the same types form a run, and a block's callback starts only
/// once the callbacks of every block before its run have returned. Every callback is slowed down, so a broken barrier
/// lets later blocks overtake earlier ones.
#[allow(
    clippy::panic_in_result_fn,
    reason = "the barrier check fails the test"
)]
pub fn osmflow_unordered(file: &PbfReader) -> osmflow::Result<Vec<String>> {
    static POOL: std::sync::OnceLock<std::sync::Arc<rayon::ThreadPool>> =
        std::sync::OnceLock::new();
    let pool = POOL.get_or_init(|| {
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(3)
            .build()
            .unwrap();
        std::sync::Arc::new(pool)
    });
    let file = file.clone().with_pool(std::sync::Arc::clone(pool));
    // The first blob of each blob's run, from a sequential read; a block with no objects joins the run it is in.
    let mut run_of = std::collections::BTreeMap::new();
    let (mut run_types, mut run_start) = (0, 0);
    for block in file.blocks(Select::all())? {
        let block = block?;
        let (blob, t) = (block.position().blob, types(&block));
        if t != 0 && t != run_types {
            (run_types, run_start) = (t, blob);
        }
        run_of.insert(blob, run_start);
    }
    let finished = std::sync::Mutex::new(std::collections::BTreeSet::new());
    let blocks = std::sync::Mutex::new(Vec::new());
    file.par_unordered(Select::all(), |block| {
        let blob = block.position().blob;
        let start = run_of[&blob];
        {
            let finished = finished.lock().unwrap();
            for &earlier in run_of.range(..start).map(|(b, _)| b) {
                assert!(
                    finished.contains(&earlier),
                    "blob {blob}, in the run from blob {start}, started before blob {earlier} finished"
                );
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
        let lines = osmflow_block(&block, false);
        blocks.lock().unwrap().push((blob, lines));
        finished.lock().unwrap().insert(blob);
        Ok(())
    })?;
    let mut blocks = blocks.into_inner().unwrap();
    blocks.sort_by_key(|(blob, _)| *blob);
    Ok(blocks.into_iter().flat_map(|(_, lines)| lines).collect())
}
