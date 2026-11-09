//! What the PBF reading benchmarks share: the summary every reader computes, dropping the input from the page cache,
//! and the timing loop. Each reader's crate is a package of its own (`benches/osmflow`, `benches/osmpbf`,
//! `benches/osmpbfreader`), so its dependencies are built with only the features it asks for: Cargo merges the
//! features of every package in one build, and one reader's choice of zlib backend would otherwise be every reader's.
//! `../README.md` describes the benchmark and how to run it.

#![allow(
    clippy::multiple_crate_versions,
    reason = "a benchmark, never published: criterion's duplicate dependencies cost only build time"
)]

use std::hint::black_box;
use std::path::{Path, PathBuf};
use std::time::Instant;
use std::{env, fs};

use criterion::{BatchSize, Criterion};

/// What every reader computes: enough to show it read every object completely. Every tag key and value and every
/// member role is taken as a string slice, passed through `black_box` and its length summed, so each crate does the
/// work of handing out its strings (whatever its API does to give them) and the benchmark copies none.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Summary {
    nodes: u64,
    ways: u64,
    relations: u64,
    tags: u64,
    refs: u64,
    members: u64,
    string_bytes: u64,
    id_sum: i64,
    ref_sum: i64,
    lat_sum: i64,
    lon_sum: i64,
}

impl Summary {
    /// Counts a node and adds its ID and location to the checksums.
    pub fn node(&mut self, id: i64, lat: i32, lon: i32) {
        self.nodes += 1;
        self.id_sum = self.id_sum.wrapping_add(id);
        self.lat_sum = self.lat_sum.wrapping_add(i64::from(lat));
        self.lon_sum = self.lon_sum.wrapping_add(i64::from(lon));
    }

    /// Counts a way and its node refs, and adds their IDs to the checksums.
    pub fn way(&mut self, id: i64, refs: impl Iterator<Item = i64>) {
        self.ways += 1;
        self.id_sum = self.id_sum.wrapping_add(id);
        for r in refs {
            self.refs += 1;
            self.ref_sum = self.ref_sum.wrapping_add(r);
        }
    }

    /// Counts a relation and adds its ID to the checksum. Its members are counted by [`Self::member`].
    pub fn relation(&mut self, id: i64) {
        self.relations += 1;
        self.id_sum = self.id_sum.wrapping_add(id);
    }

    /// Counts tags and their bytes, passing each through `black_box` so a reader cannot skip decoding them.
    pub fn tags<K: AsRef<str>, V: AsRef<str>>(&mut self, tags: impl Iterator<Item = (K, V)>) {
        for (k, v) in tags {
            let (k, v) = black_box((k.as_ref(), v.as_ref()));
            self.tags += 1;
            self.string_bytes += (k.len() + v.len()) as u64;
        }
    }

    /// Counts a relation member and the bytes of its role.
    pub fn member(&mut self, role: &str) {
        self.members += 1;
        self.string_bytes += black_box(role).len() as u64;
    }

    /// Adds up the summaries of two parts of a file, as parallel readers produce them.
    #[must_use]
    pub fn merge(mut self, o: Self) -> Self {
        self.nodes += o.nodes;
        self.ways += o.ways;
        self.relations += o.relations;
        self.tags += o.tags;
        self.refs += o.refs;
        self.members += o.members;
        self.string_bytes += o.string_bytes;
        self.id_sum = self.id_sum.wrapping_add(o.id_sum);
        self.ref_sum = self.ref_sum.wrapping_add(o.ref_sum);
        self.lat_sum = self.lat_sum.wrapping_add(o.lat_sum);
        self.lon_sum = self.lon_sum.wrapping_add(o.lon_sum);
        self
    }
}

