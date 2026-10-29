//! The commands that build, check and time a node store, for each kind of store.

use std::path::Path;
use std::time::Instant;

#[cfg(feature = "redb")]
use osmnodecache::{KvNodeStore, KvOpts};
use osmnodecache::{
    Location, NodeBuilder as _, NodeStore, PagedFileCache, PagedFileCacheOpts, PagedStats,
};

use crate::pbf::for_each_node;
use crate::store;

/// The memory a database store may use to cache what it read
#[cfg(feature = "redb")]
const CACHE_BYTES: usize = 16 << 30;

pub trait Backend: NodeStore {
    /// The options, filling pages to `fill` where the store has pages to fill
    fn opts(fill: f64) -> Self::Opts;

    /// Prints the store's size, given its node count if known.
    fn report(&self, label: &str, nodes: Option<u64>) -> anyhow::Result<()>;
}

impl Backend for PagedFileCache {
    fn opts(fill: f64) -> PagedFileCacheOpts {
        PagedFileCacheOpts::default().fill(fill)
    }

    fn report(&self, label: &str, _nodes: Option<u64>) -> anyhow::Result<()> {
        report_paged(label, &self.stats());
        Ok(())
    }
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

#[cfg(feature = "redb")]
fn report_kv(label: &str, disk: u64, nodes: Option<u64>) {
    let per_node = nodes.map_or(String::new(), |n| {
        format!(", {:.2} bytes per node", disk as f64 / n as f64)
    });
    println!("{label}: {:.2} GB on disk{per_node}", disk as f64 / 1e9);
}

#[cfg(feature = "redb")]
impl Backend for KvNodeStore<osmnodecache::RedbBackend> {
    fn opts(_fill: f64) -> Self::Opts {
        KvOpts::default().backend(osmnodecache::RedbOpts::default().cache_bytes(CACHE_BYTES))
    }

    fn report(&self, label: &str, nodes: Option<u64>) -> anyhow::Result<()> {
        report_kv(label, self.disk_bytes()?, nodes);
        Ok(())
    }
}

/// Builds a store from the PBF file's nodes, then times resolving the nodes of all ways with it.
pub fn build<S: Backend>(pbf: &Path, path: &Path, fill: f64) -> anyhow::Result<()> {
    let start = Instant::now();
    let mut b = S::builder(path, &S::opts(fill))?;
    let mut count = 0_u64;
    for_each_node(pbf, |nodes| {
        for n in nodes {
            b.push(n.id, Location::new(n.lat, n.lon))?;
        }
        count += nodes.len() as u64;
        Ok(())
    })?;
    let store = b.finish(0)?;
    println!(
        "built {count} nodes in {:.3} s",
        start.elapsed().as_secs_f64()
    );
    store.report("built", Some(count))?;
    store::resolve_ways(&store, pbf)
}

fn open<S: Backend>(path: &Path) -> anyhow::Result<S> {
    let start = Instant::now();
    let store = S::open(path, &S::opts(0.97))?;
    println!("opened in {:.2} s", start.elapsed().as_secs_f64());
    Ok(store)
}

/// Resolves the nodes of every way with an existing store.
pub fn resolve<S: Backend>(pbf: &Path, path: &Path) -> anyhow::Result<()> {
    store::resolve_ways(&open::<S>(path)?, pbf)
}

pub fn verify<S: Backend>(pbf: &Path, path: &Path) -> anyhow::Result<()> {
    let start = Instant::now();
    let store = open::<S>(path)?;
    let (count, wrong) = store::verify(&store, pbf)?;
    println!(
        "{count} nodes checked, {wrong} wrong, in {:.3} s",
        start.elapsed().as_secs_f64()
    );
    store.report("checked", Some(count))?;
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
    let mut store = open::<S>(path)?;
    store.report("before", None)?;
    store::bench(&mut store, pbf, every, edits, per_commit)?;
    store.report("after", None)?;
    Ok(())
}
