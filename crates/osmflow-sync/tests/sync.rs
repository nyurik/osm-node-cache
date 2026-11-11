//! Imports, updates and rebuilds against the database `OSMFLOW_SYNC_TEST_DB` names, such as
//! `postgresql://postgres@localhost:5434/postgres` (`just test-sync-db` starts one). Its `osm` schema is replaced.
//! Without it, the test does nothing.

use std::fmt::Write as _;
use std::path::Path;
use std::{env, fs};

use osmflow::Location;
use osmflow::nodes::PagedStore;
use osmflow::nodes::prelude::*;
use osmflow::osc::Replication;
use osmflow_sync::{FileStamp, ImportOpts, Mirror, NodeWay, Rebuild, UpdateOpts};
use postgres::{Client, NoTls};

const PBF: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../osmflow/fixtures/osmpbfreader/clipperton.osm.pbf"
);
/// The fixture's replication sequence and time.
const STATE: &str = "sequenceNumber=1708\ntimestamp=2025-04-23T20\\:21\\:01Z\n";
/// A node of ways 178300745 and 210476684.
const NODE: i64 = 1_886_651_627;

/// Diff 1709: [`NODE`] moves; way 265426044 and relation 11980 are deleted; a way and a tagged node are created.
const DIFF_1709: &str = r#"<modify>
  <node id="1886651627" version="9" timestamp="2026-11-01T00:00:00Z" lat="10.3" lon="-109.2"/>
</modify>
<create>
  <node id="13000000000" version="1" timestamp="2026-11-01T00:00:00Z" lat="10.31" lon="-109.21">
    <tag k="amenity" v="bench"/>
  </node>
  <way id="2000000000" version="1" timestamp="2026-11-01T00:00:00Z">
    <nd ref="1724610187"/><nd ref="13000000000"/>
    <tag k="highway" v="path"/>
  </way>
</create>
<delete>
  <way id="265426044" version="5"/>
  <relation id="11980" version="30"/>
</delete>"#;
/// Diff 1711: [`NODE`] moves again, after diff 1710 took it out of way 210476684.
const DIFF_1711: &str = r#"<modify>
  <node id="1886651627" version="10" timestamp="2026-11-01T00:02:00Z" lat="10.32" lon="-109.22"/>
</modify>"#;
/// Diff 1712: [`NODE`] moves again, and so does the bench, of way 2000000000 only.
const DIFF_1712: &str = r#"<modify>
  <node id="1886651627" version="11" timestamp="2026-11-01T00:03:00Z" lat="10.33" lon="-109.23"/>
  <node id="13000000000" version="2" timestamp="2026-11-01T00:03:00Z" lat="10.4" lon="-109.3">
    <tag k="amenity" v="bench"/>
  </node>
</modify>"#;

