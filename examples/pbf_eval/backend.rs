//! The commands that build, check and time a node store, for each kind of store.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use osmnodecache::{
    BulkLoad as _, FileStore, Location, NodeRead, NodeStore, PagedOpts, PagedStats, PagedStore,
};

use crate::pbf::for_each_node;
use crate::store;

/// The metadata key of where the PBF file's ways start, and of the file's size
const WAYS_AT: &str = "pbf_ways_offset";
const PBF_BYTES: &str = "pbf_bytes";
/// Whether to leave out the statistics of a paged store
pub static NO_STATS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// Nodes a bulk load is given at once
const BATCH: usize = 1 << 20;

/// How the commands get a store of each kind.
pub trait Backend: NodeStore {
    /// The store to read only
    type Read: NodeRead;

    /// An empty store at `path`, replacing any there, filling pages to `fill` where the store has
    /// pages to fill
    fn create(path: &Path, fill: f64) -> anyhow::Result<Self>;

    /// The store at `path`, to change; a memory store is loaded from the PBF file again.
    fn open(pbf: &Path, path: &Path) -> anyhow::Result<Self>;

    /// The store at `path`, to read only; a memory store is loaded from the PBF file again.
    fn open_read(pbf: &Path, path: &Path) -> anyhow::Result<Self::Read>;

    /// Prints what the store holds, besides the size of its files.
    fn report(&self, _label: &str) {}

    /// Prints the bytes the store's files take on disk, given its node count if known.
    fn report_files(label: &str, path: &Path, nodes: Option<u64>) {
        report_disk(label, path, nodes);
    }
}

/// A store kept in a file.
pub trait FileBackend: FileStore {
    /// The options, filling pages to `fill` where the store has pages to fill
    fn opts(fill: f64) -> Self::Opts;

    /// Prints what the store holds, besides the size of its files.
    fn report(&self, _label: &str) {}
}

impl<S: FileBackend> Backend for S {
    type Read = S::ReadOnly;

    fn create(path: &Path, fill: f64) -> anyhow::Result<Self> {
        for f in files(path) {
            let _ = std::fs::remove_file(f);
        }
        Ok(S::open_with(path, &S::opts(fill))?)
    }

    fn open(_pbf: &Path, path: &Path) -> anyhow::Result<Self> {
        Ok(S::open_with(path, &S::opts(0.97))?)
    }

    fn open_read(_pbf: &Path, path: &Path) -> anyhow::Result<S::ReadOnly> {
        let start = Instant::now();
        let store = S::open_read_only_with(path, &S::opts(0.97))?;
        println!("opened in {:.2} s", start.elapsed().as_secs_f64());
        Ok(store)
    }

    fn report(&self, label: &str) {
        FileBackend::report(self, label);
    }
}

impl FileBackend for PagedStore {
    fn opts(fill: f64) -> PagedOpts {
        PagedOpts::default().fill(fill)
    }

    /// The statistics read every page; `--no-stats` leaves them out, for a store too big for
    /// memory along with the rest
    fn report(&self, label: &str) {
        if !NO_STATS.load(std::sync::atomic::Ordering::Relaxed) {
            report_paged(label, &self.stats());
        }
    }
}

#[cfg(feature = "redb")]
impl FileBackend for osmnodecache::RedbStore {
    /// A cache of 16 GiB, as a host with the store in memory would give it, and the blocks of
    /// 2^`ONC_GROUP_SHIFT` per value if that is set
    fn opts(_fill: f64) -> Self::Opts {
        let opts = osmnodecache::KvOpts::default()
            .backend(osmnodecache::RedbOpts::default().cache_bytes(16 << 30));
        match std::env::var("ONC_GROUP_SHIFT")
            .ok()
            .and_then(|v| v.parse().ok())
        {
            Some(shift) => opts.group_shift(shift),
            None => opts,
        }
    }
}

#[cfg(feature = "dense")]
impl FileBackend for osmnodecache::DenseStore {
    fn opts(_fill: f64) -> Self::Opts {
        osmnodecache::DenseOpts::default()
    }
}

/// A memory store has no file, so each command loads it from the PBF file.
#[cfg(feature = "memory")]
impl Backend for osmnodecache::MemoryStore {
    type Read = Self;

