//! Reading a whole file: its header, then its blocks, one after another or on a pool of threads.

use std::collections::VecDeque;
use std::fs::File;
use std::marker::PhantomData;
use std::num::NonZero;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI8, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, TryRecvError, sync_channel};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::Duration;
use std::{mem, panic};

use rayon::{ThreadPool, Yield};

use crate::blob::{BlobKind, BlobReader, DataBlobs, Loader, RawBlob, ReadMode, Want};
use crate::block::{Block, Fields, Select, TagMode};
use crate::error::{PbfError, Result};
use crate::filter::{BlockStrings, Filters};
use crate::header::Header;

/// The limit of bytes in flight between the reader and the consumer (compressed blobs, decoded or not): this much per
/// pool thread, and at least the minimum.
const BYTES_IN_FLIGHT_PER_THREAD: usize = 16 << 20;
const MIN_BYTES_IN_FLIGHT: usize = 256 << 20;
/// A decoded block takes a few times its uncompressed size; this is the factor counted against the limit. Counting
/// the declared uncompressed size, not the compressed one, keeps a highly compressible blob from being admitted at a
/// fraction of the memory it decodes to.
const DECODED_FACTOR: usize = 3;
/// How long a thread of the decoding pool waits for another to finish its work, when it has none of its own to run.
const POOL_WAIT: Duration = Duration::from_millis(1);

/// An OSM PBF file, with its header read.
#[derive(Debug, Clone)]
pub struct PbfReader {
    path: PathBuf,
    header: Header,
    /// Where the parallel readers decode blocks; `None` for the shared default pool.
    pool: Option<Arc<ThreadPool>>,
    filters: Filters,
    mode: ReadMode,
}

