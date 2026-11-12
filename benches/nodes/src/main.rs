//! Times osmflow's node location stores on a PBF file sorted by type and ID: bulk loading, verifying, resolving the
//! nodes of every way, lookups, and commits, for every store and every way of opening one.
//!
//! ```text
//! osm-bench-nodes [OPTIONS] <PBF> <STORE> <STEP>... [-- <DIFF>...]
//! ```
//!
//! A store path ending in `.dense` is a `DenseStore`, one ending in `.mem` a `MemoryStore`, which has no file, so it is
//! loaded in the same run as the steps that use it; any other path is a `PagedStore`. The steps run in the order given:
//!
//! | Step | Times |
//! |---|---|
//! | `load` | Deleting the store's files, then bulk loading the file's nodes; gives the store's size on disk |
//! | `verify` | Reading every node of the store in ID order (`for_each`), checked against the file |
//! | `resolve` | Looking up the nodes of every way on osmflow's pool, with `PbfReader::par_resolve` |
//! | `lookups` | Looking up the nodes of the ways of every `--every`th block of ways, in way order and in random order, on one thread and on `--threads` |
//! | `apply` | Applying the node changes of the osmChange files after `--`, in order, with one commit per file, as replication does; this changes the store |
//!
//! | Option | Effect |
//! |---|---|
//! | `--read-only` | Opens a store with a file read-only (`FileStore::open_read_only`), for `verify`, `resolve` and `lookups` |
//! | `--max-id N` | Leaves out the nodes above ID `N`, and the way refs to them, so a store of a large file fits in memory |
//! | `--every N` | `lookups` takes the nodes of every `N`th block of ways (10) |
//! | `--threads N` | The threads of the parallel lookups (the cores) |
//! | `--cold` | Drops the store's files from the page cache first, so the steps start with none of it in memory (Linux) |
//! | `--preload` | Reads the store into memory before `resolve` (`NodeRead::preload`), timed apart |
//!
//! `load`, `verify` and `resolve` drop the PBF file from the page cache first (Linux), so each reads it from the disk
//! whatever ran before it. Each step prints a line or more, starting with its name. Peak memory is the most the process had in memory during
//! the step, pages of memory-mapped files included (Linux).

#![expect(
    clippy::print_stdout,
    reason = "the measurements are the tool's output"
)]
#![expect(
    clippy::cast_precision_loss,
    reason = "rates and sizes are printed with a few digits"
)]

use std::hint::black_box;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use std::{env, fs, thread};

use anyhow::{Context as _, bail};
use osmflow::nodes::prelude::*;
use osmflow::nodes::{DenseStore, MemoryStore, PagedStore};
use osmflow::osc::{Action, Object, OscReader};
use osmflow::{Location, NoTags, ParOrdered, PbfReader, Select};

/// What the command line asks for.
struct Config {
    pbf: PathBuf,
    store: PathBuf,
    steps: Vec<String>,
    read_only: bool,
    max_id: u64,
    every: usize,
    threads: usize,
    diffs: Vec<PathBuf>,
    cold: bool,
    preload: bool,
}