#[test]
fn import_update_rebuild() {
    let Ok(url) = env::var("OSMFLOW_SYNC_TEST_DB") else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let diffs = dir.path().join("diffs");
    write_state(&diffs, 708, STATE);
    let replication = Replication::local(&diffs);
    let files = dir.path().join("files");
    let mirror = Mirror::new(&url, &files);
    import(&mirror);
    let mut db = Client::connect(&url, NoTls).unwrap();
    write_diffs(&diffs, &mut db);

    // A stream whose diff 1708 is at another time is not the database's
    let other = dir.path().join("other");
    write_state(&other, 708, &STATE.replace("20\\:21", "20\\:22"));
    let err = mirror
        .update(&Replication::local(&other), &until(1709, 1))
        .unwrap_err();
    assert!(err.to_string().contains("not the database's"), "{err}");

    // One batch per diff, with the files of revision 1 kept
    let saved = dir.path().join("saved");
    copy_dir(&files, &saved);
    let stats = mirror.update(&replication, &until(1711, 1)).unwrap();
    assert_eq!((stats.batches, stats.diffs, stats.sequence), (3, 3, 1711));
    check_rows(&mut db);
    // Moved in diffs 1709 and 1711, so changed twice; written anew by diff 1710, which took the node out
    assert_eq!(minor_rev(&mut db, 178_300_745), (2, 4));
    assert_eq!(minor_rev(&mut db, 210_476_684), (0, 3));
    let one_by_one = rows(&mut db);

    // The files of revision 1 back: the next update brings them up to revision 4, then applies diff 1712. The index
    // still lists way 210476684 for the node, which is checked and dropped, and gained way 2000000000, of the bench.
    copy_dir(&saved, &files);
    let restored = mirror.status().unwrap();
    assert_eq!(
        (restored.nodes.unwrap().rev, restored.index.unwrap().rev),
        (1, 1)
    );
    // One open mirror for the lookups and the update. Opening it brings the index up to revision 4 from the
    // database's rows, which only adds: it still lists way 210476684 for the node.
    let mut open = mirror.open().unwrap();
    assert_eq!(
        open.ways_through(&[NODE]).unwrap(),
        [way(NODE, 178_300_745), way(NODE, 210_476_684)]
    );
    let stats = open.update(&replication, &until(1712, 1)).unwrap();
    assert_eq!(stats.batches, 1);
    // The ways through two nodes, asked in any order, with a repeat and a negative ID
    let through = open.ways_through(&[13_000_000_000, NODE, NODE, -1]);
    assert_eq!(
        through.unwrap(),
        [way(NODE, 178_300_745), way(13_000_000_000, 2_000_000_000)]
    );
    drop(open);
    assert_eq!(
        changes(&mut db, 5),
        [
            ("n", 13_000_000_000, "u"),
            ("w", 178_300_745, "g"),
            ("w", 2_000_000_000, "g")
        ]
        .map(owned)
    );
    check_files(&mirror, 5);

    update_failures(&mirror, &replication, &mut db, &saved, &one_by_one);

    // The same diffs in one batch: the same rows, each way changed once
    import(&mirror);
    let stats = mirror.update(&replication, &until(1711, 60)).unwrap();
    assert_eq!((stats.batches, stats.diffs), (1, 3));
    assert_eq!(rows(&mut db), one_by_one);
    assert_eq!(minor_rev(&mut db, 178_300_745), (1, 2));
    mirror.update(&replication, &until(1712, 60)).unwrap();
    check_files(&mirror, 3);
    let stats = mirror.update(&replication, &until(1712, 60)).unwrap();
    assert_eq!(stats.batches, 0);

    // The files made again both ways
    for from in [
        Rebuild::Database,
        Rebuild::Pbf {
            path: Path::new(PBF),
            sequence: None,
            replication: &replication,
            max_diffs: 2,
        },
    ] {
        fs::remove_file(mirror.index_path()).unwrap();
        mirror.rebuild(&from).unwrap();
        check_files(&mirror, 3);
    }
}

