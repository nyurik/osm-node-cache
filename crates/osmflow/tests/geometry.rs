//! Ways as geometries: node locations loaded into a store, and looked up for each way.

use std::collections::HashMap;
use std::iter;
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

/// A store holding nodes 1.. at these (x, y) locations, in whole degrees.
fn grid(points: &[(i32, i32)]) -> MemoryStore {
    use osmflow::nodes::NodeStore as _;
    let mut store = MemoryStore::new();
    for (id, &(x, y)) in (1..).zip(points) {
        store
            .set(id, Location::new(y * 10_000_000, x * 10_000_000))
            .unwrap();
    }
    store
}

/// The polygons as rings of (x, y) in whole degrees, outer ring first.
#[expect(clippy::cast_possible_truncation)]
fn rings(area: &geo_types::MultiPolygon<f64>) -> Vec<Vec<Vec<(i32, i32)>>> {
    let ring =
        |r: &geo_types::LineString<f64>| r.coords().map(|c| (c.x as i32, c.y as i32)).collect();
    area.0
        .iter()
        .map(|p| {
            iter::once(ring(p.exterior()))
                .chain(p.interiors().iter().map(ring))
                .collect()
        })
        .collect()
}

// 1-4 a square of 10, 5-8 a square of 2 inside it, 9-12 a square of 1 inside that
const NESTED: [(i32, i32); 12] = [
    (0, 0),
    (10, 0),
    (10, 10),
    (0, 10),
    (4, 4),
    (4, 6),
    (6, 6),
    (6, 4),
    (5, 5),
    (5, 5),
    (5, 5),
    (5, 5),
];

#[test]
fn split_and_reversed_ways_join_into_an_outer_ring_with_a_hole() {
    let store = grid(&NESTED);
    let mut locator = Locator::new(store.reader());
    let ways: [&[i64]; 3] = [&[3, 2, 1], &[5, 6, 7, 8, 5], &[3, 4, 1]];
    let area = locator.multi_polygon(ways).unwrap();
    assert_eq!(
        rings(&area),
        [[
            vec![(10, 10), (10, 0), (0, 0), (0, 10), (10, 10)]
                .into_iter()
                .rev()
                .collect::<Vec<_>>(),
            vec![(4, 4), (4, 6), (6, 6), (6, 4), (4, 4)],
        ]]
    );
}

#[test]
fn an_island_in_a_hole_is_a_polygon_of_its_own() {
    let points = [
        (0, 0),
        (10, 0),
        (10, 10),
        (0, 10),
        (2, 2),
        (2, 8),
        (8, 8),
        (8, 2),
        (4, 4),
        (6, 4),
        (6, 6),
        (4, 6),
    ];
    let store = grid(&points);
    let mut locator = Locator::new(store.reader());
    let ways: [&[i64]; 3] = [&[9, 10, 11, 12, 9], &[1, 2, 3, 4, 1], &[5, 6, 7, 8, 5]];
    let area = locator.multi_polygon(ways).unwrap();
    assert_eq!(
        rings(&area),
        [
            vec![
                vec![(0, 0), (10, 0), (10, 10), (0, 10), (0, 0)],
                vec![(2, 2), (2, 8), (8, 8), (8, 2), (2, 2)]
            ],
            vec![vec![(4, 4), (6, 4), (6, 6), (4, 6), (4, 4)]],
        ]
    );
}

#[test]
fn a_chain_that_does_not_close_is_left_out() {
    let store = grid(&NESTED);
    let mut locator = Locator::new(store.reader());
    let ways: [&[i64]; 2] = [&[1, 2, 3, 4, 1], &[5, 6, 7]];
    let area = locator.multi_polygon(ways).unwrap();
    assert_eq!(
        rings(&area),
        [[vec![(0, 0), (10, 0), (10, 10), (0, 10), (0, 0)]]]
    );
}

#[test]
fn a_dangling_way_at_a_ring_node_does_not_break_the_ring() {
    let store = grid(&NESTED);
    let mut locator = Locator::new(store.reader());
    let ways: [&[i64]; 4] = [&[2, 3], &[1, 5], &[3, 4, 1], &[1, 2]];
    let area = locator.multi_polygon(ways).unwrap();
    assert_eq!(
        rings(&area),
        [[vec![(10, 0), (10, 10), (0, 10), (0, 0), (10, 0)]]]
    );
}

#[test]
fn ways_that_close_no_ring_are_an_error() {
    let store = grid(&NESTED);
    let mut locator = Locator::new(store.reader());
    let ways: [&[i64]; 2] = [&[1, 2, 3], &[3, 4]];
    assert_eq!(locator.multi_polygon(ways), Err(GeometryError::NoRing));
}

#[test]
fn a_ring_node_without_a_location_is_an_error() {
    let store = grid(&NESTED);
    let mut locator = Locator::new(store.reader());
    let ways: [&[i64]; 1] = [&[1, 2, 99, 4, 1]];
    assert_eq!(
        locator.multi_polygon(ways),
        Err(GeometryError::MissingNode(99))
    );
}

#[test]
fn monaco_multipolygons_assemble() {
    let file = fixture("planetiler/monaco-latest.osm.pbf");
    let store = loaded(&file);
    let mut ways: HashMap<i64, Vec<i64>> = HashMap::new();
    let mut relations = Vec::new();
    for block in file.blocks(Select::all()).unwrap() {
        let block = block.unwrap();
        for way in block.ways() {
            ways.insert(way.id(), way.refs().collect());
        }
        for relation in block.relations() {
            if relation.tags().any(|tag| tag == ("type", "multipolygon")) {
                let members: Vec<i64> = relation
                    .members()
                    .filter(|m| m.kind == osmflow::MemberType::Way)
                    .map(|m| m.id)
                    .collect();
                relations.push(members);
            }
        }
    }
    let mut locator = Locator::new(store.reader());
    let (mut built, mut polygons) = (0, 0);
    for members in &relations {
        let refs = members
            .iter()
            .filter_map(|id| ways.get(id).map(Vec::as_slice));
        if let Ok(area) = locator.multi_polygon(refs) {
            built += 1;
            polygons += area.0.len();
        }
    }
    assert_eq!((relations.len(), built, polygons), (33, 33, 33));
}
