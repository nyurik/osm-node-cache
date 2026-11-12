//! Runs the `osmflow` command on the repository's fixtures, from the workspace root, and snapshots what it writes:
//! inline when it is short text, in `snapshots/` when it is long lines of JSON. Under `cargo llvm-cov` the command is
//! built instrumented too, so these runs count toward its coverage. Nothing is downloaded: the replication streams are
//! directories of the fixtures. Linux only: the snapshots hold its paths.
#![cfg(target_os = "linux")]

use std::path::Path;
use std::process::Command;
use std::sync::LazyLock;

use insta::assert_snapshot;
use regex::Regex;

const PBF: &str = "crates/osmflow/fixtures/planetiler/monaco-latest.osm.pbf";
/// A day of Geofabrik's Monaco stream: diff 4960 and its state.
const STREAM: &str = "crates/osmflow/fixtures/osc/geofabrik/monaco";
const OSC: &str = "crates/osmflow/fixtures/osc/geofabrik/monaco/000/004/960.osc.gz";
const STATE: &str = "crates/osmflow/fixtures/osc/geofabrik/monaco/000/004/960.state.txt";
/// Two minutes of the planet's minutely stream.
const PLANET: &str = "crates/osmflow/fixtures/osc/planet/minute";

/// Runs `osmflow` with `args`, from the workspace root: its exit code, standard output and standard error, with the
/// age of a state and the time of a log line, which change from run to run, replaced.
fn osmflow(args: &[&str]) -> String {
    osmflow_with(args, &[])
}

/// [`osmflow`] with the environment variables `env`.
fn osmflow_with(args: &[&str], env: &[(&str, &str)]) -> String {
    static AGE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"\b\d+[ywdhms]( \d+[wdhms])? ago\b").unwrap());
    static TIME: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#""timestamp":"[^"]*""#).unwrap());
    let output = Command::new(env!("CARGO_BIN_EXE_osmflow"))
        .current_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."))
        .args(args)
        .env_remove("RUST_LOG")
        .env_remove("RUST_LOG_FORMAT")
        .envs(env.iter().copied())
        .output()
        .unwrap();
    let text = format!(
        "exit code {}\n--- stdout\n{}--- stderr\n{}",
        output.status.code().unwrap_or(-1),
        String::from_utf8(output.stdout).unwrap(),
        String::from_utf8(output.stderr).unwrap()
    );
    let text = AGE.replace_all(&text, "[AGE] ago");
    TIME.replace_all(&text, r#""timestamp":"[TIME]""#)
        .into_owned()
}

