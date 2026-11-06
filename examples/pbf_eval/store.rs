//! What the commands check and time on every kind of node store: lookups of the ways' nodes, and
//! edits committed in small batches, as replication diffs arrive.

use std::collections::HashMap;
use std::path::Path;
use std::time::Instant;

use osmnodecache::{Location, NodeRead, NodeReader as _, NodeStore};
use rand::rngs::StdRng;
use rand::seq::SliceRandom as _;
use rand::{RngExt as _, SeedableRng as _};
use rayon::prelude::*;

use crate::pbf::{for_each_node, for_each_way, map_ways};

/// A node's new location, or `None` to remove it
pub type Edit = (u64, Option<Location>);

/// Whether to read all of the store into memory before resolving ways
pub static PRELOAD: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// First ID of the nodes that edits add, above all IDs in today's OSM data
const NEW_IDS: u64 = 20_000_000_000;

/// Counts the PBF file's nodes, and those the store has with another location or not at all.
pub fn verify(store: &impl NodeRead, pbf: &Path) -> anyhow::Result<(u64, usize)> {
    let (mut count, mut wrong) = (0_u64, 0_usize);
    for_each_node(pbf, |nodes| {
        count += nodes.len() as u64;
        wrong += nodes
            .par_chunks(4096)
            .map(|c| {
                let mut reader = store.reader();
                c.iter()
                    .filter(|n| reader.get(n.id) != Some(Location::new(n.lat, n.lon)))
                    .count()
            })
            .sum::<usize>();
        Ok(())
    })?;
    Ok((count, wrong))
}

/// Times the parts of reading a PBF file, with no store, in the order of `passes`: reading its
/// blobs (`read`), decompressing the blobs of nodes (`inflate`), decoding the nodes with this
/// tool's decoder (`nodes`) and with the `osmpbf` crate (`osmpbf`), and decoding the ways (`ways`).
/// Reading and decoding ways start where the nodes end, once a `nodes` pass found it.
pub fn decode(pbf: &Path, passes: &[String]) -> anyhow::Result<()> {
    let mut ways_at = None;
    for pass in passes {
        let start = Instant::now();
        let secs = || start.elapsed().as_secs_f64();
        match pass.as_str() {
            "nodes" => {
                let (mut count, mut sum) = (0_u64, 0_u64);
                ways_at = for_each_node(pbf, |nodes| {
                    count += nodes.len() as u64;
                    sum = nodes
                        .iter()
                        .fold(sum, |s, n| s.wrapping_add(crate::pbf::checksum(n)));
                    Ok(())
                })?;
                let s = secs();
                println!(
                    "decoded {count} nodes in {s:.1} s: {:.1} M/s; checksum {sum:x}",
                    count as f64 / s / 1e6
                );
            }
            "read" => {
                let bytes = crate::pbf::read_only(pbf, 0, ways_at)?;
                let s = secs();
                println!(
                    "read {:.2} GB of blobs in {s:.1} s: {:.0} MB/s",
                    bytes as f64 / 1e9,
                    bytes as f64 / s / 1e6
                );
            }
            "inflate" => {
                let bytes = crate::pbf::inflate_nodes(pbf)?;
                let s = secs();
                println!(
                    "decompressed {:.2} GB of node blobs in {s:.1} s: {:.0} MB/s",
                    bytes as f64 / 1e9,
                    bytes as f64 / s / 1e6
                );
            }
            "osmpbf" => {
                let (n, sum) = crate::pbf::osmpbf_nodes(pbf)?;
                let s = secs();
                println!(
                    "decoded {n} nodes with osmpbf in {s:.1} s: {:.1} M/s; checksum {sum:x}",
                    n as f64 / s / 1e6
                );
            }
            "ways" => {
                let (mut ways, mut refs) = (0, 0);
                for_each_way(pbf, ways_at.unwrap_or(0), |w| {
                    ways += w.len() as u64;
                    refs += w.refs() as u64;
                    Ok(())
                })?;
                let s = secs();
                println!(
                    "decoded {ways} ways' {refs} node refs in {s:.1} s: {:.1} M refs/s",
                    refs as f64 / s / 1e6
                );
            }
            _ => anyhow::bail!("unknown pass {pass}"),
        }
    }
    Ok(())
}

