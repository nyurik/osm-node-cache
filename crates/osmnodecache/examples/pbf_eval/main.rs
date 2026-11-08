//! Evaluates node stores on a sorted OSM PBF file. A store path ending in `.redb` or `.dense` is
//! kept in that store (with its feature), any other in a paged store. A path ending in `.mem` is a
//! memory store, which has no file: each command loads it from the PBF file again.
//!
//! ```sh
//! # what each way of coding the node locations would take, in blocks of 2^6 IDs
//! cargo run --release --example pbf_eval -- analyze europe-latest.osm.pbf 6
//! # build a store from the file's nodes (pages filled to 0.97), then time resolving the nodes of
//! # all ways with it, reading the file from where its ways start
//! cargo run --release --example pbf_eval -- build europe-latest.osm.pbf europe.cache 0.97
//! # only build the store
//! cargo run --release --example pbf_eval -- load europe-latest.osm.pbf europe.cache 0.97
//! # time resolving the nodes of all ways with an existing store
//! cargo run --release --example pbf_eval -- resolve europe-latest.osm.pbf europe.cache
//! # check that the store has exactly the file's nodes
//! cargo run --release --example pbf_eval -- verify europe-latest.osm.pbf europe.cache
//! # time lookups of the ways' nodes (of every 10th blob), then 300000 edits committed 2000 at a
//! # time (changes the store)
//! cargo run --release --example pbf_eval -- bench europe-latest.osm.pbf europe.cache 10 300000 2000
//! # the same in redb
//! cargo run --release --features redb --example pbf_eval -- build europe-latest.osm.pbf europe.redb
//! # any command, leaving out nodes above an ID and way refs to them, so the store fits in memory
//! cargo run --release --example pbf_eval -- --max-id 5000000000 build europe-latest.osm.pbf e.cache
//! # any command, reading the PBF file with direct I/O, bypassing the page cache (on Linux): it is
//! # read from disk, faster than through the page cache on a network disk, and leaves the memory
//! # to the store
//! cargo run --release --example pbf_eval -- --direct resolve planet.osm.pbf planet.cache
//! # any command, leaving out the statistics of a paged store's pages, which read all of them
//! cargo run --release --example pbf_eval -- --no-stats --direct build planet.osm.pbf planet.cache
//! # any command, decompressing the PBF file with zlib-ng rather than zlib-rs
//! cargo run --release --example pbf_eval -- --zlib-ng decode planet.osm.pbf inflate
//! # resolve after reading all of the store into memory, in big reads on many threads
//! cargo run --release --example pbf_eval -- --direct --preload resolve planet.osm.pbf planet.cache
//! # with no store: time decoding the file's nodes, reading its blobs of nodes, decompressing them,
//! # decoding the nodes with the osmpbf crate, and decoding the ways; or only the passes listed
//! cargo run --release --example pbf_eval -- --direct decode planet.osm.pbf
//! cargo run --release --example pbf_eval -- --direct decode planet.osm.pbf read
//! ```
#![allow(clippy::cast_precision_loss)]

use std::path::PathBuf;
use std::str::FromStr;

use anyhow::Context as _;

use crate::backend::Backend;

mod analyze;
mod backend;
mod pbf;
mod store;

/// Argument `i`, or `default` if it is not given
fn arg<T: FromStr>(args: &[String], i: usize, default: Option<T>) -> anyhow::Result<T> {
    match args.get(i) {
        Some(v) => v
            .parse()
            .ok()
            .with_context(|| format!("invalid argument {v}")),
        None => default.with_context(|| format!("missing argument {i}; see the doc comment")),
    }
}

/// Runs a command on a store of kind `S`.
fn run<S: Backend>(cmd: &str, args: &[String]) -> anyhow::Result<()> {
    let (pbf, path): (PathBuf, PathBuf) = (arg(args, 1, None)?, arg(args, 2, None)?);
    match cmd {
        "build" => backend::build::<S>(&pbf, &path, arg(args, 3, Some(0.97))?, true),
        "load" => backend::build::<S>(&pbf, &path, arg(args, 3, Some(0.97))?, false),
        "resolve" => backend::resolve::<S>(&pbf, &path),
        "verify" => backend::verify::<S>(&pbf, &path),
        "bench" => backend::bench::<S>(
            &pbf,
            &path,
            arg(args, 3, Some(10))?,
            arg(args, 4, Some(300_000))?,
            arg(args, 5, Some(2000))?,
        ),
        _ => anyhow::bail!("unknown command; see the doc comment"),
    }
}

/// Runs a command on the store a feature adds, or says the feature is needed.
macro_rules! run_with {
    ($name:ident, $feature:literal, $store:ty) => {
        #[cfg(feature = $feature)]
        fn $name(cmd: &str, args: &[String]) -> anyhow::Result<()> {
            run::<$store>(cmd, args)
        }

        #[cfg(not(feature = $feature))]
        fn $name(_cmd: &str, _args: &[String]) -> anyhow::Result<()> {
            anyhow::bail!(concat!("this store needs the ", $feature, " feature"))
        }
    };
}

run_with!(run_redb, "redb", osmnodecache::RedbStore);
run_with!(run_dense, "dense", osmnodecache::DenseStore);
run_with!(run_memory, "memory", osmnodecache::MemoryStore);

fn main() -> anyhow::Result<()> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(i) = args.iter().position(|a| a == "--max-id") {
        let id: u64 = arg(&args, i + 1, None)?;
        pbf::set_max_id(id);
        args.drain(i..i + 2);
    }
    if let Some(i) = args.iter().position(|a| a == "--preload") {
        store::PRELOAD.store(true, std::sync::atomic::Ordering::Relaxed);
        args.remove(i);
    }
    if let Some(i) = args.iter().position(|a| a == "--no-stats") {
        backend::NO_STATS.store(true, std::sync::atomic::Ordering::Relaxed);
        args.remove(i);
    }
    if let Some(i) = args.iter().position(|a| a == "--zlib-ng") {
        pbf::set_zlib_ng();
        args.remove(i);
    }
    if let Some(i) = args.iter().position(|a| a == "--direct") {
        pbf::set_direct();
        args.remove(i);
    }
    let start = std::time::Instant::now();
    let cmd = args.first().map_or("", String::as_str);
    if cmd == "analyze" {
        analyze::run(&arg::<PathBuf>(&args, 1, None)?, arg(&args, 2, Some(6))?)?;
    } else if cmd == "decode" {
        let passes = if args.len() > 2 {
            args[2..].to_vec()
        } else {
            ["nodes", "read", "inflate", "osmpbf", "ways"]
                .map(String::from)
                .to_vec()
        };
        store::decode(&arg::<PathBuf>(&args, 1, None)?, &passes)?;
    } else {
        let path: PathBuf = arg(&args, 2, None)?;
        match path.extension().and_then(|e| e.to_str()) {
            Some("redb") => run_redb(cmd, &args)?,
            Some("dense") => run_dense(cmd, &args)?,
            Some("mem") => run_memory(cmd, &args)?,
            _ => run::<osmnodecache::PagedStore>(cmd, &args)?,
        }
    }
    eprintln!("done in {:.1} s", start.elapsed().as_secs_f64());
    Ok(())
}
