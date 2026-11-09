//! Lookups and commits of every node store, on synthetic nodes that fit in memory.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::{env, fs, process};

use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use osmflow::Location;
use osmflow::nodes::prelude::*;
use osmflow::nodes::{DenseStore, MemoryStore, PagedStore};
use rand::rngs::StdRng;
use rand::seq::SliceRandom as _;
use rand::{RngExt as _, SeedableRng as _};

const NODES: usize = 2_000_000;

/// IDs mostly in runs, as in OSM data, and locations a short step apart.
fn nodes() -> BTreeMap<u64, Location> {
    let rng = &mut StdRng::seed_from_u64(1);
    let (mut id, mut loc) = (1, Location::new(0, 0));
    (0..NODES)
        .map(|_| {
            id += if rng.random_bool(0.8) {
                1
            } else {
                rng.random_range(2..300)
            };
            loc = Location::new(
                (loc.lat + rng.random_range(-2000..=2000)).clamp(-900_000_000, 900_000_000),
                (loc.lon + rng.random_range(-2000..=2000)).clamp(-1_800_000_000, 1_800_000_000),
            );
            (id, loc)
        })
        .collect()
}

fn path(name: &str) -> PathBuf {
    let path = env::temp_dir().join(format!("osmflow-nodes-bench-{}-{name}", process::id()));
    remove(&path);
    path
}

fn remove(path: &PathBuf) {
    let _ = fs::remove_file(path);
    for ext in ["dir", "undo"] {
        let _ = fs::remove_file(format!("{}.{ext}", path.display()));
    }
}

/// Times a store kept in a file, in the temporary directory.
fn bench_file<S: FileStore>(c: &mut Criterion, name: &str, nodes: &BTreeMap<u64, Location>) {
    let path = path(name);
    bench_store(c, name, nodes, S::open(&path).unwrap());
    remove(&path);
}

/// Times an empty `store`, loaded with `nodes`.
fn bench_store<S: NodeStore>(
    c: &mut Criterion,
    name: &str,
    nodes: &BTreeMap<u64, Location>,
    mut store: S,
) {
    let all: Vec<(u64, Location)> = nodes.iter().map(|(&k, &v)| (k, v)).collect();
    let mut load = store.bulk_load().unwrap();
    load.push_batch(&all).unwrap();
    load.finish(0).unwrap();

    let ids: Vec<u64> = all
        .iter()
        .map(|&(id, _)| id)
        .step_by(7)
        .take(100_000)
        .collect();
    c.bench_function(&format!("{name}: 100k lookups in ID order"), |b| {
        b.iter(|| {
            let mut reader = store.reader();
            ids.iter().filter(|&&id| reader.get(id).is_some()).count()
        });
    });
    let mut shuffled = ids.clone();
    shuffled.shuffle(&mut StdRng::seed_from_u64(2));
    c.bench_function(&format!("{name}: 100k lookups in random order"), |b| {
        b.iter(|| {
            let mut reader = store.reader();
            shuffled
                .iter()
                .filter(|&&id| reader.get(id).is_some())
                .count()
        });
    });
    let rng = &mut StdRng::seed_from_u64(3);
    let mut stamp = 0;
    c.bench_function(&format!("{name}: commit 2000 moved nodes"), |b| {
        b.iter_batched(
            || {
                (0..2000)
                    .map(|_| all[rng.random_range(0..all.len())])
                    .collect::<Vec<_>>()
            },
            |edits| {
                for (id, loc) in edits {
                    store.set(id, Location::new(loc.lat / 2, loc.lon)).unwrap();
                }
                stamp += 1;
                store.commit(stamp).unwrap();
            },
            BatchSize::SmallInput,
        );
    });
}

fn bench_stores(c: &mut Criterion) {
    let nodes = nodes();
    bench_file::<PagedStore>(c, "paged", &nodes);
    bench_file::<DenseStore>(c, "dense", &nodes);
    bench_store(c, "memory", &nodes, MemoryStore::new());
}

criterion_group! {
    name = benches;
    config = Criterion::default().sample_size(10);
    targets = bench_stores
}
criterion_main!(benches);
