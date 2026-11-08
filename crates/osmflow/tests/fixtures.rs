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
use std::path::Path;

use osmflow::PbfReader;
use test_each_file::test_each_path;

test_each_path! { for ["pbf"] in "crates/osmflow/fixtures" => fixture }

/// Dumps longer than this are snapshotted by their counts, hash and first lines.
const FULL_SNAPSHOT_LINES: usize = 200;

fn snapshot_name(path: &Path) -> String {
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures");
    let rel = path.strip_prefix(&fixtures).unwrap_or(path);
    rel.to_string_lossy().replace(['/', '\\'], "__")
}

fn fixture([path]: [&Path; 1]) {
    let name = snapshot_name(path);
    let read = PbfReader::open(path).and_then(|file| Ok((common::osmflow(&file, false)?, file)));
    let snapshot = match read {
        Err(e) => {
            let osmpbf = match std::panic::catch_unwind(|| common::osmpbf(path)) {
                Ok(Ok(lines)) => format!("osmpbf reads {} objects", lines.len()),
                Ok(Err(e)) => format!("osmpbf fails too: {e}"),
                Err(_) => "osmpbf panics".to_owned(),
            };
            format!("osmflow error: {e}\n{osmpbf}\n")
        }
        Ok((lines, file)) => {
            check_modes(&file, &lines);
            let dropped = check_osmpbf(path, &lines);
            // osmpbfreader drops deleted objects, so history files are left out.
            if !file.header().is_history() {
                let ours = common::osmflow(&file, true).unwrap();
                let other = common::osmpbfreader(path).expect("osmpbfreader reads the file");
                assert_lines_eq(&ours, &other, "osmpbfreader");
            }
            render(&file, &lines, dropped)
        }
    };
    insta::with_settings!({ snapshot_path => "snapshots", prepend_module_to_snapshot => false }, {
        insta::assert_snapshot!(name, snapshot);
    });
}

fn check_modes(file: &PbfReader, lines: &[String]) {
    let ordered = common::osmflow_ordered(file).expect("parallel ordered read");
    assert_eq!(lines, ordered.as_slice(), "parallel ordered read differs");
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
