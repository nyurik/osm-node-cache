use std::mem::{size_of, transmute};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock, RwLockReadGuard};

#[cfg(unix)]
pub use memmap2::Advice;
use memmap2::MmapMut;

use crate::traits::{open_cache_file, Cache, CacheStore};
use crate::{OsmNodeCacheError, OsmNodeCacheResult};

pub type OnSizeChange = fn(old_size: usize, new_size: usize) -> ();

#[derive(Clone)]
pub struct DenseFileCacheOpts {
    filename: Arc<PathBuf>,
    write: bool,
    autogrow: bool,
    init_size: usize,
    page_size: usize,
    #[cfg(unix)]
    advice: Advice,
    on_size_change: Option<OnSizeChange>,
}

impl DenseFileCacheOpts {
    #[must_use]
    pub fn new(filename: PathBuf) -> Self {
        DenseFileCacheOpts {
            filename: Arc::new(filename),
            write: true,
            autogrow: true,
            init_size: 1024 * 1024 * 1024, // 1 GB
            page_size: 1024 * 1024 * 1024, // 1 GB
            on_size_change: None,
            #[cfg(unix)]
            advice: Advice::Normal,
        }
    }

    /// Allow data modification
    #[must_use]
    pub fn write(mut self, write: bool) -> Self {
        if !write {
            todo!("Readonly cache is not supported yet")
        }
        self.write = write;
        self
    }

    /// Set callback to report when cache size changes
    #[must_use]
    pub fn on_size_change(mut self, on_size_change: Option<OnSizeChange>) -> Self {
        self.on_size_change = on_size_change;
        self
    }

    /// Automatically increase cache file size as needed. Ignored for read-only files.
    #[must_use]
    pub fn autogrow(mut self, autogrow: bool) -> Self {
        if !autogrow {
            todo!("Constant cache size is not supported yet")
        }
        self.autogrow = autogrow;
        self
    }

    /// Ensure cache file is at least this big. Ignored for read-only files.
    #[must_use]
    pub fn init_size(mut self, init_size: usize) -> Self {
        self.init_size = init_size;
        self
    }

    /// When increasing file size, grow it in page size increments. Ignored for read-only files.
    #[must_use]
    pub fn page_size(mut self, page_size: usize) -> Self {
        self.page_size = page_size;
        self
    }

    #[must_use]
    pub fn advise(mut self, advice: Advice) -> Self {
        self.advice = advice;
        self
    }

    /// Open and initialize cache file.
    pub fn open(self) -> OsmNodeCacheResult<DenseFileCache> {
        DenseFileCache::new_opt(self)
    }
}

/// Increase the size of the file if needed, and create a memory map from it
fn resize_and_memmap(index: usize, opts: &DenseFileCacheOpts) -> OsmNodeCacheResult<MmapMut> {
    if opts.page_size % size_of::<usize>() != 0 {
        return Err(OsmNodeCacheError::InvalidPageSize {
            page_size: opts.page_size,
            element_size: size_of::<usize>(),
        });
    }

    let file = open_cache_file(opts.filename.as_ref())?;
    let old_size = file.metadata().unwrap().len();

    let capacity = (index + 1) * size_of::<usize>();
    let pages = capacity / opts.page_size + usize::from(capacity % opts.page_size != 0);
    let new_size = (pages * opts.page_size) as u64;
    if old_size < new_size {
        if let Some(value) = opts.on_size_change {
            value(to_64_usize(old_size), to_64_usize(new_size));
        }
        file.set_len(new_size)?;
    }
    Ok(unsafe { MmapMut::map_mut(&file)? })
}

fn to_64_usize(old_size: u64) -> usize {
    usize::try_from(old_size).expect("Unable to convert large u64 to usize on this platform")
}

fn lock_and_link(memmap: &RwLock<MmapMut>) -> (Option<RwLockReadGuard<'_, MmapMut>>, &[AtomicU64]) {
    let mm = memmap.read().unwrap();
    // ideally this should be as_mut(), but mut is not multithreaded
    let data_as_u8: &[u8] = mm.as_ref();
    let raw_data;
    #[allow(clippy::transmute_ptr_to_ptr)]
    {
        // Major hack -- the array actually contains [u8], but AtomicU64 appear to work and simplify things
        raw_data = unsafe { transmute::<&[u8], &[AtomicU64]>(data_as_u8) };
    }

    (Some(mm), raw_data)
}