/// Updates that fail on an open mirror, from a fresh import: a batch the database refuses, a database behind the
/// files, and a node file of another database (from the files in `saved`). `one_by_one` holds the rows after diffs
/// 1709 to 1711 applied one per batch.
fn update_failures(
    mirror: &Mirror,
    replication: &Replication,
    db: &mut Client,
    saved: &Path,
    one_by_one: &[String],
) {
    // The third batch of an update fails after its moved node reached the node file, the first two committed: the
    // retry on the same mirror applies the third, moving the way through the node, and gives the same rows
    import(mirror);
    let mut open = mirror.open().unwrap();
    db.batch_execute(
        "CREATE FUNCTION osm.fail() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE 'injected'; END $$; \
         CREATE TRIGGER fail BEFORE INSERT ON osm.batches FOR EACH ROW WHEN (NEW.rev = 4) \
         EXECUTE FUNCTION osm.fail();",
    )
    .unwrap();
    let err = open.update(replication, &until(1711, 1)).unwrap_err();
    assert!(format!("{err:?}").contains("injected"), "{err:?}");
    assert_eq!(minor_rev(db, 178_300_745), (1, 2));
    db.batch_execute("DROP TRIGGER fail ON osm.batches; DROP FUNCTION osm.fail();")
        .unwrap();
    let stats = open.update(replication, &until(1711, 1)).unwrap();
    assert_eq!(stats.batches, 1);
    assert_eq!(rows(db), one_by_one);
    assert_eq!(minor_rev(db, 178_300_745), (2, 4));
    assert_eq!(minor_rev(db, 210_476_684), (0, 3));
    // The files are checked on every update: a database behind them, as after a restore, is an error
    db.batch_execute("UPDATE osm.state SET rev = rev - 1")
        .unwrap();
    let err = open.update(replication, &until(1712, 1)).unwrap_err();
    assert!(err.to_string().contains("ahead of the database's"), "{err}");
    drop(open);
    db.batch_execute("UPDATE osm.state SET rev = rev + 1")
        .unwrap();
    // A node file of another database, under an index of this one, is an error of the first update
    for entry in fs::read_dir(saved).unwrap() {
        let name = entry.unwrap().file_name();
        if name.to_string_lossy().starts_with("nodes.cache") {
            fs::copy(saved.join(&name), mirror.node_path().with_file_name(&name)).unwrap();
        }
    }
    let err = mirror
        .open()
        .unwrap()
        .update(replication, &until(1712, 1))
        .unwrap_err();
    assert!(
        err.to_string()
            .contains("nodes.cache was not made for this database"),
        "{err}"
    );
}

fn import(mirror: &Mirror) {
    let opts = ImportOpts {
        replace: true,
        ..ImportOpts::default()
    };
    let stats = mirror.import(PBF, None, &opts).unwrap();
    assert_eq!(
        (stats.nodes, stats.tagged_nodes, stats.ways, stats.relations),
        (3097, 238, 151, 17)
    );
}

fn way(node: i64, way: i64) -> NodeWay {
    NodeWay { node, way }
}

fn until(sequence: u64, max_diffs: u64) -> UpdateOpts {
    UpdateOpts {
        max_diffs,
        until: Some(sequence),
    }
}

/// Writes the state file of diff `1000 + n` into the stream directory `dir`.
fn write_state(dir: &Path, n: u32, state: &str) {
    fs::create_dir_all(dir.join("000/001")).unwrap();
    fs::write(dir.join(format!("000/001/{n}.state.txt")), state).unwrap();
}

/// Writes diffs 1709 to 1712. Diff 1710 takes [`NODE`] out of way 210476684, whose other nodes come from the database.
fn write_diffs(dir: &Path, db: &mut Client) {
    let nodes: Vec<i64> = db
        .query_one("SELECT nodes FROM osm.ways WHERE id = 210476684", &[])
        .unwrap()
        .get(0);
    let refs = nodes
        .iter()
        .filter(|&&n| n != NODE)
        .fold(String::new(), |mut refs, n| {
            write!(refs, r#"<nd ref="{n}"/>"#).unwrap();
            refs
        });
    let diff_1710 = format!(
        r#"<modify><way id="210476684" version="99" timestamp="2026-11-01T00:01:00Z">{refs}</way></modify>"#
    );
    for (n, diff) in [
        (709, DIFF_1709),
        (710, &diff_1710),
        (711, DIFF_1711),
        (712, DIFF_1712),
    ] {
        let xml = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<osmChange version=\"0.6\">\n{diff}\n</osmChange>\n"
        );
        fs::write(dir.join(format!("000/001/{n}.osc.gz")), xml).unwrap();
        let state = format!(
            "sequenceNumber={}\ntimestamp=2026-11-01T00\\:0{}\\:30Z\n",
            1000 + n,
            n - 709
        );
        write_state(dir, n, &state);
    }
}