/// Resolves the nodes of every way to their locations, as an importer building the ways'
/// geometries does: each blob's ways on the thread that decoded the blob. Reads the PBF file from
/// `ways_at`, where its ways start, if known.
pub fn resolve_ways(store: &impl NodeRead, pbf: &Path, ways_at: u64) -> anyhow::Result<()> {
    let start = Instant::now();
    if PRELOAD.load(std::sync::atomic::Ordering::Relaxed) {
        store.preload()?;
        println!(
            "preloaded the store in {:.1} s",
            start.elapsed().as_secs_f64()
        );
    }
    let (mut ways, mut refs, mut missing, mut busy) = (0_u64, 0_u64, 0_u64, 0_f64);
    map_ways(
        pbf,
        ways_at,
        |blob_ways| {
            let start = Instant::now();
            let mut reader = store.reader();
            let (mut geom, mut refs) = (Vec::new(), 0);
            let mut missing = 0;
            for way in blob_ways.iter() {
                geom.clear();
                geom.extend(way.iter().filter_map(|&id| reader.get(id.cast_unsigned())));
                std::hint::black_box(geom.as_slice());
                refs += way.len() as u64;
                missing += (way.len() - geom.len()) as u64;
            }
            let ways = blob_ways.len() as u64;
            (ways, refs, missing, start.elapsed().as_secs_f64())
        },
        |(w, r, m, t)| {
            (ways, refs, missing, busy) = (ways + w, refs + r, missing + m, busy + t);
            Ok(())
        },
    )?;
    let wall = start.elapsed().as_secs_f64();
    println!(
        "resolved {ways} ways' {refs} node refs, {missing} missing, in {wall:.3} s on {} threads: \
         {:.1} M refs/s; the lookups took {busy:.0} thread-seconds, {:.0} ns per ref",
        rayon::current_num_threads(),
        refs as f64 / wall / 1e6,
        1e9 * busy / refs as f64,
    );
    Ok(())
}

/// The node IDs of every `every`-th blob's ways, in way order.
pub fn sample_refs(pbf: &Path, ways_at: u64, every: usize) -> anyhow::Result<Vec<u64>> {
    let mut refs = Vec::new();
    let mut i = 0;
    for_each_way(pbf, ways_at, |ways| {
        if i % every == 0 {
            refs.extend(ways.iter().flatten().map(|&id| id.cast_unsigned()));
        }
        i += 1;
        Ok(())
    })?;
    Ok(refs)
}

pub fn time_lookups(store: &impl NodeRead, label: &str, refs: &[u64]) {
    let one = &refs[..refs.len().min(20_000_000)];
    let start = Instant::now();
    let mut reader = store.reader();
    std::hint::black_box(one.iter().filter(|&&id| reader.get(id).is_some()).count());
    let read = start.elapsed().as_secs_f64();
    let start = Instant::now();
    let found: usize = refs
        .par_chunks(1 << 16)
        .map(|c| {
            let mut reader = store.reader();
            c.iter().filter(|&&id| reader.get(id).is_some()).count()
        })
        .sum();
    let par = start.elapsed().as_secs_f64();
    println!(
        "{label}: {} lookups, {:.2}% found; {:.0} ns each on one thread, {:.1} M/s on {} threads",
        refs.len(),
        100.0 * found as f64 / refs.len() as f64,
        1e9 * read / one.len() as f64,
        refs.len() as f64 / par / 1e6,
        rayon::current_num_threads(),
    );
}

/// A location moved back within ±90° and ±180°
fn valid(lat: i32, lon: i32) -> Location {
    Location::new(
        lat.clamp(-900_000_000, 900_000_000),
        lon.clamp(-1_800_000_000, 1_800_000_000),
    )
}

