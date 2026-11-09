//! Parallel reads on a pool of the caller's, from one of the pool's own threads. A thread of the decoding pool that
//! waits for a block runs the pool's work meanwhile; blocking would leave a one-thread pool with nothing to run it.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::channel;
use std::thread;
use std::time::Duration;

use osmflow::{PbfReader, Select};

/// Runs `read` on the only thread of a one-thread pool, and fails if it does not finish in time.
fn on_one_thread_pool(read: impl FnOnce(&PbfReader) -> usize + Send + 'static) -> usize {
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/planetiler/monaco-latest.osm.pbf");
    let pool = Arc::new(
        rayon::ThreadPoolBuilder::new()
            .num_threads(1)
            .build()
            .unwrap(),
    );
    let file = PbfReader::open(path).unwrap().with_pool(Arc::clone(&pool));
    let (tx, rx) = channel();
    thread::spawn(move || tx.send(pool.install(|| read(&file))).unwrap());
    rx.recv_timeout(Duration::from_secs(60))
        .expect("a read on its own pool's only thread did not finish")
}

#[test]
fn ordered_read_on_its_own_pool() {
    let nodes = on_one_thread_pool(|file| {
        file.par_ordered(Select::all())
            .unwrap()
            .map(|block| block.unwrap().nodes().len())
            .sum()
    });
    assert!(nodes > 0);
}

#[test]
fn unordered_read_on_its_own_pool() {
    let nodes = on_one_thread_pool(|file| {
        let nodes = AtomicUsize::new(0);
        file.par_unordered(Select::all(), |block| {
            nodes.fetch_add(block.nodes().len(), Ordering::Relaxed);
            Ok(())
        })
        .unwrap();
        nodes.into_inner()
    });
    assert!(nodes > 0);
}