/// Makes directory `to` a copy of the files of `from`.
fn copy_dir(from: &Path, to: &Path) {
    if to.exists() {
        fs::remove_dir_all(to).unwrap();
    }
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let path = entry.unwrap().path();
        if path.is_file() {
            fs::copy(&path, to.join(path.file_name().unwrap())).unwrap();
        }
    }
}

fn minor_rev(db: &mut Client, id: i64) -> (i32, i64) {
    let row = db
        .query_one("SELECT minor, rev FROM osm.ways WHERE id = $1", &[&id])
        .unwrap();
    (row.get(0), row.get(1))
}

/// Every row of the three tables, without `minor` and `rev`, which depend on the batches.
fn rows(db: &mut Client) -> Vec<String> {
    [
        "SELECT format('n%s %s %s %s', id, version, tags, ST_AsText(geom)) FROM osm.nodes ORDER BY id",
        "SELECT format('w%s %s %s %s %s', id, version, tags, nodes, ST_AsText(geom)) FROM osm.ways ORDER BY id",
        "SELECT format('r%s %s %s %s %s', id, version, tags, member_ids, member_roles) FROM osm.relations ORDER BY id",
    ]
    .iter()
    .flat_map(|sql| db.query(*sql, &[]).unwrap())
    .map(|r| r.get(0))
    .collect()
}

/// The change log of revision `rev`.
fn changes(db: &mut Client, rev: i64) -> Vec<(String, i64, String)> {
    db.query(
        "SELECT kind::text, id, action::text FROM osm.changes WHERE rev = $1 ORDER BY 1, 2",
        &[&rev],
    )
    .unwrap()
    .iter()
    .map(|r| (r.get(0), r.get(1), r.get(2)))
    .collect()
}

fn owned((kind, id, action): (&str, i64, &str)) -> (String, i64, String) {
    (kind.to_owned(), id, action.to_owned())
}

/// The rows and the change log after diffs 1709 to 1711, one per batch.
fn check_rows(db: &mut Client) {
    let geom = |db: &mut Client, id: i64| -> Option<String> {
        db.query_opt("SELECT ST_AsText(geom) FROM osm.ways WHERE id = $1", &[&id])
            .unwrap()
            .map(|r| r.get(0))
    };
    assert!(geom(db, 178_300_745).unwrap().contains("-109.22 10.32"));
    assert!(!geom(db, 210_476_684).unwrap().contains("-109.2 "));
    assert_eq!(geom(db, 265_426_044), None);
    assert!(geom(db, 2_000_000_000).unwrap().ends_with("-109.21 10.31)"));
    let bench: i64 = db
        .query_one(
            "SELECT count(*) FROM osm.nodes WHERE tags->>'amenity' = 'bench'",
            &[],
        )
        .unwrap()
        .get(0);
    assert_eq!(bench, 1);
    let expected = [
        ("n", 13_000_000_000, "u"),
        ("r", 11980, "d"),
        ("w", 178_300_745, "g"),
        ("w", 210_476_684, "g"),
        ("w", 265_426_044, "d"),
        ("w", 2_000_000_000, "u"),
    ];
    assert_eq!(changes(db, 2), expected.map(owned));
    assert_eq!(changes(db, 3), [("w", 210_476_684, "u")].map(owned));
    assert_eq!(changes(db, 4), [("w", 178_300_745, "g")].map(owned));
}

/// The files are at revision `rev`, with the nodes where the last diff put them.
fn check_files(mirror: &Mirror, rev: i64) {
    let status = mirror.status().unwrap();
    let stamp = FileStamp {
        id: Some(status.db.id),
        rev,
    };
    assert_eq!(status.nodes.unwrap(), stamp);
    assert_eq!(status.index.unwrap(), stamp);
    let store = PagedStore::open_read_only(mirror.node_path()).unwrap();
    for (id, lat, lon) in [(NODE, 10.33, -109.23), (13_000_000_000, 10.4, -109.3)] {
        let id = id.cast_unsigned();
        assert_eq!(
            store.get(id),
            Some(Location::from_degrees(lat, lon)),
            "{id}"
        );
    }
}
