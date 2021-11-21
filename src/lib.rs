use std::fs::{File, OpenOptions};
use std::ops::Drop;

use bytesize::ByteSize;
use memmap2::MmapMut;

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
    fn set_value_f32(&mut self, index: u64, lat: f32, lon: f32) -> anyhow::Result<()> {
        self.set_value(index, pack_f32(lat, lon))
    }

    fn set_value_i32(&mut self, index: u64, lat: i32, lon: i32) -> anyhow::Result<()> {
        self.set_value(index, pack_i32(lat, lon))
    }

    fn set_value(&mut self, index: u64, value: u64) -> anyhow::Result<()>;
}

pub struct DenseFileCache {
    file: File,
    mmap: MmapMut,
}

impl DenseFileCache {
    const SIZE_STEP: u64 = 1024 * 1024 * 1024;

    pub fn new(filename: &str) -> anyhow::Result<Self> {
        let file = OpenOptions::new().read(true).write(true).create(true).open(filename)?;
        let mmap = Self::resize_and_mmap(1, &file)?;
        Ok(Self { mmap, file })
    }

    pub fn finish(&self) -> anyhow::Result<()> {
        self.mmap.flush()?;
        self.file.sync_all()?;
        Ok(())
    }

    pub fn len(&self) -> u64 {
        self.mmap.len() as u64
    }

    fn resize_and_mmap(capacity: u64, file: &File) -> anyhow::Result<MmapMut> {
        let new_size = (capacity / Self::SIZE_STEP + 1) * Self::SIZE_STEP;
        let current_size = file.metadata().unwrap().len();
        if current_size < new_size {
            println!("New cache size: {} - {} pages", ByteSize(new_size), capacity / Self::SIZE_STEP + 1);
            file.sync_all()?; // Uncertain if needed
            file.set_len(new_size)?;
        }
        Ok(unsafe { MmapMut::map_mut(file)? })
    }
}

impl Cache for DenseFileCache {
    fn set_value(&mut self, index: u64, value: u64) -> anyhow::Result<()> {
        let index_start = (index * 8) as usize;
        let index_end = index_start + 8;
        if index_end >= self.mmap.len() {
            self.finish()?;
            self.mmap = Self::resize_and_mmap(index_end as u64, &self.file)?;
        }
        // TODO: decide which to use -- "be", "ne", or "le" ?
        self.mmap[index_start..index_end].copy_from_slice(&value.to_be_bytes());
        Ok(())
    }
}

impl Drop for DenseFileCache {
    // TODO: Not sure if Drop is needed, or if it is already handled
    fn drop(&mut self) {
        self.finish().unwrap();
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn it_works() {
        assert_eq!(2 + 2, 4);
    }
}
