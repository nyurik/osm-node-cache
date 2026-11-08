//! Reading a whole file: its header, then its blocks, one after another or on a pool of threads.

use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, sync_channel};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;

use rayon::ThreadPool;

use crate::blob::{BlobKind, BlobReader, RawBlob};
use crate::block::{Block, Select};
use crate::error::{Error, Result};
use crate::header::Header;

/// Bytes read from the file at once.
const READ_BUFFER: usize = 8 << 20;
/// The limit of bytes in flight between the reader and the consumer (compressed blobs, decoded or not): this much per
/// pool thread, and at least the minimum.
const BYTES_IN_FLIGHT_PER_THREAD: usize = 16 << 20;
const MIN_BYTES_IN_FLIGHT: usize = 256 << 20;
/// A decoded block takes a few times its uncompressed size; this is the factor counted against the limit. Counting
/// the declared uncompressed size, not the compressed one, keeps a highly compressible blob from being admitted at a
/// fraction of the memory it decodes to.
const DECODED_FACTOR: usize = 3;

/// An OSM PBF file, with its header read.
#[derive(Debug, Clone)]
pub struct PbfReader {
    path: PathBuf,
    header: Header,
    /// Where the parallel readers decode blocks; `None` for the shared default pool.
    pool: Option<Arc<ThreadPool>>,
}

impl PbfReader {
    /// Opens a file and reads its header, its first blob. A file that starts with a data blob instead, which the
    /// format does not allow but some tools write, gets an empty [`Header`]; an empty file is an error.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let header = match blobs(&path)?.next().transpose()? {
            Some(blob) if blob.kind == BlobKind::Header => {
                blob.with_content(|data| Header::parse(data, blob.position))?
            }
            Some(blob) if blob.kind == BlobKind::Data => Header::default(),
            Some(RawBlob {
                kind: BlobKind::Other(kind),
                ..
            }) => return Err(Error::MissingHeader(kind)),
            Some(_) => unreachable!("header and data blobs are matched above"),
            None => return Err(Error::EmptyFile),
        };
        Ok(Self {
            path,
            header,
            pool: None,
        })
    }

    /// Decodes blocks on `pool` in the parallel readers, instead of the default: one pool shared by every reader, with
    /// two threads fewer than the cores (one is left for reading the file, one for the consumer).
    #[must_use]
    pub fn with_pool(mut self, pool: Arc<ThreadPool>) -> Self {
        self.pool = Some(pool);
        self
    }

    /// The file's header.
    #[must_use]
    pub fn header(&self) -> &Header {
        &self.header
    }

    /// The file's blocks in file order, each decoded on the calling thread.
    pub fn blocks(&self, select: Select) -> Result<impl Iterator<Item = Result<Block>> + use<>> {
        Ok(data_blobs(&self.path)?.map(move |blob| blob?.decode(select)))
    }

    /// Calls `f` for every block on the pool, unordered within each run of one object type, with a barrier wherever
    /// the type changes in file order: the consecutive blocks holding the same types form a run, handed to `f` in
    /// parallel and in no particular order, and a run starts only once `f` has returned for every block before it.
    /// In a file sorted by type, as published files are, every node is handled before any way, and every way before
    /// any relation, as resolving way geometry needs, at the speed of a read with no barriers: blocks are still read
    /// and decoded ahead while a run's last blocks finish, so a barrier costs only that short tail. A file whose
    /// types come back (nodes again after ways) gets a barrier at every change, so one whose types alternate block by
    /// block is read nearly one block at a time. A block mixing types is a run of its own, and a block holding none
    /// of the selected types joins the run it is in. Stops at the first error, from reading or from `f`.
    pub fn par_unordered<F>(&self, select: Select, f: F) -> Result<()>
    where
        F: Fn(Block) -> Result<()> + Sync,
    {
        let pool = self.pool()?;
        let run = Run {
            f,
            gate: Mutex::new(Gate::default()),
            budget: Budget::for_pool(&pool),
            failed: AtomicBool::new(false),
            first_error: Mutex::new(None),
        };
        pool.in_place_scope(|scope| {
            let run = &run;
            let mut index = 0;
            let read = admit(&self.path, &run.budget, |blob, permit| {
                let i = index;
                index += 1;
                lock(&run.gate).admit();
                scope.spawn(move |scope| {
                    if run.failed.load(Ordering::Relaxed) {
                        return;
                    }
                    match blob.decode(select) {
                        Err(e) => run.fail(e),
                        Ok(block) => {
                            let ready = lock(&run.gate).decoded(i, block, permit);
                            run.spawn_all(scope, ready);
                        }
                    }
                });
            });
            if let Err(e) = read {
                run.fail(e);
            }
        });
        run.first_error
            .into_inner()
            .unwrap_or_else(PoisonError::into_inner)
            .map_or(Ok(()), Err)
    }

    /// The file's blocks in file order, decoded ahead on the pool and handed out on the calling thread. The consumer
    /// works on one thread, so this is slower than [`par_unordered`](Self::par_unordered) when it does much per
    /// block.
    pub fn par_ordered(&self, select: Select) -> Result<ParOrdered> {
        ParOrdered::start(self.path.clone(), select, self.pool()?)
    }

    fn pool(&self) -> Result<Arc<ThreadPool>> {
        match &self.pool {
            Some(pool) => Ok(Arc::clone(pool)),
            None => default_pool(),
        }
    }
}