impl Config {
    fn parse(mut args: impl Iterator<Item = String>) -> anyhow::Result<Self> {
        fn value<T: FromStr>(name: &str, v: Option<String>) -> anyhow::Result<T> {
            let v = v.with_context(|| format!("{name} needs a value"))?;
            v.parse()
                .ok()
                .with_context(|| format!("{name}: invalid value {v}"))
        }
        let mut cfg = Self {
            pbf: PathBuf::new(),
            store: PathBuf::new(),
            steps: Vec::new(),
            read_only: false,
            max_id: u64::MAX,
            every: 10,
            threads: thread::available_parallelism().map_or(1, usize::from),
            diffs: Vec::new(),
            cold: false,
            preload: false,
        };
        let mut positional = Vec::new();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--read-only" => cfg.read_only = true,
                "--max-id" => cfg.max_id = value(&arg, args.next())?,
                "--every" => cfg.every = value(&arg, args.next())?,
                "--threads" => cfg.threads = value(&arg, args.next())?,
                "--" => cfg.diffs.extend(args.by_ref().map(PathBuf::from)),
                "--cold" => cfg.cold = true,
                "--preload" => cfg.preload = true,
                _ if arg.starts_with("--") => bail!("unknown option {arg}"),
                _ => positional.push(arg),
            }
        }
        let mut positional = positional.into_iter();
        let (Some(pbf), Some(store)) = (positional.next(), positional.next()) else {
            bail!(
                "usage: osm-bench-nodes [OPTIONS] <PBF> <STORE> <STEP>...; see the source's doc comment"
            );
        };
        (cfg.pbf, cfg.store) = (pbf.into(), store.into());
        cfg.steps = positional.collect();
        for step in &cfg.steps {
            match step.as_str() {
                "load" | "apply" if cfg.read_only => {
                    bail!("{step} changes the store, which --read-only does not")
                }
                "load" | "apply" | "verify" | "resolve" | "lookups" => {}
                _ => bail!("unknown step {step}"),
            }
        }
        if cfg.every == 0 || cfg.threads == 0 {
            bail!("--every and --threads must be above 0");
        }
        if cfg.has("apply") == cfg.diffs.is_empty() {
            bail!("the apply step needs osmChange files after --, and they need the apply step");
        }
        Ok(cfg)
    }

    fn has(&self, step: &str) -> bool {
        self.steps.iter().any(|s| s == step)
    }
}

fn main() -> anyhow::Result<()> {
    let cfg = Config::parse(env::args().skip(1))?;
    let name = cfg.store.to_string_lossy();
    if name.ends_with(".mem") {
        if cfg.read_only {
            bail!("a memory store has no file to open read-only");
        }
        run(&cfg, &mut MemoryStore::new())
    } else if name.ends_with(".dense") {
        file::<DenseStore>(&cfg)
    } else {
        file::<PagedStore>(&cfg)
    }
}

/// Opens a store with a file, after deleting its files for a `load`, or dropping them from the page cache for
/// `--cold`, and runs the steps on it.
fn file<S: FileStore>(cfg: &Config) -> anyhow::Result<()>
where
    for<'s> S::Reader<'s>: Send,
    for<'s> <S::ReadOnly as NodeRead>::Reader<'s>: Send,
{
    for file in store_files(&cfg.store) {
        if cfg.has("load") {
            if file.exists() {
                fs::remove_file(&file).with_context(|| format!("deleting {}", file.display()))?;
            }
        } else if cfg.cold && file.exists() {
            osm_bench::evict(&file);
        }
    }
    let what = || format!("opening {}", cfg.store.display());
    if cfg.read_only {
        let store = S::open_read_only(&cfg.store).with_context(what)?;
        let pbf = open_pbf(cfg)?;
        let mut ids = None;
        for step in &cfg.steps {
            read_step(cfg, &pbf, &store, step, &mut ids)?;
        }
        Ok(())
    } else {
        run(cfg, &mut S::open(&cfg.store).with_context(what)?)
    }
}

/// The files a store at `path` may have.
fn store_files(path: &Path) -> [PathBuf; 3] {
    let with = |ext: &str| PathBuf::from(format!("{}.{ext}", path.display()));
    [path.to_path_buf(), with("dir"), with("undo")]
}

fn open_pbf(cfg: &Config) -> anyhow::Result<PbfReader> {
    Ok(PbfReader::open(&cfg.pbf)?)
}

/// Runs the steps on a store opened to read and write.
fn run<S: NodeStore>(cfg: &Config, store: &mut S) -> anyhow::Result<()>
where
    for<'s> S::Reader<'s>: Send,
{
    let pbf = open_pbf(cfg)?;
    let mut ids = None;
    for step in &cfg.steps {
        match step.as_str() {
            "load" => {
                osm_bench::evict(&cfg.pbf);
                reset_peak_memory();
                let start = Instant::now();
                let nodes = load(&pbf, store, cfg.max_id)?;
                let time = start.elapsed();
                println!(
                    "load: {nodes} nodes in {}, {}, peak memory {}",
                    secs(time),
                    rate(nodes, time),
                    peak_memory()
                );
                disk(&cfg.store, nodes);
            }
            "apply" => apply(store, &cfg.diffs, cfg.max_id)?,
            _ => read_step(cfg, &pbf, store, step, &mut ids)?,
        }
    }
    Ok(())
}

