use std::fs::{File, OpenOptions};
use std::path::Path;

use crate::{OsmNodeCacheError, OsmNodeCacheResult};

const LAT_I32_RATE: f64 = i32::MAX as f64 / 90_f64;
const I32_LAT_RATE: f64 = 1_f64 / LAT_I32_RATE;
const LON_I32_RATE: f64 = i32::MAX as f64 / 180_f64;
const I32_LON_RATE: f64 = 1_f64 / LON_I32_RATE;

pub trait CacheStore {
    /// Create a thread-safe caching accessor
    fn get_accessor(&self) -> Box<dyn Cache + '_>;
}

pub trait Cache {
    fn get(&self, index: usize) -> u64;
    fn set(&mut self, index: usize, value: u64);

    /// Get latitude/longitude by decoding them from the u64 value treated as two packed i32 values.
    #[inline]
    fn get_lat_lon(&self, index: usize) -> (f64, f64) {
        let (lat, lon) = u64_to_i32s(self.get(index));
        (i32_to_latitude(lat), i32_to_longitude(lon))
    }

    /// Store latitude/longitude by encoding them as two i32 values, normalized on (-180..180) and (-90..90) ranges.
    #[inline]
    fn set_lat_lon(&mut self, index: usize, lat: f64, lon: f64) {
        self.set(
            index,
            i32s_to_u64(latitude_to_i32(lat), longitude_to_i32(lon)),
        );
    }
}

#[inline]
fn latitude_to_i32(value: f64) -> i32 {
    if (-90_f64..=90_f64).contains(&value) {
        (value * LAT_I32_RATE) as i32
    } else {
        panic!("Invalid latitude {value}")
    }
}

#[inline]
fn i32_to_latitude(value: i32) -> f64 {
    (value as f64) * I32_LAT_RATE
}

#[inline]
fn longitude_to_i32(value: f64) -> i32 {
    if (-180_f64..=180_f64).contains(&value) {
        (value * LON_I32_RATE) as i32
    } else {
        // experimental
        f64::round(((value + 180_f64) % 360_f64 - 180_f64) * LON_I32_RATE) as i32
    }
}

#[inline]
fn i32_to_longitude(value: i32) -> f64 {
    (value as f64) * I32_LON_RATE
}

#[inline]
fn u64_to_i32s(value: u64) -> (i32, i32) {
    ((value >> 32) as i32, value as i32)
}

#[inline]
fn i32s_to_u64(high: i32, low: i32) -> u64 {
    (high as u32 as u64) << 32 | (low as u32 as u64)
}

pub fn open_cache_file<P: AsRef<Path>>(filename: P) -> OsmNodeCacheResult<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(filename.as_ref())
        .map_err(|e| OsmNodeCacheError::InvalidCacheFile(filename.as_ref().to_path_buf(), e))?;
    Ok(file)
}

#[cfg(test)]
pub mod tests {
    use std::panic;
    use std::panic::{catch_unwind, UnwindSafe};

    use rand::seq::SliceRandom;
    use rand::thread_rng;

    use crate::traits::{
        i32_to_latitude, i32_to_longitude, i32s_to_u64, latitude_to_i32, longitude_to_i32,
        u64_to_i32s,
    };

    const EPSILON: f64 = f32::EPSILON as f64;

    fn eq(a: f64, b: f64) -> bool {
        (a - b).abs() > EPSILON
    }

    fn assert_floats(expected: f64, actual: f64) {
        if eq(expected, actual) {
            panic!(
                "Assert failed: expected={expected}, actual={actual}, delta={}",
                (expected - actual).abs()
            );
        }
    }

    macro_rules! test_lat {
        ( $stored:expr, $value:expr, $expected:expr ) => {
            assert_eq!($stored, latitude_to_i32($value));
            assert_floats($expected, i32_to_latitude(latitude_to_i32($value)));
        };
        ( $stored:expr, $value:expr ) => {
            assert_eq!($stored, latitude_to_i32($value));
            assert_floats($value, i32_to_latitude(latitude_to_i32($value)));
        };
    }
    macro_rules! test_lon {
        ( $stored:expr, $value:expr, $expected:expr ) => {
            assert_eq!($stored, longitude_to_i32($value));
            assert_floats($expected, i32_to_longitude(longitude_to_i32($value)));
        };
        ( $stored:expr, $value:expr ) => {
            assert_eq!($stored, longitude_to_i32($value));
            assert_floats($value, i32_to_longitude(longitude_to_i32($value)));
        };
    }