/// The pool shared by every reader not given one, built on first use.
fn default_pool() -> Result<Arc<ThreadPool>> {
    static POOL: Mutex<Option<Arc<ThreadPool>>> = Mutex::new(None);
    let mut shared = lock(&POOL);
    if let Some(pool) = &*shared {
        return Ok(Arc::clone(pool));
    }
    let cores = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(cores.saturating_sub(2).max(1))
        .thread_name(|i| format!("osmflow-{i}"))
        .build()
        .map_err(|e| Error::ThreadPool(e.to_string()))?;
    Ok(Arc::clone(shared.insert(Arc::new(pool))))
}

/// A block decoded and waiting for its turn, with the bytes it holds.
type Ready = (usize, Block, Permit);

/// The shared state of [`PbfReader::par_unordered`].
struct Run<F> {
    f: F,
    /// The barriers between runs of one object type.
    gate: Mutex<Gate>,
    budget: Arc<Budget>,
    failed: AtomicBool,
    first_error: Mutex<Option<Error>>,
}

impl<F: Fn(Block) -> Result<()> + Sync> Run<F> {
    fn fail(&self, e: Error) {
        self.failed.store(true, Ordering::Relaxed);
        self.budget.close();
        lock(&self.first_error).get_or_insert(e);
    }

    /// Calls `f` for block `i`, then hands the blocks it was holding back to the pool.
    fn call<'s>(&'s self, scope: &rayon::Scope<'s>, i: usize, block: Block, permit: Permit) {
        if !self.failed.load(Ordering::Relaxed)
            && let Err(e) = (self.f)(block)
        {
            self.fail(e);
        }
        drop(permit);
        let ready = lock(&self.gate).finished(i);
        self.spawn_all(scope, ready);
    }

    /// Hands the blocks the gate released to the pool.
    fn spawn_all<'s>(&'s self, scope: &rayon::Scope<'s>, ready: Vec<Ready>) {
        for (i, block, permit) in ready {
            scope.spawn(move |scope| self.call(scope, i, block, permit));
        }
    }
}

/// The barriers between runs of blocks of one object type, for [`PbfReader::par_unordered`]. Blocks are numbered
/// in file order, and consecutive blocks holding the same types make a run. Block `i` may be handed to the callback
/// once every block up to it is decoded (so its run is known) and every block before its run has finished its
/// callback. `checked` counts the leading decoded blocks and `finished` the leading finished ones; both only grow.
#[derive(Default)]
struct Gate {
    /// The number of the first block in `blocks`; every block before it is finished.
    first: usize,
    blocks: std::collections::VecDeque<Entry>,
    checked: usize,
    finished: usize,
    /// The types of the run the checked blocks end with, and the number of its first block.
    run_types: u8,
    run_start: usize,
}

