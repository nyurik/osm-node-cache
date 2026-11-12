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

/// The area of the ways over these points, which must be valid.
fn area(points: &[(i32, i32)], ways: &[&[i64]]) -> geo_types::MultiPolygon<f64> {
    use geo::Validation as _;
    let store = grid(points);
    let area = Locator::new(store.reader())
        .multi_polygon(ways.iter().copied())
        .unwrap();
    assert!(area.is_valid(), "{area:?}");
    area
}

/// The polygons as rings of (x, y) in whole degrees, outer ring first; each ring starts at its smallest point, keeping
/// its direction, and the polygons are in the order of their first points.
#[expect(clippy::cast_possible_truncation)]
fn rings(area: &geo_types::MultiPolygon<f64>) -> Vec<Vec<Vec<(i32, i32)>>> {
    let ring = |r: &geo_types::LineString<f64>| {
        let mut points: Vec<(i32, i32)> = r.coords().map(|c| (c.x as i32, c.y as i32)).collect();
        points.pop();
        let first = (0..points.len()).min_by_key(|&i| points[i]).unwrap();
        points.rotate_left(first);
        points.push(points[0]);
        points
    };
    let mut polygons: Vec<Vec<Vec<(i32, i32)>>> = area
        .0
        .iter()
        .map(|p| {
            iter::once(ring(p.exterior()))
                .chain(p.interiors().iter().map(ring))
                .collect()
        })
        .collect();
    polygons.sort();
    polygons
}

// 1-4 a square of 10, 5-8 a square of 2 inside it
const NESTED: [(i32, i32); 8] = [
    (0, 0),
    (10, 0),
    (10, 10),
    (0, 10),
    (4, 4),
    (4, 6),
    (6, 6),
    (6, 4),
];

/// The square of nodes 1-4 of [`NESTED`] as [`rings`] gives it.
const SQUARE: [(i32, i32); 5] = [(0, 0), (10, 0), (10, 10), (0, 10), (0, 0)];

/// Asserts that the ways over [`NESTED`] make only the square of nodes 1-4.
fn assert_only_the_square(ways: &[&[i64]]) {
    assert_eq!(rings(&area(&NESTED, ways)), [[SQUARE.to_vec()]]);
}

