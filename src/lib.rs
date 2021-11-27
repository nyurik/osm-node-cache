#![cfg_attr(all(feature = "nightly", test), feature(test))]

pub use self::dense_file::DenseFileCache;

mod dense_file;

pub fn pack_f32(a: f32, b: f32) -> u64 {
    (a.to_bits() as u64) << 32 | (b.to_bits() as u64)
}

pub fn pack_i32(a: i32, b: i32) -> u64 {
    ((a as u32) as u64) << 32 | ((b as u32) as u64)
}

pub fn unpack_f32(x: u64) -> (f32, f32) {
    (f32::from_bits((x >> 32) as u32), f32::from_bits(x as u32))
}

// pub fn unpack_to_coords(x: u64) -> (f32, f32) {
//     let (lat, lon) = unpack(x);
//     (lat as f32 * 1e-7, lon as f32 * 1e-7)
// }

pub trait Cache {
    fn get_value(&self, index: usize) -> u64;
    fn set_value(&mut self, index: usize, value: u64);

    fn set_value_f32(&mut self, index: usize, lat: f32, lon: f32) {
        self.set_value(index, pack_f32(lat, lon))
    }

    fn set_value_i32(&mut self, index: usize, lat: i32, lon: i32) {
        self.set_value(index, pack_i32(lat, lon))
    }
}
