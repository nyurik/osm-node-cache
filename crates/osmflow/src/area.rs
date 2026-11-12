//! Which closed ways are areas rather than closed lines, by their tags.

use std::collections::{HashMap, HashSet};

use geo_types::{Coord, LineString, MultiPolygon, Polygon};
use i_overlay::core::fill_rule::FillRule;
use i_overlay::core::overlay::{IntOverlayOptions, Overlay};
use i_overlay::core::overlay_rule::OverlayRule;
use i_overlay::core::solver::Solver;
use i_overlay::i_float::int::point::IntPoint;

use crate::Location;

/// What a key says about a closed way that has it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Rule {
    /// An area, whatever the value but `no`.
    Any,
    /// An area with one of these values.
    Only(Vec<String>),
    /// An area with any value but these.
    Except(Vec<String>),
}

/// The built-in rules: keys whose closed ways are areas with any value, with only some values, or with all values but
/// some. Started from `polygon-features.json` of <https://github.com/tyrasd/osm-polygon-features> (CC0-1.0),
/// and expanded using the OpenStreetMap wiki tag pages.
const BUILT_IN: &[(&str, Kind, &[&str])] = &[
    (
        "aeroway",
        Kind::Except,
        &["parking_position", "runway", "taxilane", "taxiway"],
    ),
    ("amenity", Kind::Any, &[]),
    ("area", Kind::Any, &[]),
    ("area:highway", Kind::Any, &[]),
    (
        "barrier",
        Kind::Only,
        &[
            "city_wall",
            "ditch",
            "hedge",
            "retaining_wall",
            "spikes",
            "wall",
        ],
    ),
    ("boundary", Kind::Except, &["administrative"]),
    ("building", Kind::Any, &[]),
    ("building:part", Kind::Any, &[]),
    ("craft", Kind::Any, &[]),
    ("golf", Kind::Any, &[]),
    (
        "highway",
        Kind::Only,
        &["elevator", "escape", "rest_area", "services"],
    ),
    ("historic", Kind::Any, &[]),
    ("indoor", Kind::Any, &[]),
    ("landcover", Kind::Any, &[]),
    ("landuse", Kind::Any, &[]),
    ("leisure", Kind::Except, &["slipway", "track"]),
    (
        "man_made",
        Kind::Except,
        &[
            "breakwater",
            "cutline",
            "dyke",
            "embankment",
            "groyne",
            "pipeline",
        ],
    ),
    ("military", Kind::Any, &[]),
    (
        "natural",
        Kind::Except,
        &[
            "arete",
            "cliff",
            "coastline",
            "earth_bank",
            "ridge",
            "tree_row",
        ],
    ),
    ("office", Kind::Any, &[]),
    ("place", Kind::Any, &[]),
    (
        "power",
        Kind::Only,
        &["generator", "plant", "substation", "transformer"],
    ),
    ("public_transport", Kind::Any, &[]),
    (
        "railway",
        Kind::Only,
        &["platform", "roundhouse", "station", "turntable"],
    ),
    ("ruins", Kind::Any, &[]),
    ("shop", Kind::Any, &[]),
    ("tourism", Kind::Any, &[]),
    (
        "waterway",
        Kind::Only,
        &["boatyard", "dam", "dock", "riverbank"],
    ),
];

#[derive(Clone, Copy)]
enum Kind {
    Any,
    Only,
    Except,
}

/// Which closed ways are areas rather than closed lines, by their tags: a closed way is an area when one of its tags
/// says so, and `area=yes` or `area=no` overrides every other tag. Only closed ways are tested; an open way is always
/// a line.
///
/// [`AreaRules::default`] has a built-in list, started from the CC0 list that osmtogeojson and Overpass turbo use, and
/// expanded using the OpenStreetMap wiki pages of the tags:
///
/// * any value but `no` of `amenity`, `area`, `area:highway`, `building`, `building:part`, `craft`, `golf`, `historic`,
///   `indoor`, `landcover`, `landuse`, `military`, `office`, `place`, `public_transport`, `ruins`, `shop` and
///   `tourism`;
/// * any value but `no` and a few of `aeroway` (not `parking_position`, `runway`, `taxilane`, `taxiway`), `boundary`
///   (not `administrative`), `leisure` (not `slipway`, `track`), `man_made` (not `breakwater`, `cutline`, `dyke`,
///   `embankment`, `groyne`, `pipeline`) and `natural` (not `arete`, `cliff`, `coastline`, `earth_bank`, `ridge`,
///   `tree_row`);
/// * only some values of `barrier` (`city_wall`, `ditch`, `hedge`, `retaining_wall`, `spikes`, `wall`), `highway`
///   (`elevator`, `escape`, `rest_area`, `services`), `power` (`generator`, `plant`, `substation`, `transformer`),
///   `railway` (`platform`, `roundhouse`, `station`, `turntable`) and `waterway` (`boatyard`, `dam`, `dock`,
///   `riverbank`).
///
/// [`AreaRules::add`] and [`AreaRules::remove`] change the list; [`AreaRules::none`] starts from an empty one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AreaRules {
    rules: HashMap<String, Rule>,
}

