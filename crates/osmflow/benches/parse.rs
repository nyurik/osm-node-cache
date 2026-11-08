//! Reads a whole PBF file with osmflow and with other crates, and compares their speed.
//!
//! Every reader computes the same [`Summary`], and the benchmark refuses to run unless all summaries are equal, so every
//! crate is measured doing the same work. Every run starts with the file out of the OS page cache. `README.md` in this
//! directory describes the readers and the environment variables, and how to run it.

use std::hint::black_box;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use criterion::{BatchSize, Criterion, criterion_group, criterion_main};

/// What every reader computes: enough to show it read every object completely. Every tag key and value and every
/// member role is taken as a string slice, passed through `black_box` and its length summed, so each crate does the
/// work of handing out its strings (whatever its API does to give them) and the benchmark copies none.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Summary {
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
    fn node(&mut self, id: i64, lat: i32, lon: i32) {
        self.nodes += 1;
        self.id_sum = self.id_sum.wrapping_add(id);
        self.lat_sum = self.lat_sum.wrapping_add(i64::from(lat));
        self.lon_sum = self.lon_sum.wrapping_add(i64::from(lon));
    }

    fn way(&mut self, id: i64, refs: impl Iterator<Item = i64>) {
        self.ways += 1;
        self.id_sum = self.id_sum.wrapping_add(id);
        for r in refs {
            self.refs += 1;
            self.ref_sum = self.ref_sum.wrapping_add(r);
        }
    }

    fn relation(&mut self, id: i64) {
        self.relations += 1;
        self.id_sum = self.id_sum.wrapping_add(id);
    }

    fn tags<K: AsRef<str>, V: AsRef<str>>(&mut self, tags: impl Iterator<Item = (K, V)>) {
        for (k, v) in tags {
            let (k, v) = black_box((k.as_ref(), v.as_ref()));
            self.tags += 1;
            self.string_bytes += (k.len() + v.len()) as u64;
        }
    }

    fn member(&mut self, role: &str) {
        self.members += 1;
        self.string_bytes += black_box(role).len() as u64;
    }

    fn merge(mut self, o: Self) -> Self {
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

fn osmflow_block(block: &osmflow::Block) -> Summary {
    let mut s = Summary::default();
    for n in block.nodes() {
        s.node(n.id(), n.decimicro_lat(), n.decimicro_lon());
        s.tags(n.tags());
    }
    for w in block.ways() {
        s.way(w.id(), w.refs().iter().copied());
        s.tags(w.tags());
    }
    for r in block.relations() {
        s.relation(r.id());
        s.tags(r.tags());
        r.members().for_each(|m| s.member(m.role));
    }
    s
}

fn osmflow_seq(path: &Path) -> Summary {
    let file = osmflow::PbfReader::open(path).unwrap();
    let blocks = file.blocks(osmflow::Select::all()).unwrap();
    blocks.fold(Summary::default(), |s, b| {
        s.merge(osmflow_block(&b.unwrap()))
    })
}

fn osmflow_ordered(path: &Path) -> Summary {
    let file = osmflow::PbfReader::open(path).unwrap();
    let blocks = file.par_ordered(osmflow::Select::all()).unwrap();
    blocks.fold(Summary::default(), |s, b| {
        s.merge(osmflow_block(&b.unwrap()))
    })
}

fn osmflow_unordered(path: &Path) -> Summary {
    let file = osmflow::PbfReader::open(path).unwrap();
    let total = Mutex::new(Summary::default());
    file.par_unordered(osmflow::Select::all(), |block| {
        let s = osmflow_block(&block);
        let mut total = total.lock().unwrap();
        *total = total.merge(s);
        Ok(())
    })
    .unwrap();
    total.into_inner().unwrap()
}

fn osmpbf_element(e: &osmpbf::Element<'_>) -> Summary {
    use osmpbf::Element;
    let mut s = Summary::default();
    match e {
        Element::Node(n) => {
            s.node(n.id(), n.decimicro_lat(), n.decimicro_lon());
            s.tags(n.tags());
        }
        Element::DenseNode(n) => {
            s.node(n.id(), n.decimicro_lat(), n.decimicro_lon());
            s.tags(n.tags());
        }
        Element::Way(w) => {
            s.way(w.id(), w.refs());
            s.tags(w.tags());
        }
        Element::Relation(r) => {
            s.relation(r.id());
            s.tags(r.tags());
            r.members().for_each(|m| s.member(m.role().unwrap()));
        }
    }
    s
}

fn osmpbf_seq(path: &Path) -> Summary {
    let mut total = Summary::default();
    osmpbf::ElementReader::from_path(path)
        .unwrap()
        .for_each(|e| total = total.merge(osmpbf_element(&e)))
        .unwrap();
    total
}

fn osmpbf_par(path: &Path) -> Summary {
    osmpbf::ElementReader::from_path(path)
        .unwrap()
        .par_map_reduce(|e| osmpbf_element(&e), Summary::default, Summary::merge)
        .unwrap()
}

fn osmpbfreader_obj(obj: &osmpbfreader::OsmObj) -> Summary {
    use osmpbfreader::OsmObj;
    let mut s = Summary::default();
    match obj {
        OsmObj::Node(n) => {
            s.node(n.id.0, n.decimicro_lat, n.decimicro_lon);
            s.tags(n.tags.iter());
        }
        OsmObj::Way(w) => {
            s.way(w.id.0, w.nodes.iter().map(|n| n.0));
            s.tags(w.tags.iter());
        }
        OsmObj::Relation(r) => {
            s.relation(r.id.0);
            s.tags(r.tags.iter());
            r.refs.iter().for_each(|m| s.member(&m.role));
        }
    }
    s
}

fn osmpbfreader_seq(path: &Path) -> Summary {
    let mut reader = osmpbfreader::OsmPbfReader::new(std::fs::File::open(path).unwrap());
    reader.iter().fold(Summary::default(), |s, o| {
        s.merge(osmpbfreader_obj(&o.unwrap()))
    })
}

fn osmpbfreader_par(path: &Path) -> Summary {
    let mut reader = osmpbfreader::OsmPbfReader::new(std::fs::File::open(path).unwrap());
    reader.par_iter().fold(Summary::default(), |s, o| {
        s.merge(osmpbfreader_obj(&o.unwrap()))
    })
}

/// Drops the file from the OS page cache and checks that at most 1% of it is still cached.
#[cfg(target_os = "linux")]
#[allow(
    unsafe_code,
    reason = "posix_fadvise and mincore have no safe wrapper in std"
)]
fn evict(path: &Path) {
    use std::os::fd::AsRawFd as _;

    const PAGE: usize = 4096;
    let file = std::fs::File::open(path).unwrap();
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
            std::ptr::null_mut(),
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
        std::io::Error::last_os_error()
    );
    let mut pages = vec![0u8; len.div_ceil(PAGE)];
    // SAFETY: `addr` maps `len` bytes, and `pages` has one byte per page of them.
    let rc = unsafe { libc::mincore(addr, len, pages.as_mut_ptr()) };
    let err = std::io::Error::last_os_error();
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
fn evict(path: &Path) {
    static WARNED: std::sync::Once = std::sync::Once::new();
    WARNED.call_once(|| {
        eprintln!(
            "warning: {} stays in the page cache on this system",
            path.display()
        )
    });
}