/// Runs a step that only reads the store.
fn read_step<S: NodeRead>(
    cfg: &Config,
    pbf: &PbfReader,
    store: &S,
    step: &str,
    ids: &mut Option<Vec<u64>>,
) -> anyhow::Result<()>
where
    for<'s> S::Reader<'s>: Send,
{
    if step != "lookups" {
        osm_bench::evict(&cfg.pbf);
    }
    reset_peak_memory();
    let start = Instant::now();
    match step {
        "verify" => {
            let nodes = verify(pbf, store, cfg.max_id)?;
            let time = start.elapsed();
            println!(
                "verify: {nodes} nodes in {}, {}, peak memory {}",
                secs(time),
                rate(nodes, time),
                peak_memory()
            );
        }
        "resolve" => {
            if cfg.preload {
                store.preload()?;
                println!("preload: {}", secs(start.elapsed()));
            }
            let start = Instant::now();
            let (refs, missing, left_out) = resolve(pbf, store, cfg.max_id)?;
            let time = start.elapsed();
            println!(
                "resolve: {refs} refs in {}, {}, {missing} not in the store, {left_out} above --max-id, \
                 peak memory {}",
                secs(time),
                rate(refs, time),
                peak_memory()
            );
        }
        "lookups" => lookups(store, way_nodes(pbf, cfg, ids)?, cfg.threads),
        _ => unreachable!("Config::parse accepts only the steps run handles"),
    }
    Ok(())
}

/// The nodes handed to a store at once, as many as `PbfReader::load_nodes` hands it.
const LOAD_BATCH: usize = 1 << 20;

/// Bulk loads the file's nodes up to `max_id`, and returns their number.
fn load<S: NodeStore>(pbf: &PbfReader, store: &mut S, max_id: u64) -> anyhow::Result<u64> {
    if max_id == u64::MAX {
        return Ok(pbf.load_nodes(store)?);
    }
    // as `load_nodes` does, but stopping at `max_id`
    let stamp = pbf
        .header()
        .replication_sequence
        .and_then(|s| u64::try_from(s).ok())
        .unwrap_or(0);
    let mut loader = store.bulk_load()?;
    let mut nodes = Nodes::new(pbf, max_id)?;
    let mut batch = Vec::with_capacity(LOAD_BATCH);
    let mut count = 0;
    while let Some(block) = nodes.next_batch()? {
        batch.extend_from_slice(block);
        if batch.len() >= LOAD_BATCH {
            loader.push_batch(&batch)?;
            count += batch.len() as u64;
            batch.clear();
        }
    }
    loader.push_batch(&batch)?;
    count += batch.len() as u64;
    loader.finish(stamp)?;
    Ok(count)
}

/// The file's nodes up to an ID, a block at a time, in file order.
struct Nodes {
    blocks: ParOrdered<NoTags>,
    batch: Vec<(u64, Location)>,
    max_id: u64,
    done: bool,
}

impl Nodes {
    fn new(pbf: &PbfReader, max_id: u64) -> anyhow::Result<Self> {
        Ok(Self {
            blocks: pbf.par_ordered(Select::node_locations())?,
            batch: Vec::new(),
            max_id,
            done: false,
        })
    }