impl Default for AreaRules {
    fn default() -> Self {
        let mut rules = Self::none();
        for &(key, kind, values) in BUILT_IN {
            let values = values.iter().map(|&v| v.to_owned()).collect();
            let rule = match kind {
                Kind::Any => Rule::Any,
                Kind::Only => Rule::Only(values),
                Kind::Except => Rule::Except(values),
            };
            rules.rules.insert(key.to_owned(), rule);
        }
        rules
    }
}

impl AreaRules {
    /// No rules: only `area=yes` makes a closed way an area.
    #[must_use]
    pub fn none() -> Self {
        Self {
            rules: HashMap::new(),
        }
    }

    /// Makes closed ways with `key` areas: with any value when `values` is empty, otherwise with these values too.
    pub fn add(&mut self, key: &str, values: &[&str]) -> &mut Self {
        let rule = self.rules.remove(key);
        let rule = match (rule, values) {
            (_, []) | (Some(Rule::Any), _) => Rule::Any,
            (None, _) => Rule::Only(owned(values)),
            (Some(Rule::Only(mut only)), _) => {
                only.extend(values.iter().map(|&v| v.to_owned()));
                Rule::Only(only)
            }
            (Some(Rule::Except(mut except)), _) => {
                except.retain(|v| !values.contains(&v.as_str()));
                Rule::Except(except)
            }
        };
        self.rules.insert(key.to_owned(), rule);
        self
    }

    /// Makes closed ways with `key` lines, unless another tag makes them areas: with any value when `values` is empty,
    /// otherwise with these values.
    pub fn remove(&mut self, key: &str, values: &[&str]) -> &mut Self {
        let Some(rule) = self.rules.remove(key) else {
            return self;
        };
        let rule = match (rule, values) {
            (_, []) => return self,
            (Rule::Any, _) => Rule::Except(owned(values)),
            (Rule::Only(mut only), _) => {
                only.retain(|v| !values.contains(&v.as_str()));
                if only.is_empty() {
                    return self;
                }
                Rule::Only(only)
            }
            (Rule::Except(mut except), _) => {
                except.extend(values.iter().map(|&v| v.to_owned()));
                Rule::Except(except)
            }
        };
        self.rules.insert(key.to_owned(), rule);
        self
    }

    /// Whether a closed way with these tags is an area.
    pub fn is_area<'a>(&self, tags: impl IntoIterator<Item = (&'a str, &'a str)>) -> bool {
        let mut area = false;
        for (key, value) in tags {
            if key == "area" {
                match value {
                    "yes" => return true,
                    "no" => return false,
                    _ => {}
                }
            }
            // `key=no` says the object is not one, as `building=no` does: it never makes an area.
            area |= value != "no"
                && self.rules.get(key).is_some_and(|rule| match rule {
                    Rule::Any => true,
                    Rule::Only(only) => only.iter().any(|v| v == value),
                    Rule::Except(except) => except.iter().all(|v| v != value),
                });
        }
        area
    }
}

fn owned(values: &[&str]) -> Vec<String> {
    values.iter().map(|&v| v.to_owned()).collect()
}

