use std::fs::{File, OpenOptions};

use anyhow::Result;
use bytesize::ByteSize;
use memmap2::MmapMut;

use crate::Cache;

pub struct DenseFileCache {
    file: File,
    mmap: MmapMut,
}

impl DenseFileCache {
    const SIZE_STEP: u64 = 1024 * 1024 * 1024;

    pub fn new(filename: &str) -> Result<Self> {
        let file = OpenOptions::new().read(true).write(true).create(true).open(filename)?;
        let mmap = Self::resize_and_mmap(1, &file)?;
        Ok(Self { mmap, file })
    }

    pub fn finish(&self) -> Result<()> {
        self.mmap.flush()?;
        self.file.sync_all()?;
        Ok(())
    }

    pub fn len(&self) -> u64 {
        self.mmap.len() as u64
    }

    fn resize_and_mmap(capacity: u64, file: &File) -> Result<MmapMut> {
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
    fn set_value(&mut self, index: u64, value: u64) -> Result<()> {
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