    /// The next block's nodes, or `None` once past the last node up to the ID.
    fn next_batch(&mut self) -> anyhow::Result<Option<&[(u64, Location)]>> {
        while !self.done {
            let Some(block) = self.blocks.next() else {
                self.done = true;
                break;
            };
            let block = block?;
            self.batch.clear();
            let mut past = false;
            for n in block.nodes() {
                let Ok(id) = u64::try_from(n.id()) else {
                    continue;
                };
                if id > self.max_id {
                    past = true;
                    break;
                }
                self.batch
                    .push((id, Location::new(n.decimicro_lat(), n.decimicro_lon())));
            }
            // the file is sorted by type and ID: no node comes after one past the ID, nor after the ways
            self.done = past
                || (block.nodes().len() == 0 && block.ways().len() + block.relations().len() > 0);
            if !self.batch.is_empty() {
                return Ok(Some(&self.batch));
            }
        }
        Ok(None)
    }
}

/// Reads every node of the store, in ID order, and checks it against the file's nodes up to `max_id`. Returns the
/// number of nodes, and fails on the first difference.
fn verify<S: NodeRead>(pbf: &PbfReader, store: &S, max_id: u64) -> anyhow::Result<u64> {
    let mut file = Nodes::new(pbf, max_id)?;
    let (mut batch, mut at) = (Vec::new(), 0);
    let mut next = || -> anyhow::Result<Option<(u64, Location)>> {
        while at == batch.len() {
            let Some(b) = file.next_batch()? else {
                return Ok(None);
            };
            (batch, at) = (b.to_vec(), 0);
        }
        at += 1;
        Ok(Some(batch[at - 1]))
    };
    let (mut count, mut failure) = (0_u64, None);
    store.for_each(|id, loc| {
        if failure.is_some() {
            return;
        }
        match next() {
            Ok(Some(node)) if node == (id, loc) => count += 1,
            Ok(Some((file_id, file_loc))) => {
                failure = Some(anyhow::anyhow!(
                    "the store has node {id} at {loc:?} where the file has node {file_id} at {file_loc:?}"
                ));
            }
            Ok(None) => failure = Some(anyhow::anyhow!("the store has node {id}, past the file's last node")),
            Err(e) => failure = Some(e),
        }
    })?;
    if let Some(e) = failure {
        return Err(e);
    }
    if let Some((id, _)) = next()? {
        bail!("the file has node {id}, past the store's last node");
    }
    Ok(count)
}

/// Ways with their refs, and nothing else.
fn ways() -> Select<NoTags> {
    Select::all()
        .without_nodes()
        .without_relations()
        .without_metadata()
        .without_tags()
}

/// Looks up every way's nodes up to `max_id` on osmflow's pool. Returns the refs looked up, those not in the store,
/// and those left out as above `max_id`.
fn resolve<S: NodeRead>(pbf: &PbfReader, store: &S, max_id: u64) -> anyhow::Result<(u64, u64, u64)>
where
    for<'s> S::Reader<'s>: Send,
{
    let (refs, missing, left_out) = (AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0));
    pbf.par_resolve(ways(), store, |block, locator| {
        let (mut found, mut lost, mut above, mut sum) = (0, 0, 0, 0_i64);
        for way in block.ways() {
            for r in way.refs() {
                if u64::try_from(r).is_ok_and(|id| id > max_id) {
                    above += 1;
                } else if let Some(loc) = locator.location(r) {
                    found += 1;
                    sum = sum.wrapping_add(i64::from(loc.lat));
                } else {
                    lost += 1;
                }
            }
        }
        black_box(sum);
        refs.fetch_add(found + lost, Ordering::Relaxed);
        missing.fetch_add(lost, Ordering::Relaxed);
        left_out.fetch_add(above, Ordering::Relaxed);
        Ok(())
    })?;
    Ok((
        refs.into_inner(),
        missing.into_inner(),
        left_out.into_inner(),
    ))
}

