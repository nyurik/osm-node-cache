//! Ways as geometries: node locations loaded into a store, and looked up for each way.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};

use geo_types::Geometry;
use osmflow::nodes::{MemoryStore, NodeRead as _};
use osmflow::{AreaRules, GeometryError, Location, Locator, PbfReader, Select};

fn fixture(name: &str) -> PbfReader {
    PbfReader::open(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("fixtures")
            .join(name),
    )
    .unwrap()
}

/// The file's node locations in 1e-7 degrees, by ID.
fn locations(file: &PbfReader) -> HashMap<i64, (i32, i32)> {
    let mut out = HashMap::new();
    for block in file.blocks(Select::node_locations()).unwrap() {
        for n in block.unwrap().nodes() {
            out.insert(n.id(), (n.decimicro_lat(), n.decimicro_lon()));
        }
    }
    out
}

fn loaded(file: &PbfReader) -> MemoryStore {
    let mut store = MemoryStore::new();
    let count = file.load_nodes(&mut store).unwrap();
    assert_eq!(count, store.len() as u64);
    store
}

#[test]
fn ways_get_their_nodes_locations() {
    for name in [
        "planetiler/monaco-latest.osm.pbf",
        "go-osm/monaco-20150428.osm.pbf",
        "r5/missing-nodes.pbf",
    ] {
        let file = fixture(name);
        let expected = locations(&file);
        let store = loaded(&file);
        assert_eq!(
            store.len(),
            expected.keys().filter(|&&id| id >= 0).count(),
            "{name}"
        );
        let mut locator = Locator::new(store.reader());
        let (mut lines, mut areas) = (0, 0);
        for block in file.blocks(Select::all()).unwrap() {
            for way in block.unwrap().ways() {
                let refs: Vec<i64> = way.refs().collect();
                match locator.line_string(&way) {
                    Ok(line) => {
                        lines += 1;
                        assert_eq!(line.0.len(), refs.len());
                        for (c, id) in line.coords().zip(&refs) {
                            let (lat, lon) = expected[id];
                            let loc = Location::new(lat, lon);
                            assert_eq!(
                                (c.y, c.x),
                                (loc.lat_degrees(), loc.lon_degrees()),
                                "{name} {id}"
                            );
                        }
                        let closed = refs.len() >= 4 && refs.first() == refs.last();
                        match locator.polygon(&way) {
                            Ok(_) => areas += 1,
                            Err(e) => assert!(
                                !closed && e == GeometryError::NotClosed,
                                "{name} way {}",
                                way.id()
                            ),
                        }
                    }
                    Err(GeometryError::MissingNode(id)) => {
                        assert!(!expected.contains_key(&id), "{name} {id}");
                    }
                    Err(e) => assert!(
                        refs.len() < 2 && e == GeometryError::TooFewNodes,
                        "{name} {e}"
                    ),
                }
            }
        }
        assert!(lines > 0, "{name}");
        if name.starts_with("planetiler") {
            assert!(areas > 0);
        }
    }
}

#[test]
fn a_way_leaving_the_file_names_its_missing_node() {
    let file = fixture("r5/missing-nodes.pbf");
    let store = loaded(&file);
    let mut locator = Locator::new(store.reader());
    let missing = file
        .blocks(Select::all())
        .unwrap()
        .flat_map(|b| {
            let block = b.unwrap();
            block
                .ways()
                .map(|w| locator.line_string(&w).err())
                .collect::<Vec<_>>()
        })
        .flatten()
        .collect::<Vec<_>>();
    assert!(
        matches!(missing.as_slice(), [GeometryError::MissingNode(_), ..]),
        "{missing:?}"
    );
}

#[test]
fn locations_on_ways_need_no_store() {
    let file = fixture("osmpbf/loc_on_ways.osm.pbf");
    let empty = MemoryStore::new();
    let mut locator = Locator::new(empty.reader());
    let mut lines = 0;
    for block in file.blocks(Select::all()).unwrap() {
        for way in block.unwrap().ways() {
            let line = locator.line_string(&way).unwrap();
            let expected: Vec<_> = way.locations().unwrap().collect();
            assert_eq!(line.0.len(), expected.len());
            lines += 1;
        }
    }
    assert!(lines > 0);
}

#[test]
fn par_resolve_gives_each_thread_a_lookup() {
    let file = fixture("planetiler/monaco-latest.osm.pbf");
    let store = loaded(&file);
    let mut locator = Locator::new(store.reader());
    let mut expected = 0;
    for block in file.blocks(Select::all()).unwrap() {
        expected += block
            .unwrap()
            .ways()
            .filter(|w| locator.line_string(w).is_ok())
            .count();
    }
    let lines = AtomicUsize::new(0);
    file.par_resolve(Select::all(), &store, |block, locator| {
        let n = block
            .ways()
            .filter(|w| locator.line_string(w).is_ok())
            .count();
        lines.fetch_add(n, Ordering::Relaxed);
        Ok(())
    })
    .unwrap();
    assert_eq!(lines.into_inner(), expected);
}

#[test]
fn closed_ways_are_areas_by_their_tags() {
    let file = fixture("planetiler/monaco-latest.osm.pbf");
    let store = loaded(&file);
    let mut locator = Locator::new(store.reader());
    let (rules, none) = (AreaRules::default(), AreaRules::none());
    let (mut areas, mut closed_lines, mut open) = (0, 0, 0);
    for block in file.blocks(Select::all()).unwrap() {
        let block = block.unwrap();
        for way in block.ways() {
            let refs: Vec<i64> = way.refs().collect();
            let closed = refs.len() >= 4 && refs.first() == refs.last();
            match locator.area_or_line(&way, &rules) {
                Ok(Geometry::Polygon(_)) => {
                    assert!(closed && rules.is_area(way.tags()), "way {}", way.id());
                    areas += 1;
                }
                Ok(Geometry::LineString(_)) if closed => {
                    assert!(!rules.is_area(way.tags()), "way {}", way.id());
                    closed_lines += 1;
                }
                Ok(Geometry::LineString(_)) => open += 1,
                Ok(other) => panic!("way {}: {other:?}", way.id()),
                Err(e) => assert!(matches!(e, GeometryError::MissingNode(_)), "{e}"),
            }
            // With no rules, only `area=yes` makes an area.
            if let Ok(Geometry::Polygon(_)) = locator.area_or_line(&way, &none) {
                assert!(way.tags().any(|t| t == ("area", "yes")), "way {}", way.id());
            }
        }
    }
    assert!(
        areas > 1000 && closed_lines > 0 && open > 1000,
        "{areas} {closed_lines} {open}"
    );
}