/// Edits in the proportions of Europe's daily diffs, in the order they arrive: as many nodes moved a
/// little as deleted in short runs, and four times as many added above all IDs in runs.
fn make_edits(store: &impl NodeRead, refs: &[u64], count: usize) -> Vec<Edit> {
    let mut rng = StdRng::seed_from_u64(2);
    let mut reader = store.reader();
    let mut next_id = NEW_IDS;
    let mut edits = Vec::with_capacity(count);
    while edits.len() < count {
        let id = refs[rng.random_range(0..refs.len())];
        // run lengths average 1, 10 and 100
        match rng.random_range(0..114) {
            0..100 => {
                if let Some(old) = reader.get(id) {
                    let r = if rng.random_bool(0.9) { 500 } else { 5000 };
                    let loc = valid(
                        old.lat + rng.random_range(-r..=r),
                        old.lon + rng.random_range(-r..=r),
                    );
                    edits.push((id, Some(loc)));
                }
            }
            100..110 => edits.extend((id..id + rng.random_range(1..20)).map(|id| (id, None))),
            _ => {
                let mut loc = reader.get(id).unwrap_or_default();
                for _ in 0..rng.random_range(1..200) {
                    loc = valid(
                        loc.lat + rng.random_range(-3000..=3000),
                        loc.lon + rng.random_range(-3000..=3000),
                    );
                    edits.push((next_id, Some(loc)));
                    next_id += 1 + u64::from(rng.random_bool(0.05));
                }
            }
        }
    }
    edits.truncate(count);
    edits
}

/// Times lookups, then `edits` edits committed `per_commit` at a time (this changes the store).
pub fn bench(
    store: &mut impl NodeStore,
    pbf: &Path,
    ways_at: u64,
    every: usize,
    edits: usize,
    per_commit: usize,
) -> anyhow::Result<()> {
    let start = Instant::now();
    let refs = sample_refs(pbf, ways_at, every)?;
    println!(
        "read {} way node refs in {:.1} s",
        refs.len(),
        start.elapsed().as_secs_f64()
    );
    time_lookups(store, "ways' nodes in way order", &refs);
    let mut shuffled = refs[..refs.len().min(200_000_000)].to_vec();
    shuffled.shuffle(&mut StdRng::seed_from_u64(1));
    time_lookups(store, "the same nodes in random order", &shuffled);

    let edits = make_edits(store, &refs, edits);
    // the time to apply each batch of edits and commit it, and of the commit alone
    let (mut batches, mut commits) = (Vec::new(), Vec::new());
    for chunk in edits.chunks(per_commit) {
        let start = Instant::now();
        for &(id, loc) in chunk {
            match loc {
                Some(loc) => store.set(id, loc)?,
                None => store.remove(id)?,
            }
        }
        let commit = Instant::now();
        store.commit(store.stamp() + 1)?;
        commits.push(commit.elapsed().as_secs_f64());
        batches.push(start.elapsed().as_secs_f64());
    }
    let total: f64 = batches.iter().sum();
    let committing: f64 = commits.iter().sum();
    batches.sort_by(f64::total_cmp);
    commits.sort_by(f64::total_cmp);
    println!(
        "{} edits in {} commits of {per_commit}: {total:.1} s, {:.1} µs per edit; \
         per commit median {:.1} ms, 99th percentile {:.1} ms, max {:.1} ms",
        edits.len(),
        batches.len(),
        1e6 * total / edits.len() as f64,
        1e3 * batches[batches.len() / 2],
        1e3 * batches[batches.len() * 99 / 100],
        1e3 * batches[batches.len() - 1],
    );
    println!(
        "  of which the commits alone: {committing:.1} s; median {:.1} ms, 99th percentile {:.1} ms",
        1e3 * commits[commits.len() / 2],
        1e3 * commits[commits.len() * 99 / 100],
    );
    let expected: HashMap<u64, Option<Location>> = edits.into_iter().collect();
    let wrong = expected
        .par_iter()
        .collect::<Vec<_>>()
        .par_chunks(4096)
        .map(|c| {
            let mut reader = store.reader();
            c.iter()
                .filter(|&(&id, &loc)| reader.get(id) != loc)
                .count()
        })
        .sum::<usize>();
    println!("{} edited nodes checked, {wrong} wrong", expected.len());
    anyhow::ensure!(wrong == 0, "edits were lost");
    time_lookups(store, "ways' nodes in way order, after the edits", &refs);
    Ok(())
}
