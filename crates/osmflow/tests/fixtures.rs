//! Reads every `*.pbf` file under `fixtures/`, and checks that:
//!
//! - osmflow reads the same objects on one thread, and in parallel ordered and unordered (whose barriers at every
//!   change of object type are checked as it reads);
//! - the `osmpbf` crate reads the same objects, metadata included;
//! - the `osmpbfreader` crate reads the same objects, tags and coordinates (it has no metadata, and history files are
//!   left out, as it drops deleted objects);
//! - the result matches the snapshot in `tests/snapshots/`: the whole dump for small files, its counts, hash and first
//!   lines for large ones. A file osmflow cannot read snapshots its error, and what `osmpbf` does with it.

mod common;

use std::fmt::Write as _;
use std::panic;
use std::path::Path;

use osmflow::{PbfReader, ReadMode, Select};
use test_each_file::test_each_path;

test_each_path! { for ["pbf"] in "crates/osmflow/fixtures" => fixture }

/// Dumps longer than this are snapshotted by their counts, hash and first lines.
const FULL_SNAPSHOT_LINES: usize = 200;

/// Fixtures another reader gets wrong: (snapshot name, reader, what it does). They are not compared with that
/// reader, and their snapshot says why.
const MISREAD: &[(&str, &str, &str)] = &[
    (
        "generated__split_messages.osm.pbf",
        "osmpbf",
        LAST_OCCURRENCE,
    ),
    (
        "generated__split_messages.osm.pbf",
        "osmpbfreader",
        LAST_OCCURRENCE,
    ),
    (
        "generated__split_string_table_zlib.osm.pbf",
        "osmpbf",
        LAST_OCCURRENCE,
    ),
    (
        "generated__split_string_table_zlib.osm.pbf",
        "osmpbfreader",
        LAST_OCCURRENCE,
    ),
];
const LAST_OCCURRENCE: &str =
    "it keeps only the last occurrence of a message field, where protobuf merges them";

/// Neither `osmpbf` nor `osmpbfreader` merges message fields, so the fixture that splits them is checked against the
/// objects it encodes instead: those of `packed.osm.pbf`, its nodes twice and then its way.
#[test]
fn split_messages_are_merged() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/generated");
    let read =
        |name: &str| common::osmflow(&PbfReader::open(dir.join(name)).unwrap(), false).unwrap();
    let packed = read("packed.osm.pbf");
    let of_type = |kind: char| packed.iter().filter(move |l| l.starts_with(kind)).cloned();
    let expected: Vec<String> = of_type('n')
        .chain(of_type('n'))
        .chain(of_type('w'))
        .collect();
    assert_eq!(read("split_messages.osm.pbf"), expected);
}

/// Checks the block filters: rejecting every block for one object type removes exactly that type's objects, in every
/// read mode, whether the blocks are skipped whole or only their groups of that type; and keeping blocks with a key
/// keeps every object that has it.
fn check_filters(path: &Path, lines: &[String]) {
    type Narrow = fn(PbfReader) -> PbfReader;
    let cases: [(char, Narrow); 3] = [
        ('n', |f| f.nodes_where(|_| false)),
        ('w', |f| f.ways_where(|_| false)),
        ('r', |f| f.relations_where(|_| false)),
    ];
    for (kind, narrow) in cases {
        let file = narrow(PbfReader::open(path).unwrap());
        let want: Vec<&String> = lines.iter().filter(|l| !l.starts_with(kind)).collect();
        let modes = [
            ("one thread", common::osmflow(&file, false)),
            ("ordered", common::osmflow_ordered(&file)),
            ("unordered", common::osmflow_unordered(&file)),
        ];
        for (mode, got) in modes {
            let got = got.unwrap();
            assert_eq!(
                got.iter().collect::<Vec<_>>(),
                want,
                "{mode} read without {kind}"
            );
        }
    }
    // Every read mode reads the same objects; they differ only in how much of the file they read.
    let ways: Vec<&String> = lines.iter().filter(|l| l.starts_with('w')).collect();
    for mode in [ReadMode::Sequential, ReadMode::Selective] {
        let file = PbfReader::open(path)
            .unwrap()
            .read_mode(mode)
            .nodes_where(|_| false)
            .relations_where(|_| false);
        let got = common::osmflow(&file, false).unwrap();
        assert_eq!(
            got.iter().collect::<Vec<_>>(),
            ways,
            "{mode:?} read of ways"
        );
    }
    // A read finds a string table that goes on after a block's first group only in a block it keeps, and from then
    // on the reader's filtered reads check whole tables. This read keeps every block for its nodes, so the filtered
    // read after it, by a clone that shares what it found, keeps every object with the key.
    let reader = PbfReader::open(path).unwrap();
    common::osmflow(&reader.clone().ways_where(|s| s.has("highway")), false).unwrap();
    let split = path.ends_with("generated/split_string_table_zlib.osm.pbf");
    assert_eq!(
        reader.split_string_tables() > 0,
        split,
        "split string tables found"
    );
    let file = reader
        .nodes_where(|s| s.has("highway"))
        .ways_where(|s| s.has("highway"))
        .relations_where(|s| s.has("highway"));
    let got = common::osmflow(&file, false).unwrap();
    let mut got = got.iter();
    for line in lines
        .iter()
        .filter(|l| l.contains(" Thighway=") || l.contains(",highway="))
    {
        assert!(got.any(|g| g == line), "a filter of highway lost {line}");
    }
}

