//! Evaluates node stores on a sorted OSM PBF file. A store path ending in `.redb` is kept in redb
//! (with the `redb` feature), any other in a paged file.
//!
//! ```sh
//! # what each way of coding the node locations would take, in blocks of 2^6 IDs
//! cargo run --release --example pbf_eval -- analyze europe-latest.osm.pbf 6
//! # build a store from the file's nodes (a paged file filled to 0.97), then time resolving the
//! # nodes of all ways with it
//! cargo run --release --example pbf_eval -- build europe-latest.osm.pbf europe.cache 0.97
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
//! cargo run --release --example pbf_eval -- --max-id 2000000000 build europe-latest.osm.pbf e.cache
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
        "build" => backend::build::<S>(&pbf, &path, arg(args, 3, Some(0.97))?),
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

#[cfg(feature = "redb")]
fn run_redb(cmd: &str, args: &[String]) -> anyhow::Result<()> {
    run::<osmnodecache::KvNodeStore<osmnodecache::RedbBackend>>(cmd, args)
}

#[cfg(not(feature = "redb"))]
fn run_redb(_cmd: &str, _args: &[String]) -> anyhow::Result<()> {
    anyhow::bail!("a redb store needs the redb feature")
}

fn main() -> anyhow::Result<()> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(i) = args.iter().position(|a| a == "--max-id") {
        let id: u64 = arg(&args, i + 1, None)?;
        pbf::set_max_id(id);
        args.drain(i..i + 2);
    }
    let start = std::time::Instant::now();
    let cmd = args.first().map_or("", String::as_str);
    if cmd == "analyze" {
        analyze::run(&arg::<PathBuf>(&args, 1, None)?, arg(&args, 2, Some(6))?)?;
    } else {
        let path: PathBuf = arg(&args, 2, None)?;
        match path.extension().and_then(|e| e.to_str()) {
            Some("redb") => run_redb(cmd, &args)?,
            _ => run::<osmnodecache::PagedFileCache>(cmd, &args)?,
        }
    }
    eprintln!("done in {:.1} s", start.elapsed().as_secs_f64());
    Ok(())
}