#[test]
fn dump_pbf() {
    assert_snapshot!(osmflow(&["dump", PBF, "--limit", "8"]), @r#"
    exit code 0
    --- stdout
    # file: crates/osmflow/fixtures/planetiler/monaco-latest.osm.pbf (445.3 kB)
    # writing program: osmium/1.8.0
    # required features: OsmSchema-V0.6, DenseNodes
    # bbox (west, south, east, north): 7.409205, 43.72335, 7.448637, 43.75169
    # replication: sequence 2947, 2021-04-21T20:21:46Z, http://download.geofabrik.de/europe/monaco-updates
    n21911883 lat=43.7371175 lon=7.4229093
    n21911886 lat=43.7372399 lon=7.4234985 {"highway":"crossing","crossing":"zebra","crossing_ref":"zebra"}
    n21911894 lat=43.7377782 lon=7.4259476
    n21911901 lat=43.7377621 lon=7.4267099
    n21911908 lat=43.7381906 lon=7.426961
    n21911954 lat=43.7383862 lon=7.4269064
    n21911969 lat=43.7384518 lon=7.426836
    n21912089 lat=43.7389494 lon=7.4259518
    --- stderr
    "#);
    assert_snapshot!(osmflow(&["dump", PBF, "--blocks", "--limit", "5"]), @r#"
    exit code 0
    --- stdout
    # file: crates/osmflow/fixtures/planetiler/monaco-latest.osm.pbf (445.3 kB)
    # writing program: osmium/1.8.0
    # required features: OsmSchema-V0.6, DenseNodes
    # bbox (west, south, east, north): 7.409205, 43.72335, 7.448637, 43.75169
    # replication: sequence 2947, 2021-04-21T20:21:46Z, http://download.geofabrik.de/europe/monaco-updates
    ## blob 1 at byte 170: 71,019 bytes zlib, 125,391 raw, 376 strings in 5,471 bytes; groups: 8,000 dense nodes
    n21911883 lat=43.7371175 lon=7.4229093
    n21911886 lat=43.7372399 lon=7.4234985 {"highway":"crossing","crossing":"zebra","crossing_ref":"zebra"}
    n21911894 lat=43.7377782 lon=7.4259476
    n21911901 lat=43.7377621 lon=7.4267099
    n21911908 lat=43.7381906 lon=7.426961
    --- stderr
    "#);
    assert_snapshot!(
        "dump_geojson_ways",
        osmflow(&[
            "dump",
            PBF,
            "--format",
            "geojson",
            "--geometry",
            "--type",
            "way",
            "--limit",
            "2"
        ])
    );
    assert_snapshot!(
        "dump_highways_with_metadata",
        osmflow(&[
            "dump",
            PBF,
            "--format",
            "geojsonseq",
            "--tag",
            "highway",
            "--metadata",
            "--limit",
            "3"
        ])
    );
    assert_snapshot!(
        "dump_building_areas",
        osmflow(&[
            "dump",
            PBF,
            "--format",
            "geojsonseq",
            "--area",
            "building",
            "--type",
            "way",
            "--geometry",
            "--limit",
            "2"
        ])
    );
    assert_snapshot!(
        "dump_relation",
        osmflow(&[
            "dump",
            PBF,
            "--format",
            "geojson",
            "--geometry",
            "--id",
            "r1369191,r1369193"
        ])
    );
}

#[test]
fn stats_pbf() {
    assert_snapshot!(osmflow(&["stats", PBF]), @"
    exit code 0
    --- stdout
    file          crates/osmflow/fixtures/planetiler/monaco-latest.osm.pbf (445.3 kB)
    written by    osmium/1.8.0
    features      OsmSchema-V0.6, DenseNodes
    header bbox   lon 7.409205 .. 7.448637, lat 43.72335 .. 43.75169
    node extent   lon 7.4016897 .. 7.5002447, lat 43.5165358 .. 43.7543341
    replication   sequence 2947 at 2021-04-21T20:21:46Z
    updates       http://download.geofabrik.de/europe/monaco-updates
    timestamps    not read; --metadata reads them
    order         by type, then ID (the header does not say sorted)
    blocks        6 (6 zlib): 445.0 kB in blobs, 882.8 kB uncompressed; 6,980 strings in 106.8 kB
    groups        4 dense nodes, 1 ways, 1 relations
    way refs      30,812 node IDs in all ways
    members       27,740 members of all relations
                            count          tagged            tags   IDs
    nodes                  25,423           2,427           7,822   21911883 .. 8639732906
    ways                    4,106           4,045          13,791   4097656 .. 931954691
    relations                 243             243           3,129   7385 .. 12431099
    total                  29,772           6,715          24,742
    --- stderr
    ");
    assert_snapshot!(
        "stats_pbf_json",
        osmflow(&[
            "stats",
            PBF,
            "--format",
            "json",
            "--metadata",
            "--threads",
            "2"
        ])
    );
}

#[test]
fn dump_changes() {
    assert_snapshot!("dump_osc", osmflow(&["dump", OSC]));
    assert_snapshot!(
        "dump_osc_modified",
        osmflow(&["dump", OSC, "--format", "geojsonseq", "--action", "modify"])
    );
    assert_snapshot!(
        "dump_changeset",
        osmflow(&[
            "dump",
            "crates/osmflow/fixtures/osc/paulmach-osm/changeset_38162206.osc",
            "--changeset",
            "38162206",
            "--metadata",
            "--limit",
            "4"
        ])
    );
    assert_snapshot!(
        "dump_stream",
        osmflow(&["dump", STREAM, "--seq", "4960", "--limit", "3"])
    );
}

#[test]
fn stats_changes() {
    assert_snapshot!(osmflow(&["stats", OSC]), @"
    exit code 0
    --- stdout
    file          crates/osmflow/fixtures/osc/geofabrik/monaco/000/004/960.osc.gz (3.0 kB)
    sequence      4960 at 2026-11-06T21:21:21Z
    node extent   lon 7.4184846 .. 7.4276832, lat 43.7284687 .. 43.7323492
    timestamps    2026-11-06T10:06:56Z .. 2026-11-06T15:52:40Z; highest version 75
    changesets    not in the input
    users         not in the input
                  --------------- created or modified ---------------
                        create       modify       tagged         tags       delete      objects
    nodes                    1            1            2           71            0            2
    ways                     0            0            0            0            0            0
    relations                0            1            1          243            0            1
    total                    1            2            3          314            0            3
    --- stderr
    ");
    assert_snapshot!(
        "stats_osc_json",
        osmflow(&["stats", OSC, "--format", "json"])
    );
    assert_snapshot!(osmflow(&["stats", STATE]), @"
    exit code 0
    --- stdout
    state         sequence 4960 at 2026-11-06T21:21:21Z, [AGE] ago
    --- stderr
    ");
    assert_snapshot!(osmflow(&["-vv", "stats", STREAM, "--seq", "4960"]), @"
    exit code 0
    --- stdout
    stream        crates/osmflow/fixtures/osc/geofabrik/monaco
    sequence      4960 at 2026-11-06T21:21:21Z
    node extent   lon 7.4184846 .. 7.4276832, lat 43.7284687 .. 43.7323492
    timestamps    2026-11-06T10:06:56Z .. 2026-11-06T15:52:40Z; highest version 75
    changesets    not in the input
    users         not in the input
                  --------------- created or modified ---------------
                        create       modify       tagged         tags       delete      objects
    nodes                    1            1            2           71            0            2
    ways                     0            0            0            0            0            0
    relations                0            1            1          243            0            1
    total                    1            2            3          314            0            3
    --- stderr
    DEBUG reading diffs first=4960 count=1
    DEBUG diff: read path=crates/osmflow/fixtures/osc/geofabrik/monaco/000/004/960.osc.gz sequence=4960
    ");
    assert_snapshot!(
        "stats_planet_logged_as_json",
        osmflow_with(
            &["-vv", "stats", PLANET, "--seq", "7363600"],
            &[("RUST_LOG_FORMAT", "json")]
        )
    );
}

#[test]
fn seq() {
    assert_snapshot!(osmflow(&["seq", PLANET, "crates/osmflow/fixtures/osc/planet/minute/007/363/630.state.txt"]), @"
    exit code 0
    --- stdout
    The latest diff of crates/osmflow/fixtures/osc/planet/minute is 7363630, at 2026-11-08T08:30:41Z, [AGE] ago.
    630.state.txt is the state of diff 7363630, at 2026-11-08T08:30:41Z.
    There is no diff after it yet.
    --- stderr
    ");
    assert_snapshot!(osmflow(&["seq", STREAM, "2026-11-08"]), @"
    exit code 0
    --- stdout
    The latest diff of crates/osmflow/fixtures/osc/geofabrik/monaco is 4960, at 2026-11-06T21:21:21Z, [AGE] ago.
    The last diff at or before 2026-11-08T00:00:00Z is 4960, at 2026-11-06T21:21:21Z.
    There is no diff after it yet.
    --- stderr
    ");
    // The PBF file is older than the stream's first diff
    assert_snapshot!(osmflow(&["seq", STREAM, PBF]), @"
    exit code 1
    --- stdout
    --- stderr
    osmflow: 2021-04-21T20:21:46Z is before the oldest diff the stream has, 4960 at 2026-11-06T21:21:21Z
    ");
}