/// Before a read has found a string table that goes on after a block's first group, a filter judges a block by the
/// strings at its start, as `PbfReader::split_string_tables` says: a block a filter skips whole is not looked at.
#[test]
fn a_split_string_table_is_checked_once_one_is_found() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/generated/split_string_table_zlib.osm.pbf");
    let highway = |f: PbfReader| {
        f.nodes_where(|s| s.has("highway"))
            .ways_where(|s| s.has("highway"))
            .relations_where(|s| s.has("highway"))
    };
    let reader = PbfReader::open(&path).unwrap();
    let ways = |f: &PbfReader| {
        common::osmflow(f, false)
            .unwrap()
            .iter()
            .filter(|l| l.starts_with('w'))
            .count()
    };
    assert_eq!(
        ways(&highway(reader.clone())),
        0,
        "skipped by the start of its table"
    );
    assert_eq!(reader.split_string_tables(), 0);
    // nodes are kept, so the block is inflated whole, and its later strings found
    assert_eq!(ways(&reader.clone().ways_where(|s| s.has("highway"))), 1);
    assert_eq!(reader.split_string_tables(), 1);
    assert_eq!(
        ways(&highway(reader.clone())),
        1,
        "checked whole once one was found"
    );
    assert_eq!(reader.split_string_tables(), 2);
}

/// Why `reader` is not compared on the fixture `name`, if it is not.
fn misread(name: &str, reader: &str) -> Option<String> {
    MISREAD
        .iter()
        .find(|(n, r, _)| *n == name && *r == reader)
        .map(|(_, r, why)| format!("# {r} is not compared: {why}"))
}

/// The fixture's path below `fixtures/`, with `__` between its parts. `test_each_path!` hands out canonical paths,
/// so the directory is made canonical too: on Windows that adds the `\\?\` prefix, which `CARGO_MANIFEST_DIR` lacks.
fn snapshot_name(path: &Path) -> String {
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .canonicalize()
        .unwrap();
    let rel = path
        .strip_prefix(&fixtures)
        .expect("the fixture is under the canonical fixtures directory");
    rel.to_string_lossy().replace(['/', '\\'], "__")
}

fn fixture([path]: [&Path; 1]) {
    let name = snapshot_name(path);
    let read = PbfReader::open(path).and_then(|file| Ok((common::osmflow(&file, false)?, file)));
    let snapshot = match read {
        Err(e) => {
            let osmpbf = match panic::catch_unwind(|| common::osmpbf(path)) {
                Ok(Ok(lines)) => format!("osmpbf reads {} objects", lines.len()),
                Ok(Err(e)) => format!("osmpbf fails too: {e}"),
                Err(_) => "osmpbf panics".to_owned(),
            };
            format!("osmflow error: {e}\n{osmpbf}\n")
        }
        Ok((lines, file)) => {
            check_modes(&file, &lines);
            check_filters(path, &lines);
            let (osmpbf, osmpbfreader) = (misread(&name, "osmpbf"), misread(&name, "osmpbfreader"));
            let dropped = if osmpbf.is_none() {
                check_osmpbf(path, &lines)
            } else {
                0
            };
            // osmpbfreader drops deleted objects, so history files are left out.
            if !file.header().is_history() && osmpbfreader.is_none() {
                let ours = common::osmflow(&file, true).unwrap();
                let other = common::osmpbfreader(path).expect("osmpbfreader reads the file");
                assert_lines_eq(&ours, &other, "osmpbfreader");
            }
            let mut out = render(&file, &lines, dropped);
            for note in [osmpbf, osmpbfreader].into_iter().flatten() {
                out.push_str(&note);
                out.push('\n');
            }
            out
        }
    };
    insta::with_settings!({ snapshot_path => "snapshots", prepend_module_to_snapshot => false }, {
        insta::assert_snapshot!(name, snapshot);
    });
}

