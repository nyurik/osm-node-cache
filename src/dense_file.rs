#[cfg(all(feature = "nightly", test))]
extern crate test;

use std::fs::OpenOptions;
use std::mem::{size_of, transmute};
use std::sync::{Arc, Mutex, RwLock, RwLockReadGuard};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::Result;
use bytesize::ByteSize;
use memmap2::MmapMut;

use crate::Cache;

/// Increase the size of the file if needed, and create a memory map from it
fn resize_and_memmap(filename: &str, index: usize, page_size: usize, verbose: bool) -> Result<MmapMut> {
    if page_size % size_of::<usize>() != 0 {
        panic!("page_size={} is not a multiple of {}.", page_size, size_of::<usize>())
    }
    let file = OpenOptions::new().read(true).write(true).create(true).open(filename)?;
    let old_size = file.metadata().unwrap().len();

    let capacity = (index + 1) * size_of::<usize>();
    let pages = capacity / page_size + (if capacity % page_size == 0 { 0 } else { 1 });
    let new_size = (pages * page_size) as u64;
    // println!("Trying to grow {} ➡ {}", old_size, new_size);
    if old_size < new_size {
        if verbose {
            println!("Growing cache {} ➡ {} ({} pages)", ByteSize(old_size), ByteSize(new_size), pages);
        }
        file.set_len(new_size)?;
    }
    Ok(unsafe { MmapMut::map_mut(&file)? })
}

fn lock_and_link(memmap: &RwLock<MmapMut>) -> (Option<RwLockReadGuard<MmapMut>>, &[AtomicU64]) {
    let mm = memmap.read().unwrap();
    let data_as_u8: &[u8] = mm.as_ref();  // ideally this should be as_mut()
    // Major hack -- the array actually contains [u8], but AtomicU64 should work just as well
    let raw_data: &[AtomicU64] = unsafe { transmute(data_as_u8) };
    (Some(mm), raw_data)
}

#[derive(Clone)]
pub struct DenseFileCache {
    filename: Arc<String>,
    page_size: usize,
    verbose: bool,
    memmap: Arc<RwLock<MmapMut>>,
    mutex: Arc<Mutex<()>>,
}

struct CacheWriter<'a> {
    parent: &'a DenseFileCache,
    mm_setter: Option<RwLockReadGuard<'a, MmapMut>>,
    raw_data: &'a [AtomicU64],
}

impl DenseFileCache {
    /// Open or create a file for caching
    pub fn new(filename: String) -> Result<Self> {
        Self::new_ex(filename, 1024 * 1024 * 1024, true)
    }

    fn new_ex(filename: String, page_size: usize, verbose: bool) -> Result<Self> {
        let mmap = resize_and_memmap(&filename, 0, page_size, verbose)?;
        Ok(Self {
            filename: Arc::new(filename),
            page_size,
            verbose,
            memmap: Arc::new(RwLock::new(mmap)),
            mutex: Arc::new(Mutex::new(())),
        })
    }

    /// Create a thread-safe caching accessor
    pub fn get_accessor(&self) -> impl Cache + '_ {
        let (mm_setter, raw_data) = lock_and_link(&self.memmap);
        CacheWriter { parent: &self, mm_setter, raw_data }
    }
}

impl<'a> CacheWriter<'a> {
    fn len(&self) -> usize {
        // hack: len() is in bytes, not u64s
        self.raw_data.len() / size_of::<usize>()
    }
}

impl<'a> Cache for CacheWriter<'a> {
    fn get_value(&self, index: usize) -> u64 {
        if index >= self.len() {
            panic!("Index {} exceeds cache size {}", index, self.len())
        }
        self.raw_data[index].load(Ordering::Relaxed)
    }

    /// Set value at index position in the open memory map.
    /// The existence of this object implies it already holds a read lock
    /// If needed, this fn will release the read lock, get a write lock to grow the file,
    /// and re-acquire the read lock.
    /// Note that RwLock is a misnomer here:
    ///    "read" lock means we can write to memmap (OK in parallel)
    ///    "write" lock means we can destroy memmap, grow file, and re-create memmap (exclusive)
    /// It would be prohibitively expensive to acquire a read lock on each call.
    fn set_value(&mut self, index: usize, value: u64) {
        if index >= self.len() {
            // Ensure we save everything and drop the lock.
            // Growing file size can only happen inside the write lock.
            // We must get a separate mutex lock before the write lock because otherwise
            // one thread could get write lock, grow, and get the read lock, while some
            // other thread could be stuck waiting for the write lock even though the file
            // has already been grown.
            self.mm_setter = Option::None;
            {
                let _ = self.parent.mutex.lock().unwrap();
                if index >= self.len() {
                    // println!("Growing:  Index {}, Length {} ", index, self.len());
                    let p = self.parent;
                    let mut write_lock = p.memmap.write().unwrap();
                    // println!("Got write lock:  Index {}, Length {} ", index, self.len());
                    write_lock.flush().unwrap();
                    *write_lock = resize_and_memmap(&p.filename, index, p.page_size, p.verbose).unwrap();
                    // println!("Got write lock:  Index {}, Length {} ", index, self.len());
                }
            }

            let (mm_setter, raw_data) = lock_and_link(&self.parent.memmap);
            self.mm_setter = mm_setter;
            self.raw_data = raw_data;
            // println!("Got read lock:  Index {}, Length {} ", index, self.len());
        }
        self.raw_data[index].store(value, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use rand::seq::SliceRandom;
    use rand::thread_rng;
    use rayon::iter::ParallelBridge;
    use rayon::iter::ParallelIterator;

    use crate::*;

    #[test]
    fn dense_file() {
        let test_file = "./dense_file_test.dat";
        let _ = fs::remove_file(test_file);
        {
            let fc = DenseFileCache::new_ex(test_file.to_string(), 8, false).unwrap();
            let threads = 10;
            let items = 100000;
            (0_usize..threads).par_bridge()
                .for_each_with(fc.clone(),
                               |fc, _thread_id| {
                                   let mut cache = fc.get_accessor();
                                   for v in get_random_items(items) {
                                       cache.set_value(v, v as u64);
                                   }
                               },
                );
            (0_usize..threads).par_bridge()
                .for_each_with(fc,
                               |fc, _thread_id| {
                                   let cache = fc.get_accessor();
                                   for v in get_random_items(items) {
                                       assert_eq!(v as u64, cache.get_value(v))
                                   }
                               },
                );
        }
        let _ = fs::remove_file(test_file);
    }

    fn get_random_items(items: usize) -> Vec<usize> {
        let mut vec: Vec<usize> = (0_usize..items).collect();
        vec.shuffle(&mut thread_rng());
        vec
    }
}

/// The benchmarks require nightly to run:
///   $ cargo +nightly bench
#[cfg(all(feature = "nightly", test))]
mod bench {
    use std::fs;

    use crate::*;

    use super::test::Bencher;

    #[bench]
    fn dense_bench(bench: &mut Bencher) {
        let test_file = "./dense_file_perf.dat";
        let _ = fs::remove_file(test_file);
        let fc = DenseFileCache::new_ex(test_file.to_string(), 1024 * 1024, false).unwrap();

        let mut cache = fc.get_accessor();
        bench.iter(|| {
            for v in 0..1000 {
                cache.set_value(v, v as u64);
            }
        });
        let _ = fs::remove_file(test_file);
    }
}
