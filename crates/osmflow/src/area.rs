//! Which closed ways are areas rather than closed lines, by their tags.

use std::collections::HashMap;

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
