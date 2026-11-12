//! The areas of multipolygon relations: their member ways joined into closed rings, and the area the rings cover by
//! the even-odd rule.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};

use geo_types::{LineString, MultiPolygon, Polygon};
use i_overlay::core::fill_rule::FillRule;
use i_overlay::core::overlay::{IntOverlayOptions, Overlay, ShapeType};
use i_overlay::core::overlay_rule::OverlayRule;
use i_overlay::core::solver::Solver;
use i_overlay::i_float::int::point::IntPoint;

use crate::Location;
use crate::resolve::coord;

/// A way that does not end where it starts, with its end nodes.
#[derive(Clone, Copy)]
struct Open<'w> {
    nodes: &'w [i64],
    first: i64,
    last: i64,
}

/// The indexes of the open ways that end at each node.
type Ends = HashMap<i64, Vec<usize>>;

/// The closed rings the ways make, joined end to end where they share an end node, each as its node IDs with the
/// first repeated at the end; a way listed twice counts once, and chains that do not close, and rings of fewer than
/// four nodes, are left out.
pub(crate) fn closed_rings<'w>(ways: impl IntoIterator<Item = &'w [i64]>) -> Vec<Cow<'w, [i64]>> {
    let mut rings = Vec::new();
    let mut open = Vec::new();
    let mut seen = HashSet::new();
    for way in ways {
        if !seen.insert(way) {
            continue;
        }
        match *way {
            [first, .., last] if first == last => rings.push(Cow::Borrowed(way)),
            [first, .., last] => open.push(Some(Open {
                nodes: way,
                first,
                last,
            })),
            _ => {}
        }
    }
    let mut ends = Ends::new();
    for (i, way) in open.iter().flatten().enumerate() {
        ends.entry(way.first).or_default().push(i);
        ends.entry(way.last).or_default().push(i);
    }
    drop_dangling(&mut open, &ends);
    for i in 0..open.len() {
        if let Some(start) = open[i].take() {
            rings.extend(chain(start, &mut open, &ends).map(Cow::Owned));
        }
    }
    rings.retain(|ring| ring.len() >= 4);
    rings
}

/// Takes out a way not yet taken that ends at `node`.
fn take_at<'w>(open: &mut [Option<Open<'w>>], ends: &Ends, node: i64) -> Option<Open<'w>> {
    ends[&node].iter().find_map(|&i| open[i].take())
}

/// Takes out the ways with an end no other way reaches, and the ways that leaves dangling in turn: such a way is in no
/// ring, and a chain that followed one off a ring could not close it.
fn drop_dangling(open: &mut [Option<Open<'_>>], ends: &Ends) {
    let left = |open: &[Option<Open<'_>>], node| {
        ends[&node].iter().filter(|&&i| open[i].is_some()).count()
    };
    let mut dangling: Vec<i64> = ends
        .iter()
        .filter(|(_, ways)| ways.len() == 1)
        .map(|(&node, _)| node)
        .collect();
    while let Some(node) = dangling.pop() {
        if let Some(way) = take_at(open, ends, node) {
            dangling.extend(
                [way.first, way.last]
                    .into_iter()
                    .filter(|&end| left(open, end) == 1),
            );
        }
    }
}

/// The nodes of `start` and of the ways taken out after it, each one that ends where the chain does, until the chain
/// comes back to where it started; `None` if it does not.
fn chain(start: Open<'_>, open: &mut [Option<Open<'_>>], ends: &Ends) -> Option<Vec<i64>> {
    let mut nodes = start.nodes.to_vec();
    let mut end = start.last;
    while end != start.first {
        let next = take_at(open, ends, end)?;
        if next.first == end {
            nodes.extend_from_slice(&next.nodes[1..]);
            end = next.last;
        } else {
            nodes.extend(next.nodes.iter().rev().skip(1));
            end = next.first;
        }
    }
    Some(nodes)
}

/// The area the closed rings cover by the even-odd rule, as valid polygons, in degrees; each ring is its locations,
/// with the first repeated at the end.
///
/// A point is in the area where it is inside an odd number of rings: a ring inside another is a hole of it, a ring
/// inside that an island, and an edge two rings share cancels out. Rings may touch each other or themselves at a node,
/// or cross: rings that touch are split there, and rings that cross get a vertex where they cross, on the 1e-7 degree
/// grid. Outer rings run counterclockwise, holes clockwise. Every location is kept as it is: the overlay runs on
/// `i64`s, whose range holds any location.
pub(crate) fn even_odd(rings: &[Vec<Location>]) -> MultiPolygon<f64> {
    let options = IntOverlayOptions {
        preserve_input_collinear: true,
        preserve_output_collinear: true,
        ..IntOverlayOptions::ogc()
    };
    let edges = rings.iter().map(Vec::len).sum();
    let mut overlay = Overlay::new_custom(edges, options, Solver::default());
    for ring in rings {
        // the first location is repeated at the end, and the overlay closes each path itself
        let points = ring
            .iter()
            .skip(1)
            .map(|loc| IntPoint::new(i64::from(loc.lon), i64::from(loc.lat)));
        overlay.add_path_iter(points, ShapeType::Subject);
    }
    overlay
        .overlay(OverlayRule::Subject, FillRule::EvenOdd)
        .into_iter()
        .filter_map(|shape| {
            let mut contours = shape.into_iter().map(closed_ring);
            Some(Polygon::new(contours.next()?, contours.collect()))
        })
        .collect()
}

/// A contour of the overlay, its first point not repeated, as a closed ring in degrees.
fn closed_ring(contour: Vec<IntPoint<i64>>) -> LineString<f64> {
    let narrow =
        |v: i64| i32::try_from(v).expect("the overlay's points are within the bounds of its input");
    let first = contour.first().copied();
    contour
        .into_iter()
        .chain(first)
        .map(|p| coord(Location::new(narrow(p.y), narrow(p.x))))
        .collect()
}