#[derive(Default)]
struct Entry {
    /// The types the block holds, once it is decoded.
    types: Option<u8>,
    /// The number of the first block of its run, once every block up to it is decoded.
    run_start: Option<usize>,
    finished: bool,
    /// The decoded block, while it waits for earlier blocks.
    waiting: Option<(Block, Permit)>,
}

/// The types `block` holds, of those selected, as bits.
fn types(block: &Block) -> u8 {
    u8::from(block.nodes().len() > 0)
        | u8::from(block.ways().len() > 0) << 1
        | u8::from(block.relations().len() > 0) << 2
}

impl Gate {
    /// Adds the next block of the file, being decoded.
    fn admit(&mut self) {
        self.blocks.push_back(Entry::default());
    }

    fn entry(&mut self, i: usize) -> &mut Entry {
        &mut self.blocks[i - self.first]
    }

    /// Block `i` is decoded: returns the blocks now ready, `i` itself if no earlier block holds it back.
    fn decoded(&mut self, i: usize, block: Block, permit: Permit) -> Vec<Ready> {
        let entry = self.entry(i);
        entry.types = Some(types(&block));
        entry.waiting = Some((block, permit));
        self.release()
    }

    /// Block `i`'s callback returned: returns the blocks now ready.
    fn finished(&mut self, i: usize) -> Vec<Ready> {
        self.entry(i).finished = true;
        self.release()
    }

    /// Finds the runs of the newly decoded leading blocks, moves the count of finished leading blocks forward, and
    /// takes every waiting block whose earlier runs are finished.
    fn release(&mut self) -> Vec<Ready> {
        while let Some(e) = self.blocks.get_mut(self.checked - self.first)
            && let Some(types) = e.types
        {
            // A block with none of the selected types joins whichever run it is in.
            if types != 0 && types != self.run_types {
                self.run_types = types;
                self.run_start = self.checked;
            }
            e.run_start = Some(self.run_start);
            self.checked += 1;
        }
        while self
            .blocks
            .get(self.finished - self.first)
            .is_some_and(|e| e.finished)
        {
            self.finished += 1;
        }
        let mut ready = Vec::new();
        for (k, e) in self.blocks.iter_mut().enumerate() {
            if e.run_start.is_some_and(|start| self.finished >= start)
                && let Some((block, permit)) = e.waiting.take()
            {
                ready.push((self.first + k, block, permit));
            }
        }
        while self.first < self.finished {
            self.blocks.pop_front();
            self.first += 1;
        }
        ready
    }
}

fn blobs(path: &Path) -> Result<BlobReader<BufReader<File>>> {
    Ok(BlobReader::new(BufReader::with_capacity(
        READ_BUFFER,
        File::open(path)?,
    )))
}

/// The file's data blobs, read but not decompressed.
fn data_blobs(path: &Path) -> Result<impl Iterator<Item = Result<RawBlob>> + use<>> {
    Ok(blobs(path)?.filter(|b| !matches!(b, Ok(blob) if blob.kind != BlobKind::Data)))
}

