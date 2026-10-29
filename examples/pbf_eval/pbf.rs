//! Reading the nodes and ways of a sorted OSM PBF file in order, decoding the blobs in parallel.

use std::path::Path;
use std::sync::mpsc::sync_channel;
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, Instant};

use osmpbf::{Blob, BlobDecode, BlobReader, BlobType};
use rayon::prelude::*;

/// Blobs decoded together; each holds about 8000 nodes or ways.
const BATCH: usize = 256;
/// How often to report how much of the file has been read
const PROGRESS: Duration = Duration::from_secs(30);

/// Nodes above this ID, and way refs to them, are left out, to measure a store that fits in memory.
static MAX_ID: OnceLock<u64> = OnceLock::new();

/// Leaves out nodes above `id` from here on.
pub fn set_max_id(id: u64) {
    MAX_ID.set(id).expect("the node ID limit is set once");
}

fn max_id() -> u64 {
    MAX_ID.get().copied().unwrap_or(u64::MAX)
}

#[derive(Clone, Copy)]
pub struct Node {
    pub id: u64,
    pub lat: i32,
    pub lon: i32,
}

/// Calls `f` with the nodes of each blob, in file order (by ID in a sorted file).
pub fn for_each_node(
    path: &Path,
    mut f: impl FnMut(&[Node]) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let max = max_id();
    let mut past_max = false;
    let res = for_each_blob(path, decode_nodes, true, |nodes| {
        let n = nodes.partition_point(|n| n.id <= max);
        if n > 0 {
            f(&nodes[..n])?;
        }
        // an error stops reading the file
        past_max = n < nodes.len();
        anyhow::ensure!(!past_max, "past the node ID limit");
        Ok(())
    });
    if past_max {
        Ok(())
    } else {
        res
    }
}

/// Calls `f` with the node lists of each blob's ways, in file order.
pub fn for_each_way(
    path: &Path,
    f: impl FnMut(Vec<Vec<i64>>) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    for_each_blob(path, decode_ways, false, f)
}

/// Calls `f` with what `map` returns for the node lists of each blob's ways, in file order. `map`
/// runs on the threads that decode the blobs.
pub fn map_ways<T: Send>(
    path: &Path,
    map: impl Fn(Vec<Vec<i64>>) -> T + Sync,
    f: impl FnMut(T) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    for_each_blob(path, |blob| decode_ways(blob).map(&map), false, f)
}

/// Calls `f` with each blob that `decode` returns something for. A sorted file has all nodes first,
/// so with `nodes_only` reading stops at the first blob without nodes.
fn for_each_blob<T: Send>(
    path: &Path,
    decode: impl Fn(&Blob) -> Option<T> + Sync,
    nodes_only: bool,
    f: impl FnMut(T) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let reader = BlobReader::from_path(path)?;
    let size = std::fs::metadata(path)?.len();
    let (start, mut reported) = (Instant::now(), Instant::now());
    let mut progress = move |blob: &Blob| {
        if reported.elapsed() >= PROGRESS {
            reported = Instant::now();
            let offset = blob.offset().map_or(0, |o| o.0);
            eprintln!(
                "  {:.1}% of the file read in {:.0} s",
                100.0 * offset as f64 / size as f64,
                start.elapsed().as_secs_f64()
            );
        }
    };
    let (tx, rx) = sync_channel::<Vec<Option<T>>>(4);
    let decode = &decode;
    thread::scope(|s| {
        // owns `tx`, so the channel closes when it is done
        let producer = s.spawn(move || -> anyhow::Result<()> {
            let mut batch = Vec::with_capacity(BATCH);
            for blob in reader {
                let blob = blob?;
                if matches!(blob.get_type(), BlobType::OsmData) {
                    batch.push(blob);
                }
                if batch.len() == BATCH {
                    progress(&batch[0]);
                    let decoded: Vec<_> = batch.par_iter().map(decode).collect();
                    let done = nodes_only && decoded.iter().any(Option::is_none);
                    if tx.send(decoded).is_err() || done {
                        return Ok(());
                    }
                    batch.clear();
                }
            }
            let _ = tx.send(batch.par_iter().map(decode).collect());
            Ok(())
        });
        // if `f` fails, dropping `rx` stops the producer too
        let res = rx.into_iter().flatten().flatten().try_for_each(f);
        producer.join().expect("PBF reader thread panicked")?;
        res
    })
}

fn decode_nodes(blob: &Blob) -> Option<Vec<Node>> {
    let BlobDecode::OsmData(block) = blob.decode().expect("invalid PBF blob") else {
        return None;
    };
    let mut nodes = Vec::new();
    for group in block.groups() {
        nodes.extend(group.dense_nodes().map(|n| Node {
            id: n.id().cast_unsigned(),
            lat: n.decimicro_lat(),
            lon: n.decimicro_lon(),
        }));
        nodes.extend(group.nodes().map(|n| Node {
            id: n.id().cast_unsigned(),
            lat: n.decimicro_lat(),
            lon: n.decimicro_lon(),
        }));
    }
    (!nodes.is_empty()).then_some(nodes)
}

fn decode_ways(blob: &Blob) -> Option<Vec<Vec<i64>>> {
    let BlobDecode::OsmData(block) = blob.decode().expect("invalid PBF blob") else {
        return None;
    };
    let max = i64::try_from(max_id()).unwrap_or(i64::MAX);
    let ways: Vec<Vec<i64>> = block
        .groups()
        .flat_map(|g| {
            g.ways()
                .map(|w| w.refs().filter(|&id| id <= max).collect())
                .collect::<Vec<_>>()
        })
        .filter(|refs: &Vec<i64>| !refs.is_empty())
        .collect();
    (!ways.is_empty()).then_some(ways)
}
