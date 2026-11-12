//! Ways as geometries: node locations loaded into a store, and looked up for each way.

use std::collections::HashMap;
use std::iter;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};

use geo::Validation as _;
use geo_types::{Geometry, LineString, MultiPolygon};
use osmflow::nodes::{MemoryStore, NodeRead as _, NodeStore as _};
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

/// A degree in 1e-7 degrees, the unit of a [`Location`].
const DEGREE: i32 = 10_000_000;

/// The area of ways through these (x, y) points, in multiples of `unit` 1e-7 degrees; points at the same place are the
/// same node.
fn multi_polygon_in(unit: i32, ways: &[&[(i32, i32)]]) -> Result<MultiPolygon<f64>, GeometryError> {
    let mut store = MemoryStore::new();
    let mut ids = HashMap::new();
    let ways: Vec<Vec<i64>> = ways
        .iter()
        .map(|way| {
            way.iter()
                .map(|&(x, y)| {
                    let next = ids.len() as u64 + 1;
                    *ids.entry((x, y)).or_insert_with(|| {
                        store.set(next, Location::new(y * unit, x * unit)).unwrap();
                        i64::try_from(next).unwrap()
                    })
                })
                .collect()
        })
        .collect();
    Locator::new(store.reader()).multi_polygon(ways.iter().map(Vec::as_slice))
}

