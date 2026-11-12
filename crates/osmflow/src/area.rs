//! Which closed ways are areas rather than closed lines, by their tags.

use std::collections::HashMap;

use geo_types::{Coord, LineString, MultiPolygon, Polygon};

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
/// first repeated at the end; chains that do not close, and rings of fewer than four nodes, are left out.
pub(crate) fn closed_rings<'w>(ways: impl IntoIterator<Item = &'w [i64]>) -> Vec<Vec<i64>> {
    let mut rings = Vec::new();
    let mut open: Vec<&[i64]> = Vec::new();
    for way in ways {
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

/// Twice the signed area of a closed ring: positive when it runs counterclockwise.
fn signed_area(ring: &LineString<f64>) -> f64 {
    ring.0
        .windows(2)
        .map(|w| w[0].x * w[1].y - w[1].x * w[0].y)
        .sum()
}

/// Whether `point` is inside the closed ring, by the even-odd rule; a point on its edge may count as either.
fn inside(point: Coord<f64>, ring: &LineString<f64>) -> bool {
    let mut inside = false;
    for w in ring.0.windows(2) {
        let (a, b) = (w[0], w[1]);
        if (a.y > point.y) != (b.y > point.y)
            && point.x < (b.x - a.x) * (point.y - a.y) / (b.y - a.y) + a.x
        {
            inside = !inside;
        }
    }
    inside
}

/// Whether `inner`, a ring that does not cross `outer`, lies inside it: tested at a vertex of `inner` that is not one
/// of `outer`'s, as rings of a relation may touch at a node.
fn ring_inside(inner: &LineString<f64>, outer: &LineString<f64>) -> bool {
    let shared = |c: &Coord<f64>| outer.0.contains(c);
    inner
        .0
        .iter()
        .find(|c| !shared(c))
        .is_some_and(|&c| inside(c, outer))
}

/// The rings as polygons: a ring inside an odd number of others is a hole of the smallest ring around it, every other
/// ring the outer ring of a polygon. Outer rings are made to run counterclockwise, holes clockwise.
pub(crate) fn nest(rings: Vec<LineString<f64>>) -> MultiPolygon<f64> {
    let mut rings: Vec<(f64, LineString<f64>)> = rings
        .into_iter()
        .map(|ring| (signed_area(&ring), ring))
        .collect();
    rings.sort_by(|a, b| b.0.abs().total_cmp(&a.0.abs()));
    // For each ring, larger first: its depth, and the polygon it is the outer ring of, or a hole of.
    let mut depth: Vec<usize> = Vec::with_capacity(rings.len());
    let mut polygon_of: Vec<usize> = Vec::with_capacity(rings.len());
    let mut polygons: Vec<(LineString<f64>, Vec<LineString<f64>>)> = Vec::new();
    for i in 0..rings.len() {
        let parent = (0..i)
            .rev()
            .find(|&j| ring_inside(&rings[i].1, &rings[j].1));
        let d = parent.map_or(0, |j| depth[j] + 1);
        depth.push(d);
        let (area, ring) = (rings[i].0, rings[i].1.clone());
        if d % 2 == 0 {
            let ring = if area < 0.0 { reversed(ring) } else { ring };
            polygon_of.push(polygons.len());
            polygons.push((ring, Vec::new()));
        } else {
            let parent = polygon_of[parent.expect("a ring at an odd depth is inside another")];
            let ring = if area > 0.0 { reversed(ring) } else { ring };
            polygon_of.push(parent);
            polygons[parent].1.push(ring);
        }
    }
    MultiPolygon::new(
        polygons
            .into_iter()
            .map(|(outer, holes)| Polygon::new(outer, holes))
            .collect(),
    )
}

fn reversed(mut ring: LineString<f64>) -> LineString<f64> {
    ring.0.reverse();
    ring
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