type Reader = fn(&Path) -> Summary;

const READERS: [(&str, Reader); 7] = [
    ("osmflow/one thread", osmflow_seq),
    ("osmflow/parallel ordered", osmflow_ordered),
    ("osmflow/parallel unordered", osmflow_unordered),
    ("osmpbf/one thread", osmpbf_seq),
    ("osmpbf/par_map_reduce", osmpbf_par),
    ("osmpbfreader/one thread", osmpbfreader_seq),
    ("osmpbfreader/par_iter", osmpbfreader_par),
];

fn input() -> PathBuf {
    std::env::var_os("OSMFLOW_BENCH_PBF").map_or_else(
        || Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/planetiler/monaco-latest.osm.pbf"),
        PathBuf::from,
    )
}

/// The readers to run: those whose name contains any of the comma-separated `OSMFLOW_BENCH_READERS`, or all.
fn readers() -> Vec<(&'static str, Reader)> {
    let filter = std::env::var("OSMFLOW_BENCH_READERS").unwrap_or_default();
    READERS
        .into_iter()
        .filter(|(name, _)| filter.is_empty() || filter.split(',').any(|f| name.contains(f)))
        .collect()
}

fn bench(c: &mut Criterion) {
    let path = input();
    let readers = readers();
    // One timed pass per reader, which also checks they all read the same data. With `OSMFLOW_BENCH_ONCE`, this is
    // the whole benchmark: criterion's ten samples per reader take too long on large files.
    let mut expected = None;
    for (name, read) in &readers {
        evict(&path);
        let start = std::time::Instant::now();
        let got = read(&path);
        eprintln!("{name}: {:.3} s", start.elapsed().as_secs_f64());
        let expected = expected.get_or_insert(got);
        assert_eq!(
            got, *expected,
            "{name} read different data than {}",
            readers[0].0
        );
    }
    let size = std::fs::metadata(&path).unwrap().len();
    eprintln!(
        "{}: {size} bytes, {:?}",
        path.display(),
        expected.unwrap_or_default()
    );
    if std::env::var_os("OSMFLOW_BENCH_ONCE").is_some_and(|v| !v.is_empty()) {
        return;
    }

    let mut group = c.benchmark_group("read");
    group.sample_size(10);
    for (name, read) in readers {
        group.bench_function(name, |b| {
            b.iter_batched(
                || evict(&path),
                |()| black_box(read(black_box(&path))),
                BatchSize::PerIteration,
            );
        });
    }
    group.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