#[test]
fn split_and_reversed_ways_join_into_an_outer_ring_with_a_hole() {
    let ways: [&[i64]; 3] = [&[3, 2, 1], &[5, 6, 7, 8, 5], &[3, 4, 1]];
    assert_eq!(
        rings(&area(&NESTED, &ways)),
        [[
            SQUARE.to_vec(),
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
    let ways: [&[i64]; 3] = [&[9, 10, 11, 12, 9], &[1, 2, 3, 4, 1], &[5, 6, 7, 8, 5]];
    assert_eq!(
        rings(&area(&points, &ways)),
        [
            vec![
                SQUARE.to_vec(),
                vec![(2, 2), (2, 8), (8, 8), (8, 2), (2, 2)],
            ],
            vec![vec![(4, 4), (6, 4), (6, 6), (4, 6), (4, 4)]],
        ]
    );
}

#[test]
fn rings_touching_at_a_node_are_two_polygons() {
    // two squares that share node 3, each of two ways: four way ends meet there
    let points = [(0, 0), (5, 0), (5, 5), (0, 5), (10, 5), (10, 10), (5, 10)];
    let ways: [&[i64]; 4] = [&[1, 2, 3], &[3, 5, 6], &[6, 7, 3], &[3, 4, 1]];
    assert_eq!(
        rings(&area(&points, &ways)),
        [
            vec![vec![(0, 0), (5, 0), (5, 5), (0, 5), (0, 0)]],
            vec![vec![(5, 5), (10, 5), (10, 10), (5, 10), (5, 5)]],
        ]
    );
}

#[test]
fn a_hole_touching_its_outer_ring_at_a_node_stays_a_hole() {
    // a triangle hole whose corner is node 3, a corner of the outer ring, both split into two ways
    let points = [(0, 0), (10, 0), (10, 10), (0, 10), (8, 5), (5, 8)];
    let ways: [&[i64]; 4] = [&[1, 2, 3], &[3, 5, 6], &[6, 3], &[3, 4, 1]];
    assert_eq!(
        rings(&area(&points, &ways)),
        [[SQUARE.to_vec(), vec![(5, 8), (10, 10), (8, 5), (5, 8)],]]
    );
}

#[test]
fn an_edge_two_rings_share_cancels_out() {
    // a hole on the outer ring's right edge, from 5 to 6, notches the outer ring
    let points = [
        (0, 0),
        (10, 0),
        (10, 10),
        (0, 10),
        (10, 3),
        (10, 7),
        (6, 7),
        (6, 3),
    ];
    let ways: [&[i64]; 2] = [&[1, 2, 5, 6, 3, 4, 1], &[5, 6, 7, 8, 5]];
    assert_eq!(
        rings(&area(&points, &ways)),
        [[vec![
            (0, 0),
            (10, 0),
            (10, 3),
            (6, 3),
            (6, 7),
            (10, 7),
            (10, 10),
            (0, 10),
            (0, 0)
        ]]]
    );
}

#[test]
fn outer_rings_that_overlap_leave_the_overlap_out() {
    let points = [
        (0, 0),
        (6, 0),
        (6, 6),
        (0, 6),
        (3, 3),
        (9, 3),
        (9, 9),
        (3, 9),
    ];
    let ways: [&[i64]; 2] = [&[1, 2, 3, 4, 1], &[5, 6, 7, 8, 5]];
    assert_eq!(
        rings(&area(&points, &ways)),
        [
            vec![vec![(0, 0), (6, 0), (6, 3), (3, 3), (3, 6), (0, 6), (0, 0)]],
            vec![vec![(3, 6), (6, 6), (6, 3), (9, 3), (9, 9), (3, 9), (3, 6)]],
        ]
    );
}

#[test]
fn a_way_listed_twice_counts_once() {
    let ways: [&[i64]; 3] = [&[1, 2, 3], &[3, 4, 1], &[3, 4, 1]];
    assert_only_the_square(&ways);
}

#[test]
fn a_chain_that_does_not_close_is_left_out() {
    let ways: [&[i64]; 2] = [&[1, 2, 3, 4, 1], &[5, 6, 7]];
    assert_only_the_square(&ways);
}

#[test]
fn a_dangling_way_at_a_ring_node_does_not_break_the_ring() {
    let ways: [&[i64]; 4] = [&[2, 3], &[1, 5], &[3, 4, 1], &[1, 2]];
    assert_only_the_square(&ways);
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

/// The area's locations in 1e-7 degrees, from a ring of these (lon, lat) in 1e-7 degrees.
#[expect(clippy::cast_possible_truncation)]
fn round_trip(points: &[(i32, i32)]) -> Vec<(i32, i32)> {
    use osmflow::nodes::NodeStore as _;
    let mut store = MemoryStore::new();
    for (id, &(lon, lat)) in (1..).zip(points) {
        store.set(id, Location::new(lat, lon)).unwrap();
    }
    let ring: Vec<i64> = (1..=i64::try_from(points.len()).unwrap())
        .chain([1])
        .collect();
    let area = Locator::new(store.reader())
        .multi_polygon([ring.as_slice()])
        .unwrap();
    let mut out: Vec<(i32, i32)> = area.0[0]
        .exterior()
        .coords()
        .map(|c| ((c.x * 1e7).round() as i32, (c.y * 1e7).round() as i32))
        .collect();
    out.pop();
    out.sort_unstable();
    out
}

#[test]
fn locations_are_kept_exactly() {
    // 100 degrees across, at odd units of 1e-7 degrees
    let points = [
        (-500_000_001, -3),
        (499_999_999, -3),
        (499_999_999, 123_456_789),
        (-500_000_001, 123_456_789),
    ];
    let mut expected = points.to_vec();
    expected.sort_unstable();
    assert_eq!(round_trip(&points), expected);
}

#[test]
fn rings_wider_than_214_degrees_round_to_2e_7_degrees() {
    let points = [
        (-1_799_999_999, -3),
        (1_799_999_999, -3),
        (1_799_999_999, 123_456_789),
        (-1_799_999_999, 123_456_789),
    ];
    for ((lon, lat), (x, y)) in round_trip(&points).into_iter().zip({
        let mut p = points.to_vec();
        p.sort_unstable();
        p
    }) {
        assert!(
            (lon - x).abs() <= 1 && (lat - y).abs() <= 1,
            "{lon},{lat} from {x},{y}"
        );
    }
}

#[test]
fn monaco_multipolygons_assemble() {
    use geo::Validation as _;
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
            if relation
                .tags()
                .any(|tag| tag == ("type", "multipolygon") || tag == ("type", "boundary"))
            {
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
    // members outside the extract leave a node without a location, or rings that do not close
    let (mut built, mut polygons, mut missing, mut open) = (0, 0, 0, 0);
    for members in &relations {
        let refs = members
            .iter()
            .filter_map(|id| ways.get(id).map(Vec::as_slice));
        match locator.multi_polygon(refs) {
            Ok(area) => {
                assert!(area.is_valid(), "{members:?}");
                built += 1;
                polygons += area.0.len();
            }
            Err(GeometryError::MissingNode(_)) => missing += 1,
            Err(GeometryError::NoRing) => open += 1,
            Err(e) => panic!("{members:?}: {e}"),
        }
    }
    assert_eq!(
        (relations.len(), built, polygons, missing, open),
        (72, 42, 43, 0, 30)
    );
}
