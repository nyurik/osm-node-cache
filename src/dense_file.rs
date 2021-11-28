#[cfg(all(feature = "nightly", test))]
extern crate test;

use std::fs::OpenOptions;
use std::mem::{size_of, transmute};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock, RwLockReadGuard};

use anyhow::Result;
use bytesize::ByteSize;
use memmap2::MmapMut;

use crate::Cache;

#[derive(Clone, Copy, Debug)]
pub struct DenseFileCacheOptions {
    write: bool,
    autogrow: bool,
    init_size: usize,
    page_size: usize,
    verbose: bool,
}

impl Default for DenseFileCacheOptions {
    fn default() -> Self {
        Self::new()
    }
}

impl DenseFileCacheOptions {
    pub fn new() -> Self {
        DenseFileCacheOptions {
            write: true,
            autogrow: true,
            init_size: 1024 * 1024 * 1024, // 1 GB
            page_size: 1024 * 1024 * 1024, // 1 GB
            verbose: true,
        }
    }

    /// Allow data modification
    pub fn write(&mut self, write: bool) -> &mut Self {
        if !write {
            todo!("Readonly cache is not supported yet")
        }
        self.write = write;
        self
    }

    /// Print cache file notifications.
    pub fn verbose(&mut self, verbose: bool) -> &mut Self {
        self.verbose = verbose;
        self
    }

    /// Automatically increase cache file size as needed. Ignored for read-only files.
    pub fn autogrow(&mut self, autogrow: bool) -> &mut Self {
        if !autogrow {
            todo!("Constant cache size is not supported yet")
        }
        self.autogrow = autogrow;
        self
    }

    /// Ensure cache file is at least this big. Ignored for read-only files.
    pub fn init_size(&mut self, init_size: usize) -> &mut Self {
        self.init_size = init_size;
        self
    }

    /// When increasing file size, grow it in page size increments. Ignored for read-only files.
    pub fn page_size(&mut self, page_size: usize) -> &mut Self {
        self.page_size = page_size;
        self
    }

    /// Open and initialize cache file.
    pub fn open(self, filename: String) -> Result<DenseFileCache> {
        DenseFileCache::new_opt(filename, self)
    }
}

/// Increase the size of the file if needed, and create a memory map from it
fn resize_and_memmap(
    filename: &str,
    index: usize,
    opts: &DenseFileCacheOptions,
) -> Result<MmapMut> {
    if opts.page_size % size_of::<usize>() != 0 {
        panic!(
            "page_size={} is not a multiple of {}.",
            opts.page_size,
            size_of::<usize>()
        )
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(filename)?;
    let old_size = file.metadata().unwrap().len();

    let capacity = (index + 1) * size_of::<usize>();
    let pages = capacity / opts.page_size + (if capacity % opts.page_size == 0 { 0 } else { 1 });
    let new_size = (pages * opts.page_size) as u64;
    if old_size < new_size {
        if opts.verbose {
            println!(
                "Growing cache {} ➡ {} ({} pages)",
                ByteSize(old_size),
                ByteSize(new_size),
                pages
            );
        }
        file.set_len(new_size)?;
    }
    Ok(unsafe { MmapMut::map_mut(&file)? })
}

fn lock_and_link(memmap: &RwLock<MmapMut>) -> (Option<RwLockReadGuard<MmapMut>>, &[AtomicU64]) {
    let mm = memmap.read().unwrap();
    // ideally this should be as_mut(), but mut is not multithreaded
    let data_as_u8: &[u8] = mm.as_ref();
    // Major hack -- the array actually contains [u8], but AtomicU64 appear to work and simplify things
    let raw_data: &[AtomicU64] = unsafe { transmute(data_as_u8) };

    (Some(mm), raw_data)
}

#[derive(Clone)]
pub struct DenseFileCache {
    filename: Arc<String>,
    options: DenseFileCacheOptions,
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
        DenseFileCacheOptions::new().open(filename)
    }

    fn new_opt(filename: String, options: DenseFileCacheOptions) -> Result<Self> {
        let mmap = resize_and_memmap(&filename, 0, &options)?;
        Ok(Self {
            filename: Arc::new(filename),
            options,
            memmap: Arc::new(RwLock::new(mmap)),
            mutex: Arc::new(Mutex::new(())),
        })
    }

    /// Create a thread-safe caching accessor
    pub fn get_accessor(&self) -> impl Cache + '_ {
        let (mm_setter, raw_data) = lock_and_link(&self.memmap);
        CacheWriter {
            parent: self,
            mm_setter,
            raw_data,
        }
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
                let _pre_write_lock = self.parent.mutex.lock().unwrap();
                if index >= self.len() {
                    let p = self.parent;
                    let mut write_lock = p.memmap.write().unwrap();
                    write_lock.flush().unwrap();
                    *write_lock = resize_and_memmap(&p.filename, index, &p.options).unwrap();
                }
            }

            let (mm_setter, raw_data) = lock_and_link(&self.parent.memmap);
            self.mm_setter = mm_setter;
            self.raw_data = raw_data;
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
            let fc = DenseFileCacheOptions::new()
                .page_size(8)
                .verbose(false)
                .open(test_file.to_string())
                .unwrap();
            let threads = 10;
            let items = 100000;
            (0_usize..threads)
                .par_bridge()
                .for_each_with(fc.clone(), |fc, _thread_id| {
                    let mut cache = fc.get_accessor();
                    for v in get_random_items(items) {
                        cache.set_value(v, v as u64);
                    }
                });
            (0_usize..threads)
                .par_bridge()
                .for_each_with(fc, |fc, _thread_id| {
                    let cache = fc.get_accessor();
                    for v in get_random_items(items) {
                        assert_eq!(v as u64, cache.get_value(v))
                    }
                });
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
        let fc = DenseFileCacheOptions::new()
            .page_size(1024 * 1024)
            .verbose(false)
            .open(test_file.to_string())
            .unwrap();

        let mut cache = fc.get_accessor();
        bench.iter(|| {
            for v in 0..1000 {
                cache.set_value(v, v as u64);
            }
        });
        let _ = fs::remove_file(test_file);
    }
}