#[derive(Clone)]
pub struct DenseFileCache {
    opts: DenseFileCacheOpts,
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
    pub fn new(filename: PathBuf) -> OsmNodeCacheResult<Self> {
        DenseFileCacheOpts::new(filename).open()
    }

    /// Set memory advice for the cache file
    ///
    /// # Panics
    /// This call will panic if the file lock has been poisoned.
    #[cfg(unix)]
    pub fn advise(&self, advice: Advice) -> OsmNodeCacheResult<()> {
        self.memmap.read().unwrap().advise(advice)?;
        Ok(())
    }

    fn new_opt(opts: DenseFileCacheOpts) -> OsmNodeCacheResult<Self> {
        let mmap = resize_and_memmap(0, &opts)?;
        let cache = Self {
            opts,
            memmap: Arc::new(RwLock::new(mmap)),
            mutex: Arc::new(Mutex::new(())),
        };
        #[cfg(unix)]
        if cache.opts.advice != Advice::Normal {
            cache.advise(cache.opts.advice)?;
        }
        Ok(cache)
    }
}

impl CacheStore for DenseFileCache {
    fn get_accessor(&self) -> Box<dyn Cache + '_> {
        let (mm_setter, raw_data) = lock_and_link(&self.memmap);
        Box::new(CacheWriter {
            parent: self,
            mm_setter,
            raw_data,
        })
    }
}

impl CacheWriter<'_> {
    fn len(&self) -> usize {
        // hack: len() is in bytes, not u64s
        self.raw_data.len() / size_of::<usize>()
    }
}

impl Cache for CacheWriter<'_> {
    fn get(&self, index: usize) -> u64 {
        assert!(
            index < self.len(),
            "Index {index} exceeds cache size {}",
            self.len()
        );
        self.raw_data[index].load(Ordering::Relaxed)
    }

    /// Set value at index position in the open memory map.
    /// The existence of this object implies it already holds a read lock
    /// If needed, this fn will release the read lock, get a write lock to grow the file,
    /// and re-acquire the read lock.
    /// Note that `RwLock` is a misnomer here:
    ///    "read" lock means we can write to memmap (OK in parallel)
    ///    "write" lock means we can destroy memmap, grow file, and re-create memmap (exclusive)
    /// It would be prohibitively expensive to acquire a read lock on each call.
    fn set(&mut self, index: usize, value: u64) {
        if index >= self.len() {
            // Ensure we save everything and drop the lock.
            // Growing file size can only happen inside the write lock.
            // We must get a separate mutex lock before the write lock because otherwise
            // one thread could get write lock, grow, and get the read lock, while some
            // other thread could be stuck waiting for the write lock even though the file
            // has already been grown.
            self.mm_setter = None;
            {
                let _pre_write_lock = self.parent.mutex.lock().unwrap();
                if index >= self.len() {
                    let p = self.parent;
                    let mut write_lock = p.memmap.write().unwrap();
                    write_lock.flush().unwrap();
                    *write_lock = resize_and_memmap(index, &p.opts).unwrap();
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
    use std::path::PathBuf;

    use rayon::iter::{ParallelBridge, ParallelIterator};

    use crate::traits::tests::get_random_items;
    use crate::*;

    #[test]
    fn dense_file() {
        let test_file = "./dense_file_test.dat";
        let _ = fs::remove_file(test_file);
        {
            let fc = DenseFileCacheOpts::new(PathBuf::from(test_file))
                .page_size(8)
                .open()
                .unwrap();
            let threads = 10;
            let items = 100_000;
            (0_usize..threads)
                .par_bridge()
                .for_each_with(fc.clone(), |fc, _thread_id| {
                    let mut cache = fc.get_accessor();
                    for v in get_random_items(items) {
                        cache.set(v, v as u64);
                    }
                });
            (0_usize..threads)
                .par_bridge()
                .for_each_with(fc, |fc, _thread_id| {
                    let cache = fc.get_accessor();
                    for v in get_random_items(items) {
                        assert_eq!(v as u64, cache.get(v));
                    }
                });
        }
        let _ = fs::remove_file(test_file);
    }
}
