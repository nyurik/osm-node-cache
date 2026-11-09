//! The types the PBF reader, the node stores and the osmChange parser share.

/// A node location in 1e-7 degrees, as OSM PBF and osmChange files store it: the same values as a PBF node's
/// `decimicro_lat` and `decimicro_lon`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[repr(C)]
pub struct Location {
    /// Latitude in 1e-7 degrees: ±900,000,000 at the poles.
    pub lat: i32,
    /// Longitude in 1e-7 degrees: ±1,800,000,000 at the antimeridian.
    pub lon: i32,
}

impl From<(i32, i32)> for Location {
    fn from((lat, lon): (i32, i32)) -> Self {
        Self { lat, lon }
    }
}

impl Location {
    /// A location from latitude and longitude in 1e-7 degrees, unchecked; see [`Self::is_valid`].
    #[must_use]
    pub fn new(lat: i32, lon: i32) -> Self {
        Self { lat, lon }
    }

    /// Whether the location is within ±90° latitude and ±180° longitude, as stores require.
    #[must_use]
    pub fn is_valid(self) -> bool {
        (-900_000_000..=900_000_000).contains(&self.lat)
            && (-1_800_000_000..=1_800_000_000).contains(&self.lon)
    }

    /// Rounds degrees to the nearest 1e-7 degree.
    #[must_use]
    #[expect(clippy::cast_possible_truncation)]
    pub fn from_degrees(lat: f64, lon: f64) -> Self {
        Self::new((lat * 1e7).round() as i32, (lon * 1e7).round() as i32)
    }

    /// Latitude in degrees, exact to the stored 1e-7 degree.
    #[must_use]
    pub fn lat_degrees(self) -> f64 {
        f64::from(self.lat) / 1e7
    }

    /// Longitude in degrees, exact to the stored 1e-7 degree.
    #[must_use]
    pub fn lon_degrees(self) -> f64 {
        f64::from(self.lon) / 1e7
    }
}

/// The type of a relation member.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MemberType {
    /// Numbered 0 in PBF files.
    Node,
    /// Numbered 1 in PBF files.
    Way,
    /// Numbered 2 in PBF files.
    Relation,
}

/// A relation member.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Member<'a> {
    /// What the member is, which says what [`id`](Self::id) is the ID of.
    pub kind: MemberType,
    /// The member object's ID. The object need not be in the same file.
    pub id: i64,
    /// The member's part in the relation, such as `outer` or `stop`; often empty.
    pub role: &'a str,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn degrees_are_the_nearest_f64() {
        // Multiplying by 1e-7, which an f64 cannot hold exactly, gives 13.054941099999999 and -122.29099629999999.
        let loc = Location::new(130_549_411, -1_222_909_963);
        assert_eq!(
            (loc.lat_degrees(), loc.lon_degrees()),
            (13.054_941_1, -122.290_996_3)
        );
        let edge = Location::new(-900_000_000, 1_800_000_000);
        assert_eq!((edge.lat_degrees(), edge.lon_degrees()), (-90.0, 180.0));
        assert_eq!(Location::from_degrees(13.054_941_1, -122.290_996_3), loc);
    }
}