impl PbfReader {
    /// Opens a file and reads its header, its first blob. A file that starts with a data blob instead, which the
    /// format does not allow but some tools write, gets an empty [`Header`]; an empty file is an error.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let Some(blob) = BlobReader::open(&path)?
            .next_with(|_, _| Want::WHOLE)
            .transpose()?
        else {
            return Err(PbfError::EmptyFile);
        };
        let header = match &blob.kind {
            BlobKind::Header => blob.with_content(|data| Header::parse(data, blob.position))?,
            BlobKind::Data => Header::default(),
            BlobKind::Other(kind) => return Err(PbfError::MissingHeader(kind.clone())),
        };
        Ok(Self {
            path,
            header,
            pool: None,
            filters: Filters::default(),
            mode: ReadMode::default(),
        })
    }

    /// How a read with filters reads the blobs it may skip; see [`ReadMode`]. The default suits most storage.
    #[must_use]
    pub fn read_mode(mut self, mode: ReadMode) -> Self {
        self.mode = mode;
        self
    }

    /// Skips the nodes of every block whose strings `keep` rejects: as no object of such a block uses those strings,
    /// none of its nodes can be wanted. Its nodes are not decoded, and if the block holds nothing else, it is not even
    /// inflated past its string table, when the file proves that: the block has a single group, or the file is
    /// sorted by type and the next block also starts with nodes. Blocks with nodes that `keep` accepts are read
    /// whole, so their nodes still need checking one by one.
    ///
    /// [`BlockStrings`] says what such a test can know: only that a block might hold a wanted object.
    #[must_use]
    pub fn nodes_where(
        mut self,
        keep: impl Fn(&BlockStrings<'_>) -> bool + Send + Sync + 'static,
    ) -> Self {
        self.filters.nodes = Some(Arc::new(keep));
        self
    }

    /// Skips the ways of every block whose strings `keep` rejects; see [`nodes_where`](Self::nodes_where).
    ///
    /// ```
    /// use osmflow::{PbfReader, Select};
    ///
    /// fn roads(path: &std::path::Path) -> Result<(), osmflow::PbfError> {
    ///     let file = PbfReader::open(path)?
    ///         .ways_where(|strings| strings.has("highway"))
    ///         .relations_where(|_| false);
    ///     file.par_unordered(Select::all(), |block| {
    ///         let Some(highway) = block.tag_key("highway") else {
    ///             return Ok(()); // a block of nodes, or of ways without roads
    ///         };
    ///         for way in block.ways() {
    ///             if let Some(kind) = way.tag(highway) { /* a road */ }
    ///         }
    ///         Ok(())
    ///     })
    /// }
    /// ```
    #[must_use]
    pub fn ways_where(
        mut self,
        keep: impl Fn(&BlockStrings<'_>) -> bool + Send + Sync + 'static,
    ) -> Self {
        self.filters.ways = Some(Arc::new(keep));
        self
    }

    /// Skips the relations of every block whose strings `keep` rejects; see [`nodes_where`](Self::nodes_where).
    #[must_use]
    pub fn relations_where(
        mut self,
        keep: impl Fn(&BlockStrings<'_>) -> bool + Send + Sync + 'static,
    ) -> Self {
        self.filters.relations = Some(Arc::new(keep));
        self
    }

    /// Decodes blocks on `pool` in the parallel readers, instead of the default: one pool shared by every reader, with
    /// two threads fewer than the cores (one is left for reading the file, one for the consumer).
    #[must_use]
    pub fn with_pool(mut self, pool: Arc<ThreadPool>) -> Self {
        self.pool = Some(pool);
        self
    }

    /// The same file, without the filters, which some reads must not have.
    #[cfg(feature = "nodes")]
    pub(crate) fn without_filters(&self) -> Self {
        Self {
            filters: Filters::default(),
            ..self.clone()
        }
    }

    /// The file's header.
    #[must_use]
    pub fn header(&self) -> &Header {
        &self.header
    }

    /// The file's blocks in file order, each decoded on the calling thread. Blocks the filters skip whole are left
    /// out.
    pub fn blocks<T: TagMode>(
        &self,
        select: Select<T>,
    ) -> Result<impl Iterator<Item = Result<Block<T>>> + use<T>> {
        let loader = self.loader(select.fields())?;
        Ok(
            DataBlobs::new(&self.path, Arc::clone(&loader))?.filter_map(move |blob| {
                let block = blob.and_then(|blob| loader.load(&blob));
                block.map(|b| b.map(Block::with_mode)).transpose()
            }),
        )
    }

    /// How a read with `select` loads its blocks.
    fn loader(&self, select: Fields) -> Result<Arc<Loader>> {
        Ok(Arc::new(Loader {
            file: File::open(&self.path)?,
            select,
            filters: self.filters.clone(),
            sorted: self.header.is_sorted(),
            mode: self.mode,
            kept: AtomicI8::new(0),
        }))
    }

    /// Calls `f` for every block on the pool. Consecutive blocks holding the same object types form a run: a run's
    /// blocks are handed to `f` in parallel and in no particular order, and a run starts only once `f` has returned for
    /// every block before it.
    ///
    /// In a file sorted by type, as published files are, every node is so handled before any way, and every way before
    /// any relation, as resolving way geometry needs. Blocks are still read and decoded ahead while a run's last blocks
    /// finish, so a barrier costs only that short tail. A file whose types come back (nodes again after ways) gets a
    /// barrier at every change, and one whose types alternate block by block is read nearly one block at a time.
    ///
    /// A block mixing types is a run of its own, a block holding none of the selected types joins the run it is in, and
    /// a block the filters skip whole is not handed to `f`. Stops at the first error, from reading or from `f`.
    pub fn par_unordered<T, F>(&self, select: Select<T>, f: F) -> Result<()>
    where
        T: TagMode,
        F: Fn(Block<T>) -> Result<()> + Sync,
    {
        let pool = self.pool()?;
        let loader = self.loader(select.fields())?;
        let run = Run {
            f: |block: Block| f(block.with_mode()),
            gate: Mutex::new(Gate::default()),
            budget: Budget::for_pool(&pool),
            failed: AtomicBool::new(false),
            first_error: Mutex::new(None),
        };
        pool.in_place_scope(|scope| {
            let run = &run;
            let mut index = 0;
            let loader = &loader;
            let read = admit(&self.path, loader, &run.budget, &pool, |blob, permit| {
                let i = index;
                index += 1;
                lock(&run.gate).admit();
                scope.spawn(move |scope| {
                    if run.failed.load(Ordering::Relaxed) {
                        return;
                    }
                    match loader.load(&blob) {
                        Err(e) => run.fail(e),
                        Ok(Some(block)) => {
                            let ready = lock(&run.gate).decoded(i, block, permit);
                            run.spawn_all(scope, ready);
                        }
                        Ok(None) => {
                            drop(permit);
                            let ready = lock(&run.gate).skipped(i);
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
    /// block. Way refs are decoded as they are iterated, so reading them here decodes them on the calling thread.
    // Ordered reads are rare. If they come to need refs, an option could decode refs on the pool for them.
    pub fn par_ordered<T: TagMode>(&self, select: Select<T>) -> Result<ParOrdered<T>> {
        ParOrdered::start(
            self.path.clone(),
            self.loader(select.fields())?,
            self.pool()?,
        )
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
    let cores = thread::available_parallelism().map_or(1, NonZero::get);
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(cores.saturating_sub(2).max(1))
        .thread_name(|i| format!("osmflow-{i}"))
        .build()
        .map_err(|e| PbfError::ThreadPool(e.to_string()))?;
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
    first_error: Mutex<Option<PbfError>>,
}

impl<F: Fn(Block) -> Result<()> + Sync> Run<F> {
    fn fail(&self, e: PbfError) {
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
    blocks: VecDeque<Entry>,
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

    /// Block `i` was skipped whole by the filters: it has no callback, and joins the run it is in. Returns the blocks
    /// now ready.
    fn skipped(&mut self, i: usize) -> Vec<Ready> {
        let entry = self.entry(i);
        entry.types = Some(0);
        entry.finished = true;
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

/// Reads the file's data blobs and hands each to `spawn` with its share of the budget, until the end of the file,
/// a read error, or the budget is closed.
fn admit(
    path: &Path,
    loader: &Arc<Loader>,
    budget: &Arc<Budget>,
    pool: &ThreadPool,
    mut spawn: impl FnMut(RawBlob, Permit),
) -> Result<()> {
    for blob in DataBlobs::new(path, Arc::clone(loader))? {
        let blob = blob?;
        let Some(permit) = budget.acquire(blob.content_len() * DECODED_FACTOR, pool) else {
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
    ///
    /// On a thread of `pool`, it runs the pool's work while it waits: that work returns the bytes, and blocking the
    /// thread could leave it no thread to run on (a one-thread pool, a read started from the pool's own work).
    fn acquire(self: &Arc<Self>, cost: usize, pool: &ThreadPool) -> Option<Permit> {
        let fits =
            |&(used, closed): &(usize, bool)| closed || used == 0 || used + cost <= self.limit;
        let on_pool = pool.current_thread_index().is_some();
        let mut state = lock(&self.state);
        while !fits(&state) {
            if on_pool {
                drop(state);
                if pool.yield_now() == Some(Yield::Idle) {
                    // No work to run here: it runs on other threads, which signal when they return bytes.
                    let wait = self.changed.wait_timeout(lock(&self.state), POOL_WAIT);
                    drop(wait.unwrap_or_else(PoisonError::into_inner));
                }
                state = lock(&self.state);
            } else {
                state = self
                    .changed
                    .wait(state)
                    .unwrap_or_else(PoisonError::into_inner);
            }
        }
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
        state.1 |= thread::panicking();
        self.budget.changed.notify_all();
    }
}

/// A block's place in the output order, filled when it is decoded (`None` if the filters skipped it whole). The
/// permit keeps its bytes counted until the consumer takes it.
type Slot = Receiver<(Result<Option<Block>>, Option<Permit>)>;

/// Blocks in file order, decoded in parallel. See [`PbfReader::par_ordered`].
///
/// A reader thread reads the blobs and, for each, reserves a slot at the back of a queue, then has the pool decode
/// the blob into its slot. The iterator takes slots from the front, so blocks come out in file order however the
/// pool finishes them. Bytes in flight are bounded, so the reader waits when the consumer falls behind.
pub struct ParOrdered<T: TagMode = crate::Tags> {
    slots: Receiver<Slot>,
    budget: Arc<Budget>,
    reader: Option<JoinHandle<()>>,
    /// The pool that decodes the blocks, whose work a consumer on one of its threads runs while it waits
    pool: Arc<ThreadPool>,
    tags: PhantomData<T>,
}

impl<T: TagMode> ParOrdered<T> {
    fn start(path: PathBuf, loader: Arc<Loader>, pool: Arc<ThreadPool>) -> Result<Self> {
        let budget = Budget::for_pool(&pool);
        // The budget bounds the queue of slots; this bound only keeps the channel itself small.
        let (slots_tx, slots) = sync_channel::<Slot>(1024);
        let (reader_budget, reader_pool) = (Arc::clone(&budget), Arc::clone(&pool));
        let reader = thread::Builder::new()
            .name("osmflow-reader".to_owned())
            .spawn(move || {
                let pool = reader_pool;
                let read = admit(&path, &loader, &reader_budget, &pool, |blob, permit| {
                    let (tx, slot) = sync_channel(1);
                    if slots_tx.send(slot).is_ok() {
                        let loader = Arc::clone(&loader);
                        pool.spawn(move || {
                            let _ = tx.send((loader.load(&blob), Some(permit)));
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
            pool,
            tags: PhantomData,
        })
    }

    /// Waits for a block. On a thread of the decoding pool, it runs the pool's work while it waits, as the block may
    /// be waiting for that very thread.
    fn wait_for(&self, slot: &Slot) -> Option<(Result<Option<Block>>, Option<Permit>)> {
        if self.pool.current_thread_index().is_none() {
            return slot.recv().ok();
        }
        loop {
            match slot.try_recv() {
                Ok(item) => return Some(item),
                Err(TryRecvError::Disconnected) => return None,
                Err(TryRecvError::Empty) => {
                    if self.pool.yield_now() == Some(Yield::Idle) {
                        match slot.recv_timeout(POOL_WAIT) {
                            Ok(item) => return Some(item),
                            Err(RecvTimeoutError::Disconnected) => return None,
                            Err(RecvTimeoutError::Timeout) => {}
                        }
                    }
                }
            }
        }
    }
}

impl<T: TagMode> Iterator for ParOrdered<T> {
    type Item = Result<Block<T>>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let Ok(slot) = self.slots.recv() else {
                // The reader is done: re-raise its panic, if it had one, rather than end as if the file had.
                if let Some(Err(panic)) = self.reader.take().map(JoinHandle::join) {
                    panic::resume_unwind(panic);
                }
                return None;
            };
            let (block, _permit) = self.wait_for(&slot)?;
            // A block the filters skipped whole has no place in the output.
            if let Some(block) = block.transpose() {
                return Some(block.map(Block::with_mode));
            }
        }
    }
}

impl<T: TagMode> Drop for ParOrdered<T> {
    fn drop(&mut self) {
        // Stop the reader: wake it if it waits for the budget, and fail its next send by dropping the queue.
        self.budget.close();
        drop(mem::replace(&mut self.slots, sync_channel(0).1));
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}