    fn create(_path: &Path, _fill: f64) -> anyhow::Result<Self> {
        Ok(Self::new())
    }

    fn open(pbf: &Path, _path: &Path) -> anyhow::Result<Self> {
        let start = Instant::now();
        let mut store = Self::new();
        load(&mut store, pbf)?;
        println!(
            "loaded from the PBF file in {:.2} s",
            start.elapsed().as_secs_f64()
        );
        Ok(store)
    }

    fn open_read(pbf: &Path, path: &Path) -> anyhow::Result<Self> {
        Self::open(pbf, path)
    }

    fn report(&self, label: &str) {
        let bytes = self.memory_bytes() as f64;
        println!(
            "{label}: {} nodes, {:.3} GB in memory, {:.2} bytes per node",
            self.len(),
            bytes / 1e9,
            bytes / self.len().max(1) as f64
        );
    }

    fn report_files(_label: &str, _path: &Path, _nodes: Option<u64>) {}
}

fn report_paged(label: &str, s: &PagedStats) {
    let page_bytes = (s.pages * 4096) as f64;
    println!(
        "{label}: {} nodes, {} blocks, {} pages; {:.2} bytes per node in pages ({:.2} GB), {:.1}% of page bytes used; \
         file {:.2} GB",
        s.nodes,
        s.blocks,
        s.pages,
        page_bytes / s.nodes as f64,
        page_bytes / 1e9,
        100.0 * s.used_bytes as f64 / page_bytes,
        s.file_bytes as f64 / 1e9,
    );
    let pct = |v: u64| 100.0 * v as f64 / s.blocks as f64;
    println!(
        "  blocks: {:.1}% full, {:.1}% one run, {:.1}% mixed; {:.1}% plain, {:.1}% varint, {:.1}% packed",
        pct(s.masks[0]),
        pct(s.masks[1]),
        pct(s.masks[2]),
        pct(s.codecs[0]),
        pct(s.codecs[1]),
        pct(s.codecs[2]),
    );
}

/// The files a store at `path` may have
fn files(path: &Path) -> Vec<PathBuf> {
    ["", ".dir", ".undo"]
        .iter()
        .map(|ext| PathBuf::from(format!("{}{ext}", path.display())))
        .collect()
}

/// Prints the memory the process holds, other than mapped files.
fn report_memory(label: &str) {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    if let Some(line) = status.lines().find(|l| l.starts_with("RssAnon:")) {
        let kb = line.split_whitespace().nth(1).unwrap_or("?");
        println!("  {label}: {kb} KB of memory, besides mapped files");
    }
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        // SAFETY: glibc's statistics of its allocator
        let m = unsafe { libc::mallinfo2() };
        println!(
            "  {label}: malloc has {} MB in use, {} MB free in its arenas, {} MB mapped",
            m.uordblks >> 20,
            m.fordblks >> 20,
            m.hblkhd >> 20
        );
    }
}

/// Prints the bytes the store's files take on disk, given its node count if known.
fn report_disk(label: &str, path: &Path, nodes: Option<u64>) {
    #[cfg(unix)]
    let disk: u64 = files(path)
        .iter()
        .filter_map(|f| std::fs::metadata(f).ok())
        .map(|m| std::os::unix::fs::MetadataExt::blocks(&m) * 512)
        .sum();
    #[cfg(not(unix))]
    let disk: u64 = files(path)
        .iter()
        .filter_map(|f| std::fs::metadata(f).ok())
        .map(|m| m.len())
        .sum();
    let per_node = nodes.map_or(String::new(), |n| {
        format!(", {:.2} bytes per node", disk as f64 / n as f64)
    });
    println!("{label}: {:.3} GB on disk{per_node}", disk as f64 / 1e9);
}

/// Where the PBF file's ways start, if the store recorded it for this file.
fn ways_at(store: &impl NodeRead, pbf: &Path) -> u64 {
    let size = std::fs::metadata(pbf).map_or(0, |m| m.len());
    let get = |key| {
        store
            .metadata()
            .get(key)
            .and_then(|v| v.try_into().ok())
            .map(u64::from_le_bytes)
    };
    match (get(WAYS_AT), get(PBF_BYTES)) {
        (Some(at), Some(bytes)) if bytes == size => at,
        _ => 0,
    }
}