/// The refs up to `--max-id` of the ways of every `--every`th block of ways, in file order, read once per run.
fn way_nodes<'a>(
    pbf: &PbfReader,
    cfg: &Config,
    ids: &'a mut Option<Vec<u64>>,
) -> anyhow::Result<&'a [u64]> {
    if let Some(ids) = ids {
        return Ok(ids);
    }
    let start = Instant::now();
    let mut found = Vec::new();
    let mut blocks = 0;
    for block in pbf.par_ordered(ways())? {
        let block = block?;
        if block.ways().len() == 0 {
            continue;
        }
        blocks += 1;
        if (blocks - 1) % cfg.every != 0 {
            continue;
        }
        for way in block.ways() {
            found.extend(
                way.refs()
                    .filter_map(|r| u64::try_from(r).ok())
                    .filter(|&id| id <= cfg.max_id),
            );
        }
    }
    println!(
        "way nodes: {} refs of {} of {blocks} blocks of ways, read in {}",
        found.len(),
        blocks.div_ceil(cfg.every),
        secs(start.elapsed())
    );
    if found.is_empty() {
        bail!("the file has no way refs to look up");
    }
    Ok(ids.insert(found))
}

/// Times looking up `ids` on one thread and on `threads`, in their order and shuffled.
fn lookups<S: NodeRead>(store: &S, ids: &[u64], threads: usize) {
    let mut shuffled = ids.to_vec();
    shuffle(&mut shuffled);
    for (order, ids) in [("way order", ids), ("random order", &shuffled)] {
        for threads in [1, threads] {
            let (time, found) = time_lookups(store, ids, threads);
            let n = ids.len() as u64;
            println!(
                "lookups, {order}, {threads} threads: {n} in {}, {}, {:.1} ns per lookup on each thread, {} not found",
                secs(time),
                rate(n, time),
                time.as_nanos() as f64 * threads as f64 / n as f64,
                n - found
            );
        }
    }
}

/// Shuffles `ids` the same way on every run: Fisher-Yates with a fixed-seed xorshift generator.
fn shuffle(ids: &mut [u64]) {
    let mut state = 0x9E37_79B9_7F4A_7C15_u64;
    for i in (1..ids.len()).rev() {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        // the remainder is at most `i`, so it fits
        let j = usize::try_from(state % (i as u64 + 1)).unwrap_or(i);
        ids.swap(i, j);
    }
}

/// Looks up `ids`, split into one run per thread, and returns the time taken and the nodes found.
fn time_lookups<S: NodeRead>(store: &S, ids: &[u64], threads: usize) -> (Duration, u64) {
    let start = Instant::now();
    let found = thread::scope(|s| {
        let runs: Vec<_> = ids
            .chunks(ids.len().div_ceil(threads))
            .map(|run| {
                s.spawn(move || {
                    let mut reader = store.reader();
                    let (mut found, mut sum) = (0_u64, 0_i64);
                    for &id in run {
                        if let Some(loc) = reader.get(id) {
                            found += 1;
                            sum = sum.wrapping_add(i64::from(loc.lat));
                        }
                    }
                    black_box(sum);
                    found
                })
            })
            .collect();
        runs.into_iter()
            .map(|run| run.join().unwrap_or_default())
            .sum()
    });
    (start.elapsed(), found)
}

/// A diff's node changes up to an ID: a node's new location, or `None` for a deleted node.
struct NodeChanges {
    changes: Vec<(u64, Option<Location>)>,
    created: u64,
    modified: u64,
    deleted: u64,
}

/// Reads the node changes of an osmChange file.
fn node_changes(path: &Path, max_id: u64) -> anyhow::Result<NodeChanges> {
    let mut nc = NodeChanges {
        changes: Vec::new(),
        created: 0,
        modified: 0,
        deleted: 0,
    };
    for change in OscReader::open(path)? {
        let change = change?;
        let Object::Node(node) = change.object else {
            continue;
        };
        let Some(id) = u64::try_from(node.id).ok().filter(|&id| id <= max_id) else {
            continue;
        };
        let count = match change.action {
            Action::Create => &mut nc.created,
            Action::Modify => &mut nc.modified,
            Action::Delete => &mut nc.deleted,
        };
        *count += 1;
        let loc = if change.action == Action::Delete {
            None
        } else {
            Some(
                node.location
                    .with_context(|| format!("{}: node {id} has no location", path.display()))?,
            )
        };
        nc.changes.push((id, loc));
    }
    Ok(nc)
}