/// Reads the file's data blobs and hands each to `spawn` with its share of the budget, until the end of the file,
/// a read error, or the budget is closed.
fn admit(path: &Path, budget: &Arc<Budget>, mut spawn: impl FnMut(RawBlob, Permit)) -> Result<()> {
    for blob in data_blobs(path)? {
        let blob = blob?;
        let Some(permit) = budget.acquire(blob.content_len() * DECODED_FACTOR) else {
            break;
        };
        spawn(blob, permit);
    }
    Ok(())
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Bytes admitted into the pipeline: (bytes in use, closed). A blob larger than the whole limit is admitted alone.
struct Budget {
    limit: usize,
    state: Mutex<(usize, bool)>,
    changed: Condvar,
}

impl Budget {
    /// A budget that keeps every thread of `pool` busy.
    fn for_pool(pool: &ThreadPool) -> Arc<Self> {
        Self::new(
            (pool.current_num_threads() * BYTES_IN_FLIGHT_PER_THREAD).max(MIN_BYTES_IN_FLIGHT),
        )
    }

    fn new(limit: usize) -> Arc<Self> {
        Arc::new(Self {
            limit,
            state: Mutex::new((0, false)),
            changed: Condvar::new(),
        })
    }

    /// Waits until `cost` more bytes fit, and takes them until the permit is dropped; `None` once the budget is closed.
    fn acquire(self: &Arc<Self>, cost: usize) -> Option<Permit> {
        let mut state = self
            .changed
            .wait_while(lock(&self.state), |&mut (used, closed)| {
                !closed && used != 0 && used + cost > self.limit
            })
            .unwrap_or_else(PoisonError::into_inner);
        if state.1 {
            return None;
        }
        state.0 += cost;
        Some(Permit {
            budget: Arc::clone(self),
            cost,
        })
    }

    fn close(&self) {
        lock(&self.state).1 = true;
        self.changed.notify_all();
    }
}

/// Bytes taken from a [`Budget`], given back when dropped. A task that panics closes the budget, so the reader stops.
struct Permit {
    budget: Arc<Budget>,
    cost: usize,
}

impl Drop for Permit {
    fn drop(&mut self) {
        let mut state = lock(&self.budget.state);
        state.0 -= self.cost;
        state.1 |= std::thread::panicking();
        self.budget.changed.notify_all();
    }
}

/// A block's place in the output order, filled when it is decoded. The permit keeps its bytes counted until the
/// consumer takes it.
type Slot = Receiver<(Result<Block>, Option<Permit>)>;

/// Blocks in file order, decoded in parallel. See [`PbfReader::par_ordered`].
///
/// A reader thread reads the blobs and, for each, reserves a slot at the back of a queue, then has the pool decode
/// the blob into its slot. The iterator takes slots from the front, so blocks come out in file order however the
/// pool finishes them. Bytes in flight are bounded, so the reader waits when the consumer falls behind.
pub struct ParOrdered {
    slots: Receiver<Slot>,
    budget: Arc<Budget>,
    reader: Option<JoinHandle<()>>,
}

impl ParOrdered {
    fn start(path: PathBuf, select: Select, pool: Arc<ThreadPool>) -> Result<Self> {
        let budget = Budget::for_pool(&pool);
        // The budget bounds the queue of slots; this bound only keeps the channel itself small.
        let (slots_tx, slots) = sync_channel::<Slot>(1024);
        let reader_budget = Arc::clone(&budget);
        let reader = std::thread::Builder::new()
            .name("osmflow-reader".to_owned())
            .spawn(move || {
                let read = admit(&path, &reader_budget, |blob, permit| {
                    let (tx, slot) = sync_channel(1);
                    if slots_tx.send(slot).is_ok() {
                        pool.spawn(move || {
                            let _ = tx.send((blob.decode(select), Some(permit)));
                        });
                    }
                });
                if let Err(e) = read {
                    let (tx, slot) = sync_channel(1);
                    let _ = tx.send((Err(e), None));
                    let _ = slots_tx.send(slot);
                }
            })?;
        Ok(Self {
            slots,
            budget,
            reader: Some(reader),
        })
    }
}

impl Iterator for ParOrdered {
    type Item = Result<Block>;

    fn next(&mut self) -> Option<Self::Item> {
        let Ok(slot) = self.slots.recv() else {
            // The reader is done: re-raise its panic, if it had one, rather than end as if the file had.
            if let Some(Err(panic)) = self.reader.take().map(JoinHandle::join) {
                std::panic::resume_unwind(panic);
            }
            return None;
        };
        let (block, _permit) = slot.recv().ok()?;
        Some(block)
    }
}

impl Drop for ParOrdered {
    fn drop(&mut self) {
        // Stop the reader: wake it if it waits for the budget, and fail its next send by dropping the queue.
        self.budget.close();
        drop(std::mem::replace(&mut self.slots, sync_channel(0).1));
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}