/// The closed rings the ways make, joined end to end where they share an end node, each as its node IDs with the
/// first repeated at the end; a way listed twice counts once, and chains that do not close, and rings of fewer than
/// four nodes, are left out.
pub(crate) fn closed_rings<'w>(ways: impl IntoIterator<Item = &'w [i64]>) -> Vec<Vec<i64>> {
    let mut rings = Vec::new();
    let mut open: Vec<&[i64]> = Vec::new();
    let mut seen = HashSet::new();
    for way in ways {
        if !seen.insert(way) {
            continue;
        }
        match (way.first(), way.last()) {
            (Some(first), Some(last)) if way.len() >= 2 && first == last => {
                rings.push(way.to_vec());
            }
            _ if way.len() >= 2 => open.push(way),
            _ => {}
        }
    }
    let mut by_end: HashMap<i64, Vec<usize>> = HashMap::new();
    for (i, way) in open.iter().enumerate() {
        by_end.entry(way[0]).or_default().push(i);
        by_end.entry(way[way.len() - 1]).or_default().push(i);
    }
    let mut used = vec![false; open.len()];
    // A way with an end no other way reaches is in no ring: dropping it, and the ways that leaves dangling in turn,
    // keeps a chain from following one off a ring it could have closed.
    let mut degree: HashMap<i64, usize> = by_end
        .iter()
        .map(|(&node, ways)| (node, ways.len()))
        .collect();
    let mut dangling: Vec<i64> = degree
        .iter()
        .filter(|&(_, &d)| d == 1)
        .map(|(&node, _)| node)
        .collect();
    while let Some(node) = dangling.pop() {
        let Some(i) = by_end[&node].iter().copied().find(|&i| !used[i]) else {
            continue;
        };
        used[i] = true;
        for end in [open[i][0], open[i][open[i].len() - 1]] {
            let d = degree
                .get_mut(&end)
                .expect("every end of a way has a degree");
            *d -= 1;
            if *d == 1 {
                dangling.push(end);
            }
        }
    }
    for start in 0..open.len() {
        if used[start] {
            continue;
        }
        used[start] = true;
        let mut chain = open[start].to_vec();
        while chain.first() != chain.last() {
            let end = *chain.last().expect("a chain has two nodes or more");
            let Some(next) = by_end
                .get(&end)
                .and_then(|ways| ways.iter().copied().find(|&i| !used[i]))
            else {
                break;
            };
            used[next] = true;
            let way = open[next];
            if way[0] == end {
                chain.extend_from_slice(&way[1..]);
            } else {
                chain.extend(way.iter().rev().skip(1));
            }
        }
        if chain.first() == chain.last() {
            rings.push(chain);
        }
    }
    rings.retain(|ring| ring.len() >= 4);
    rings
}

/// Twice the signed area of a ring of points, the first not repeated at the end: positive when it runs
/// counterclockwise. Exact, as the products of coordinates in the overlay's range fit an `i64` and their sum an `i128`.
fn signed_area_2x(ring: &[IntPoint]) -> i128 {
    let next = ring.iter().cycle().skip(1);
    ring.iter()
        .zip(next)
        .map(|(a, b)| i128::from(i64::from(a.x) * i64::from(b.y) - i64::from(b.x) * i64::from(a.y)))
        .sum()
}

/// How far from the origin `i_overlay` computes without overflow: it scales its own float input into this range.
const OVERLAY_RANGE: i64 = 1 << 30;