/// Bulk loads the PBF file's nodes into an empty store, and records where its ways start. Returns
/// the number of nodes, and where the ways start. Prints how long the store took of it.
fn load<S: NodeStore>(store: &mut S, pbf: &Path) -> anyhow::Result<(u64, Option<u64>)> {
    let mut load = store.bulk_load()?;
    let (mut count, mut batch) = (0_u64, Vec::with_capacity(BATCH));
    let mut in_store = Duration::ZERO;
    let ways_at = for_each_node(pbf, |nodes| {
        batch.extend(nodes.iter().map(|n| (n.id, Location::new(n.lat, n.lon))));
        if batch.len() >= BATCH {
            let start = Instant::now();
            load.push_batch(&batch)?;
            in_store += start.elapsed();
            count += batch.len() as u64;
            batch.clear();
        }
        Ok(())
    })?;
    let start = Instant::now();
    load.push_batch(&batch)?;
    count += batch.len() as u64;
    load.finish(0)?;
    let finish = start.elapsed();
    println!(
        "  the store took {:.1} s of the load, and {:.1} s more to finish it",
        in_store.as_secs_f64(),
        finish.as_secs_f64()
    );
    if let Some(at) = ways_at {
        store.set_metadata(WAYS_AT, &at.to_le_bytes())?;
        store.set_metadata(PBF_BYTES, &std::fs::metadata(pbf)?.len().to_le_bytes())?;
        store.commit(0)?;
    }
    Ok((count, ways_at))
}

/// Builds a store from the PBF file's nodes, then, if `resolve`, times resolving the nodes of all
/// ways with it, reading the file from where its ways start. A store already at `path` is
/// replaced.
pub fn build<S: Backend>(pbf: &Path, path: &Path, fill: f64, resolve: bool) -> anyhow::Result<()> {
    let start = Instant::now();
    let mut store = S::create(path, fill)?;
    let (count, ways_at) = load(&mut store, pbf)?;
    report_memory("after the load");
    // glibc keeps what decoding freed in its arenas, 12.7 GB after the planet: memory the store
    // needs to stay in the page cache
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        // SAFETY: returns free memory of glibc's allocator to the OS
        unsafe { libc::malloc_trim(0) };
        report_memory("after trimming");
    }
    println!(
        "built {count} nodes in {:.3} s",
        start.elapsed().as_secs_f64()
    );
    store.report("built");
    S::report_files("built", path, Some(count));
    if resolve {
        store::resolve_ways(&store, pbf, ways_at.unwrap_or(0))?;
        report_memory("after resolving");
    }
    Ok(())
}

/// Resolves the nodes of every way with an existing store.
pub fn resolve<S: Backend>(pbf: &Path, path: &Path) -> anyhow::Result<()> {
    let store = S::open_read(pbf, path)?;
    store::resolve_ways(&store, pbf, ways_at(&store, pbf))?;
    report_memory("after resolving");
    Ok(())
}

pub fn verify<S: Backend>(pbf: &Path, path: &Path) -> anyhow::Result<()> {
    let start = Instant::now();
    let store = S::open_read(pbf, path)?;
    let (count, wrong) = store::verify(&store, pbf)?;
    println!(
        "{count} nodes checked, {wrong} wrong, in {:.3} s",
        start.elapsed().as_secs_f64()
    );
    S::report_files("checked", path, Some(count));
    anyhow::ensure!(wrong == 0, "the store does not match the PBF file");
    Ok(())
}

pub fn bench<S: Backend>(
    pbf: &Path,
    path: &Path,
    every: usize,
    edits: usize,
    per_commit: usize,
) -> anyhow::Result<()> {
    let mut store = S::open(pbf, path)?;
    store.report("before");
    S::report_files("before", path, None);
    let at = ways_at(&store, pbf);
    store::bench(&mut store, pbf, at, every, edits, per_commit)?;
    store.report("after");
    S::report_files("after", path, None);
    Ok(())
}