/// Drops the file from the OS page cache and checks that at most 1% of it is still cached.
#[cfg(target_os = "linux")]
#[allow(
    unsafe_code,
    reason = "sysconf, posix_fadvise and mincore have no safe wrapper in std"
)]
fn evict(path: &Path) {
    use std::fs::File;
    use std::os::fd::AsRawFd as _;
    use std::{io, ptr};

    // SAFETY: `sysconf` only reads a system setting.
    let page = usize::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) })
        .ok()
        .filter(|&page| page > 0)
        .expect("sysconf gives the page size");
    let file = File::open(path).unwrap();
    let len = usize::try_from(file.metadata().unwrap().len()).unwrap();
    if len == 0 {
        return;
    }
    let fd = file.as_raw_fd();
    // SAFETY: `fd` is an open file for the whole call.
    let rc = unsafe { libc::posix_fadvise(fd, 0, 0, libc::POSIX_FADV_DONTNEED) };
    assert_eq!(rc, 0, "posix_fadvise failed with {rc}");

    // Map the file without touching it, and ask which of its pages are in memory.
    // SAFETY: a fresh read-only shared mapping of an open file, unmapped below and never read.
    let addr = unsafe {
        libc::mmap(
            ptr::null_mut(),
            len,
            libc::PROT_READ,
            libc::MAP_SHARED,
            fd,
            0,
        )
    };
    assert_ne!(
        addr,
        libc::MAP_FAILED,
        "mmap failed: {}",
        io::Error::last_os_error()
    );
    // mincore gives one byte per page of the system's page size
    let mut pages = vec![0u8; len.div_ceil(page)];
    // SAFETY: `addr` maps `len` bytes, and `pages` has one byte per page of them.
    let rc = unsafe { libc::mincore(addr, len, pages.as_mut_ptr()) };
    let err = io::Error::last_os_error();
    // SAFETY: `addr` and `len` are the mapping made above.
    unsafe { libc::munmap(addr, len) };
    assert_eq!(rc, 0, "mincore failed: {err}");
    let cached = pages.iter().filter(|&&p| p & 1 != 0).count();
    assert!(
        cached * 100 <= pages.len(),
        "{} still has {cached} of {} pages in the page cache",
        path.display(),
        pages.len()
    );
}

#[cfg(not(target_os = "linux"))]
#[expect(
    clippy::print_stderr,
    reason = "a warning that the timings include cached reads"
)]
fn evict(path: &Path) {
    use std::sync::Once;

    static WARNED: Once = Once::new();
    WARNED.call_once(|| {
        eprintln!(
            "warning: {} stays in the page cache on this system",
            path.display()
        );
    });
}

/// A reader: reads the whole file, and returns its summary.
pub type Reader = fn(&Path) -> Summary;

/// The file to read: `OSMFLOW_BENCH_PBF`, or osmflow's Monaco fixture.
fn input() -> PathBuf {
    env::var_os("OSMFLOW_BENCH_PBF").map_or_else(
        || {
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../crates/osmflow/fixtures/planetiler/monaco-latest.osm.pbf")
        },
        PathBuf::from,
    )
}

/// Times `readers` on the input file, and checks they all read the same data. Each prints its time, then the file's
/// size and summary; `report.py` checks the summaries of separate runs against each other. Only the readers whose
/// name contains one of the comma-separated `OSMFLOW_BENCH_READERS` run, or all of them.
///
/// # Panics
/// If the readers' summaries differ, or the file cannot be read or dropped from the page cache.
#[expect(
    clippy::print_stderr,
    reason = "the times and summaries are the output"
)]
pub fn run(readers: &[(&'static str, Reader)]) {
    let filter = env::var("OSMFLOW_BENCH_READERS").unwrap_or_default();
    let readers: Vec<_> = readers
        .iter()
        .filter(|(name, _)| filter.is_empty() || filter.split(',').any(|f| name.contains(f)))
        .collect();
    if readers.is_empty() {
        return;
    }
    let path = input();
    // One timed pass per reader, which also checks they all read the same data. With `OSMFLOW_BENCH_ONCE`, this is
    // the whole benchmark: criterion's ten samples per reader take too long on large files.
    let mut expected = None;
    for (name, read) in &readers {
        evict(&path);
        let start = Instant::now();
        let got = read(&path);
        eprintln!("{name}: {:.3} s", start.elapsed().as_secs_f64());
        let expected = expected.get_or_insert(got);
        assert_eq!(
            got, *expected,
            "{name} read different data than {}",
            readers[0].0
        );
    }
    let size = fs::metadata(&path).unwrap().len();
    eprintln!(
        "{}: {size} bytes, {:?}",
        path.display(),
        expected.unwrap_or_default()
    );
    if env::var_os("OSMFLOW_BENCH_ONCE").is_some_and(|v| !v.is_empty()) {
        return;
    }

    let mut c = Criterion::default().configure_from_args();
    let mut group = c.benchmark_group("read");
    group.sample_size(10);
    for (name, read) in readers {
        group.bench_function(*name, |b| {
            b.iter_batched(
                || evict(&path),
                |()| black_box(read(black_box(&path))),
                BatchSize::PerIteration,
            );
        });
    }
    group.finish();
    c.final_summary();
}