    fn assert_panic<F: FnOnce() -> R + UnwindSafe, R: std::fmt::Debug>(f: F) {
        let handler = panic::take_hook();
        panic::set_hook(Box::new(|_| {}));
        let res = catch_unwind(f);
        panic::set_hook(handler);
        assert!(
            res.is_err(),
            "Expected a panic, but received {:?} instead",
            res.unwrap()
        );
    }

    #[test]
    fn test_latitude() {
        test_lat!(0, 0.0);
        test_lat!(0, 0.00000001);
        test_lat!(0, -0.00000001);
        test_lat!(2, 0.0000001);
        test_lat!(-2, -0.0000001);
        test_lat!(23, 0.000001);
        test_lat!(-23, -0.000001);
        test_lat!(238, 0.00001);
        test_lat!(-238, -0.00001);
        test_lat!(2386, 0.0001);
        test_lat!(-2386, -0.0001);
        test_lat!(23860, 0.001);
        test_lat!(-23860, -0.001);
        test_lat!(238609, 0.01);
        test_lat!(2386092, 0.1);
        test_lat!(23860929, 1.0);
        test_lat!(2147483408, 89.99999);
        test_lat!(2147483623, 89.999999);
        test_lat!(2147483644, 89.9999999);
        test_lat!(2147483646, 89.99999999);
        test_lat!(2147483646, 89.999999999);
        test_lat!(2147483644, 90_f64 - EPSILON);
        test_lat!(-2147483644, -90_f64 + EPSILON);
        test_lat!(2147483647, 90.0);

        assert_panic(|| latitude_to_i32(90_f64 + EPSILON));
        assert_panic(|| latitude_to_i32(-90_f64 - EPSILON));
    }

    #[test]
    fn test_longitude() {
        test_lon!(0, 0.0);
        test_lon!(0, 0.00000001);
        test_lon!(0, -0.00000001);
        test_lon!(1, 0.0000001);
        test_lon!(-1, -0.0000001);
        test_lon!(23, 0.000002);
        test_lon!(-23, -0.000002);
        test_lon!(119, 0.00001);
        test_lon!(-119, -0.00001);
        test_lon!(1193, 0.0001);
        test_lon!(-1193, -0.0001);
        test_lon!(11930, 0.001);
        test_lon!(-11930, -0.001);
        test_lon!(119304, 0.01);
        test_lon!(1193046, 0.1);
        test_lon!(11930464, 1.0);
        test_lon!(2147483527, 179.99999);
        test_lon!(2147483635, 179.999999);
        test_lon!(2147483645, 179.9999999);
        test_lon!(2147483646, 179.99999999);
        test_lon!(2147483646, 179.999999999);
        test_lon!(2147483647, 180.0);
        test_lon!(-2147483647, 180.00000001, -180.0);
        test_lon!(-2147483646, 180.0000001, -180.0);
        test_lon!(-1908874353, 200.0, -160.0);
    }

    macro_rules! test_pack {
        ( $high:expr, $low:expr ) => {{
            let (high, low) = u64_to_i32s(i32s_to_u64($high, $low));
            assert_eq!((high, low), ($high, $low));
        }};
    }

    #[test]
    fn test_pack() {
        test_pack!(0, 0);
        test_pack!(1, 2);
        test_pack!(2, 1);
        test_pack!(-1, 1);
        test_pack!(1, -1);
        test_pack!(i32::MAX, -1);
        test_pack!(-1, i32::MAX);
        test_pack!(i32::MAX, i32::MIN);
        test_pack!(i32::MIN, i32::MAX);
    }

    pub(crate) fn get_random_items(items: usize) -> Vec<usize> {
        let mut vec: Vec<usize> = (0_usize..items).collect();
        vec.shuffle(&mut thread_rng());
        vec
    }
}