/// The area the closed rings cover by the even-odd rule, as valid polygons, in degrees; each ring is its locations,
/// with the first repeated at the end.
///
/// A point is in the area where it is inside an odd number of rings: a ring inside another is a hole of it, a ring
/// inside that an island, and an edge two rings share cancels out. Rings may touch each other or themselves at a node,
/// or cross: rings that touch are split there, and rings that cross get a vertex where they cross, on the integer grid.
/// Outer rings run counterclockwise, holes clockwise. Every location is kept as it is, unless the rings span more than
/// 2^31 units of 1e-7 degrees (about 214 degrees of longitude), which rounds them down to multiples of 2e-7 degrees.
pub(crate) fn even_odd(rings: &[Vec<Location>]) -> MultiPolygon<f64> {
    let (mut min, mut max) = ((i64::MAX, i64::MAX), (i64::MIN, i64::MIN));
    for loc in rings.iter().flatten() {
        let (x, y) = (i64::from(loc.lon), i64::from(loc.lat));
        min = (min.0.min(x), min.1.min(y));
        max = (max.0.max(x), max.1.max(y));
    }
    if min.0 > max.0 {
        return MultiPolygon::new(Vec::new());
    }
    // Locations relative to the middle of their bounds, halved until they fit the overlay's range
    let origin = (i64::midpoint(min.0, max.0), i64::midpoint(min.1, max.1));
    let reach = (max.0 - min.0).max(max.1 - min.1) / 2 + 1;
    let mut shift = 0;
    while reach >> shift >= OVERLAY_RANGE {
        shift += 1;
    }
    let narrow = |v: i64| i32::try_from(v).expect("the overlay's range and locations fit an i32");
    let contours: Vec<Vec<IntPoint>> = rings
        .iter()
        .map(|ring| {
            ring[..ring.len() - 1]
                .iter()
                .map(|loc| {
                    IntPoint::new(
                        narrow((i64::from(loc.lon) - origin.0) >> shift),
                        narrow((i64::from(loc.lat) - origin.1) >> shift),
                    )
                })
                .collect()
        })
        .collect();
    let options = IntOverlayOptions {
        preserve_input_collinear: true,
        preserve_output_collinear: true,
        ..IntOverlayOptions::ogc()
    };
    let shapes = Overlay::with_contours_custom(&contours, &[], options, Solver::default())
        .overlay(OverlayRule::Subject, FillRule::EvenOdd);
    // `i_overlay` gives a hole that touches its outer ring at a node counterclockwise, so each ring is turned the right
    // way here
    let ring = |mut contour: Vec<IntPoint>, counterclockwise: bool| {
        if (signed_area_2x(&contour) > 0) != counterclockwise {
            contour.reverse();
        }
        let mut coords: Vec<Coord<f64>> = contour
            .into_iter()
            .map(|p| {
                let lon = narrow((i64::from(p.x) << shift) + origin.0);
                let lat = narrow((i64::from(p.y) << shift) + origin.1);
                let loc = Location::new(lat, lon);
                Coord {
                    x: loc.lon_degrees(),
                    y: loc.lat_degrees(),
                }
            })
            .collect();
        coords.extend(coords.first().copied());
        LineString::new(coords)
    };
    MultiPolygon::new(
        shapes
            .into_iter()
            .filter_map(|shape| {
                let mut rings = shape.into_iter();
                let outer = ring(rings.next()?, true);
                Some(Polygon::new(
                    outer,
                    rings.map(|hole| ring(hole, false)).collect(),
                ))
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn built_in_rules() {
        let rules = AreaRules::default();
        assert!(rules.is_area([("building", "yes")]));
        assert!(rules.is_area([("name", "x"), ("natural", "water")]));
        assert!(!rules.is_area([("natural", "coastline")]));
        assert!(rules.is_area([("highway", "services")]));
        assert!(!rules.is_area([("highway", "pedestrian")]));
        assert!(rules.is_area([("highway", "pedestrian"), ("area", "yes")]));
        assert!(!rules.is_area([("building", "yes"), ("area", "no")]));
        assert!(!rules.is_area([("area", "no"), ("building", "yes")]));
        assert!(!rules.is_area([("barrier", "fence")]));
        assert!(!rules.is_area([]));
        assert!(AreaRules::none().is_area([("area", "yes")]));
        assert!(!AreaRules::none().is_area([("building", "yes")]));
    }

    #[test]
    fn no_is_never_an_area() {
        let rules = AreaRules::default();
        assert!(!rules.is_area([("building", "no")]));
        assert!(!rules.is_area([("natural", "no")]));
        assert!(rules.is_area([("building", "no"), ("landuse", "grass")]));
    }

    #[test]
    fn lines_and_areas_from_the_wiki() {
        let rules = AreaRules::default();
        assert!(rules.is_area([("landcover", "grass")]));
        for (key, value) in [
            ("aeroway", "runway"),
            ("boundary", "administrative"),
            ("leisure", "track"),
            ("man_made", "breakwater"),
            ("natural", "earth_bank"),
        ] {
            assert!(!rules.is_area([(key, value)]), "{key}={value}");
            assert!(
                rules.is_area([(key, value), ("area", "yes")]),
                "{key}={value}"
            );
        }
        assert!(rules.is_area([("aeroway", "apron")]));
        assert!(rules.is_area([("boundary", "protected_area")]));
        assert!(rules.is_area([("leisure", "park")]));
    }

    #[test]
    fn rules_change() {
        let mut rules = AreaRules::default();
        rules
            .add("barrier", &["fence"])
            .add("highway", &["pedestrian"]);
        assert!(rules.is_area([("barrier", "fence")]) && !rules.is_area([("barrier", "gate")]));
        assert!(
            rules.is_area([("highway", "pedestrian")]) && rules.is_area([("highway", "services")])
        );
        rules.add("natural", &["coastline"]);
        assert!(rules.is_area([("natural", "coastline")]));

        rules
            .remove("building", &["roof"])
            .remove("leisure", &["pitch"]);
        assert!(!rules.is_area([("building", "roof")]) && rules.is_area([("building", "yes")]));
        assert!(!rules.is_area([("leisure", "pitch")]) && rules.is_area([("leisure", "park")]));
        rules.remove(
            "highway",
            &[
                "services",
                "rest_area",
                "escape",
                "elevator",
                "platform",
                "pedestrian",
            ],
        );
        assert!(!rules.is_area([("highway", "services")]));
        rules.remove("amenity", &[]);
        assert!(!rules.is_area([("amenity", "parking")]));
        rules.add("amenity", &[]);
        assert!(rules.is_area([("amenity", "parking")]));
    }
}