fn check_modes(file: &PbfReader, lines: &[String]) {
    let ordered = common::osmflow_ordered(file).expect("parallel ordered read");
    assert_eq!(lines, ordered.as_slice(), "parallel ordered read differs");
    // Node locations alone take another path through dense nodes, which skips their tags and metadata
    let locations =
        common::node_locations(file, Select::node_locations()).expect("node locations read");
    let all = common::node_locations(file, Select::all()).expect("full read");
    assert_eq!(locations, all, "node locations read differs");
    let unordered = common::osmflow_unordered(file).expect("parallel unordered read");
    assert_eq!(
        lines,
        unordered.as_slice(),
        "parallel unordered read differs"
    );
}

/// Compares with `osmpbf`, and returns the number of nodes whose metadata it dropped: it gives a dense node no
/// metadata at all when the block lacks any of the version, timestamp, changeset, user ID or user name columns, as in
/// Geofabrik's public extracts. Those lines are compared without their metadata.
fn check_osmpbf(path: &Path, lines: &[String]) -> usize {
    const NO_META: &str = "v-1 dV c0 t0 i0 u ";
    let other = common::osmpbf(path).expect("osmpbf reads the file");
    let mut dropped = 0;
    let ours: Vec<String> = lines
        .iter()
        .zip(&other)
        .map(|(ours, theirs)| {
            let (Some((id, rest)), Some((tid, trest))) =
                (ours.split_once(' '), theirs.split_once(' '))
            else {
                return ours.clone();
            };
            if id == tid
                && id.starts_with('n')
                && trest.starts_with(NO_META)
                && !rest.starts_with(NO_META)
            {
                let tags = rest.find(" T").map_or(rest, |at| &rest[at + 1..]);
                dropped += 1;
                format!("{id} {NO_META}{tags}")
            } else {
                ours.clone()
            }
        })
        .chain(lines.iter().skip(other.len()).cloned())
        .collect();
    assert_lines_eq(&ours, &other, "osmpbf");
    dropped
}

#[allow(clippy::panic, reason = "a test helper fails the test")]
fn assert_lines_eq(ours: &[String], other: &[String], name: &str) {
    if ours == other {
        return;
    }
    let first = ours.iter().zip(other).position(|(a, b)| a != b);
    match first {
        Some(i) => panic!(
            "{name} differs at object {i}:\n  osmflow: {}\n  {name}: {}",
            ours[i], other[i]
        ),
        None => panic!(
            "{name} reads {} objects, osmflow {}",
            other.len(),
            ours.len()
        ),
    }
}

fn render(file: &PbfReader, lines: &[String], dropped: usize) -> String {
    let mut out = common::osmflow_header(file).join("\n");
    let count = |c: u8| lines.iter().filter(|l| l.as_bytes()[0] == c).count();
    writeln!(
        out,
        "\n# {} nodes, {} ways, {} relations, hash {:016x}",
        count(b'n'),
        count(b'w'),
        count(b'r'),
        common::fnv(lines)
    )
    .unwrap();
    let shown = if lines.len() > FULL_SNAPSHOT_LINES {
        writeln!(out, "# the first 20 objects:").unwrap();
        &lines[..20]
    } else {
        lines
    };
    for line in shown {
        writeln!(out, "{line}").unwrap();
    }
    if dropped > 0 {
        writeln!(
            out,
            "# osmpbf drops the metadata of {dropped} dense nodes that lack some metadata columns"
        )
        .unwrap();
    }
    out
}