/// The polygons of the area, which must be valid, as rings of (x, y) in multiples of `unit` 1e-7 degrees, outer ring
/// first; each ring starts at its smallest point, keeping its direction, and the polygons are in the order of their
/// first points.
fn area_in(unit: i32, ways: &[&[(i32, i32)]]) -> Vec<Vec<Vec<(i32, i32)>>> {
    let area = multi_polygon_in(unit, ways).unwrap();
    assert!(area.is_valid(), "{area:?}");
    let ring = |r: &LineString<f64>| {
        let mut points: Vec<(i32, i32)> = r
            .coords()
            .map(|c| {
                let loc = Location::from_degrees(c.y, c.x);
                (loc.lon / unit, loc.lat / unit)
            })
            .collect();
        points.pop();
        let first = (0..points.len()).min_by_key(|&i| points[i]).unwrap();
        points.rotate_left(first);
        points.push(points[0]);
        points
    };
    let mut polygons: Vec<Vec<Vec<(i32, i32)>>> = area
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

/// [`area_in`] whole degrees.
fn area(ways: &[&[(i32, i32)]]) -> Vec<Vec<Vec<(i32, i32)>>> {
    area_in(DEGREE, ways)
}

#[test]
fn split_and_reversed_ways_join_into_an_outer_ring_with_a_hole() {
    let area = area(&[
        &[(10, 10), (10, 0), (0, 0)],
        &[(4, 4), (4, 6), (6, 6), (6, 4), (4, 4)],
        &[(10, 10), (0, 10), (0, 0)],
    ]);
    assert_eq!(
        area,
        [[
            vec![(0, 0), (10, 0), (10, 10), (0, 10), (0, 0)],
            vec![(4, 4), (4, 6), (6, 6), (6, 4), (4, 4)],
        ]]
    );
}

#[test]
fn an_island_in_a_hole_is_a_polygon_of_its_own() {
    let area = area(&[
        &[(4, 4), (6, 4), (6, 6), (4, 6), (4, 4)],
        &[(0, 0), (10, 0), (10, 10), (0, 10), (0, 0)],
        &[(2, 2), (2, 8), (8, 8), (8, 2), (2, 2)],
    ]);
    assert_eq!(
        area,
        [
            vec![
                vec![(0, 0), (10, 0), (10, 10), (0, 10), (0, 0)],
                vec![(2, 2), (2, 8), (8, 8), (8, 2), (2, 2)],
            ],
            vec![vec![(4, 4), (6, 4), (6, 6), (4, 6), (4, 4)]],
        ]
    );
}

#[test]
fn rings_touching_at_a_node_are_two_polygons() {
    // two squares that share (5, 5), each of two ways: four way ends meet there
    let area = area(&[
        &[(0, 0), (5, 0), (5, 5)],
        &[(5, 5), (10, 5), (10, 10)],
        &[(10, 10), (5, 10), (5, 5)],
        &[(5, 5), (0, 5), (0, 0)],
    ]);
    assert_eq!(
        area,
        [
            [[(0, 0), (5, 0), (5, 5), (0, 5), (0, 0)]],
            [[(5, 5), (10, 5), (10, 10), (5, 10), (5, 5)]],
        ]
    );
}

#[test]
fn a_hole_touching_its_outer_ring_at_a_node_stays_a_hole() {
    // a triangle hole with a corner at (10, 10), a corner of the outer ring, both split into two ways there
    let area = area(&[
        &[(0, 0), (10, 0), (10, 10)],
        &[(10, 10), (8, 5), (5, 8)],
        &[(5, 8), (10, 10)],
        &[(10, 10), (0, 10), (0, 0)],
    ]);
    assert_eq!(
        area,
        [[
            vec![(0, 0), (10, 0), (10, 10), (0, 10), (0, 0)],
            vec![(5, 8), (10, 10), (8, 5), (5, 8)],
        ]]
    );
}

#[test]
fn an_edge_two_rings_share_cancels_out() {
    // a hole on the outer ring's right edge, from (10, 3) to (10, 7), notches the outer ring
    let area = area(&[
        &[(0, 0), (10, 0), (10, 3), (10, 7), (10, 10), (0, 10), (0, 0)],
        &[(10, 3), (10, 7), (6, 7), (6, 3), (10, 3)],
    ]);
    assert_eq!(
        area,
        [[[
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
    let area = area(&[
        &[(0, 0), (6, 0), (6, 6), (0, 6), (0, 0)],
        &[(3, 3), (9, 3), (9, 9), (3, 9), (3, 3)],
    ]);
    assert_eq!(
        area,
        [
            [[(0, 0), (6, 0), (6, 3), (3, 3), (3, 6), (0, 6), (0, 0)]],
            [[(3, 6), (6, 6), (6, 3), (9, 3), (9, 9), (3, 9), (3, 6)]],
        ]
    );
}

#[test]
fn a_way_listed_twice_counts_once() {
    let area = area(&[
        &[(0, 0), (10, 0), (10, 10)],
        &[(10, 10), (0, 10), (0, 0)],
        &[(10, 10), (0, 10), (0, 0)],
    ]);
    assert_eq!(area, [[[(0, 0), (10, 0), (10, 10), (0, 10), (0, 0)]]]);
}

#[test]
fn a_chain_that_does_not_close_is_left_out() {
    let area = area(&[
        &[(0, 0), (10, 0), (10, 10), (0, 10), (0, 0)],
        &[(4, 4), (4, 6), (6, 6)],
    ]);
    assert_eq!(area, [[[(0, 0), (10, 0), (10, 10), (0, 10), (0, 0)]]]);
}

#[test]
fn a_dangling_way_at_a_ring_node_does_not_break_the_ring() {
    let area = area(&[
        &[(10, 0), (10, 10)],
        // from a corner of the ring to nowhere
        &[(0, 0), (4, 4)],
        &[(10, 10), (0, 10), (0, 0)],
        &[(0, 0), (10, 0)],
    ]);
    assert_eq!(area, [[[(0, 0), (10, 0), (10, 10), (0, 10), (0, 0)]]]);
}

#[test]
fn ways_that_close_no_ring_are_an_error() {
    let area = multi_polygon_in(
        DEGREE,
        &[&[(0, 0), (10, 0), (10, 10)], &[(10, 10), (0, 10)]],
    );
    assert_eq!(area, Err(GeometryError::NoRing));
}

#[test]
fn a_ring_node_without_a_location_is_an_error() {
    let mut store = MemoryStore::new();
    store.set(1, Location::new(0, 0)).unwrap();
    store.set(2, Location::new(0, 10 * DEGREE)).unwrap();
    store.set(4, Location::new(10 * DEGREE, 0)).unwrap();
    // node 3, a corner of the square, is not in the store
    let square: &[i64] = &[1, 2, 3, 4, 1];
    let area = Locator::new(store.reader()).multi_polygon([square]);
    assert_eq!(area, Err(GeometryError::MissingNode(3)));
}

#[test]
fn locations_are_kept_exactly() {
    // 100 degrees across, at odd units of 1e-7 degrees
    let (west, east, south, north) = (-500_000_001, 499_999_999, -3, 123_456_789);
    let square = [
        (west, south),
        (east, south),
        (east, north),
        (west, north),
        (west, south),
    ];
    assert_eq!(area_in(1, &[&square]), [[square]]);
}

#[test]
fn locations_are_kept_exactly_around_the_world() {
    let (west, east, south, north) = (-1_799_999_999, 1_799_999_999, -3, 123_456_789);
    let square = [
        (west, south),
        (east, south),
        (east, north),
        (west, north),
        (west, south),
    ];
    assert_eq!(area_in(1, &[&square]), [[square]]);
}

/// What became of each multipolygon relation.
#[derive(Debug, Default, PartialEq)]
struct Outcomes {
    built: usize,
    polygons: usize,
    missing_node: usize,
    no_ring: usize,
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
    assert_eq!(relations.len(), 72);
    let mut locator = Locator::new(store.reader());
    let mut outcomes = Outcomes::default();
    for members in &relations {
        // members outside the extract are missing, and leave rings that do not close
        let refs = members
            .iter()
            .filter_map(|id| ways.get(id).map(Vec::as_slice));
        match locator.multi_polygon(refs) {
            Ok(area) => {
                assert!(area.is_valid(), "{members:?}");
                outcomes.built += 1;
                outcomes.polygons += area.0.len();
            }
            Err(GeometryError::MissingNode(_)) => outcomes.missing_node += 1,
            Err(GeometryError::NoRing) => outcomes.no_ring += 1,
            Err(e) => panic!("{members:?}: {e}"),
        }
    }
    assert_eq!(
        outcomes,
        Outcomes {
            built: 42,
            polygons: 43,
            missing_node: 0,
            no_ring: 30,
        }
    );
}