/// Applies the node changes up to `max_id` of each diff, in order, committing once per diff, and prints the commits'
/// times. The diffs are read before any is applied, so their parsing is timed apart.
fn apply<S: NodeStore>(store: &mut S, diffs: &[PathBuf], max_id: u64) -> anyhow::Result<()> {
    let start = Instant::now();
    let diffs = diffs
        .iter()
        .map(|path| node_changes(path, max_id))
        .collect::<anyhow::Result<Vec<_>>>()?;
    let total = |f: fn(&NodeChanges) -> u64| diffs.iter().map(f).sum::<u64>();
    let (created, modified, deleted) = (
        total(|d| d.created),
        total(|d| d.modified),
        total(|d| d.deleted),
    );
    let changes = created + modified + deleted;
    println!(
        "diffs: {} files with {changes} node changes ({created} created, {modified} modified, {deleted} deleted), read \
         in {}",
        diffs.len(),
        secs(start.elapsed())
    );
    reset_peak_memory();
    let mut times = Vec::new();
    let mut stamp = store.stamp();
    for diff in &diffs {
        stamp += 1;
        let start = Instant::now();
        for &(id, loc) in &diff.changes {
            match loc {
                Some(loc) => store.set(id, loc)?,
                None => store.remove(id)?,
            }
        }
        store.commit(stamp)?;
        times.push(start.elapsed());
    }
    let all: Duration = times.iter().sum();
    times.sort_unstable();
    let at = |percent: usize| ms(times[(times.len() - 1) * percent / 100]);
    println!(
        "apply: {changes} node changes in {} commits in {}, {}; per commit median {:.1} ms, 99th percentile {:.1} ms, \
         most {:.1} ms; peak memory {}",
        times.len(),
        secs(all),
        rate(changes, all),
        at(50),
        at(99),
        at(100),
        peak_memory()
    );
    Ok(())
}

/// Prints the bytes the store's files take on disk, and their sizes, per node.
fn disk(path: &Path, nodes: u64) {
    let (mut used, mut size) = (Some(0), 0);
    for file in store_files(path) {
        if let Ok(meta) = fs::metadata(&file) {
            size += meta.len();
            used = used.zip(allocated(&meta)).map(|(a, b)| a + b);
        }
    }
    if size == 0 {
        return;
    }
    let per_node = |bytes: u64| bytes as f64 / nodes.max(1) as f64;
    match used {
        Some(used) => println!(
            "disk: {used} bytes on disk, {:.2} per node; files of {size} bytes, {:.2} per node",
            per_node(used),
            per_node(size)
        ),
        None => println!(
            "disk: files of {size} bytes, {:.2} per node",
            per_node(size)
        ),
    }
}

/// The bytes a file takes on disk, which a sparse file's holes do not.
#[cfg(unix)]
#[expect(clippy::unnecessary_wraps, reason = "only Unix tells")]
fn allocated(meta: &fs::Metadata) -> Option<u64> {
    use std::os::unix::fs::MetadataExt as _;
    Some(meta.blocks() * 512)
}

#[cfg(not(unix))]
fn allocated(_meta: &fs::Metadata) -> Option<u64> {
    None
}

/// Starts measuring the process's peak memory again (Linux).
fn reset_peak_memory() {
    let _ = fs::write("/proc/self/clear_refs", "5");
}

/// The process's peak memory since [`reset_peak_memory`], memory-mapped files included (Linux).
fn peak_memory() -> String {
    let kb = fs::read_to_string("/proc/self/status").ok().and_then(|s| {
        s.lines()
            .find_map(|l| l.strip_prefix("VmHWM:"))
            .and_then(|v| v.trim().trim_end_matches("kB").trim().parse::<u64>().ok())
    });
    kb.map_or_else(
        || "unknown".to_owned(),
        |kb| format!("{:.2} GB", kb as f64 / 1e6),
    )
}

fn secs(d: Duration) -> String {
    format!("{:.3} s", d.as_secs_f64())
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1e3
}

fn rate(n: u64, d: Duration) -> String {
    format!("{:.1} M/s", n as f64 / d.as_secs_f64() / 1e6)
}
