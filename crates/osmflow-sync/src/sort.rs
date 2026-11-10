//! Sorting more records than fit in memory: runs of [`RUN`] records are sorted and written to temporary files, then
//! merged.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read as _, Write as _};
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::{iter, mem, vec};

use crossbeam_channel::{Receiver, Sender, bounded};
use rayon::ThreadPoolBuilder;
use rayon::slice::ParallelSliceMut as _;

/// Records kept in memory before a run is written out: 1 GiB.
const RUN: usize = 1 << 26;

/// A record to sort, by its first number, then its second: a (node, way) pair of the reverse index, or a node and its
/// packed location.
pub(crate) type Record = (u64, u64);

/// Collects records from any thread; [`finish`](Self::finish) gives them sorted, without duplicates.
///
/// A full run goes to a writer thread, which sorts it on a thread pool of its own and writes it out. The threads that
/// add records may be the global pool's, as an import's are: a thread waiting there for a sort could take up their
/// jobs, which add records, and the global pool runs its own queued jobs before a sort handed to it. At most three
/// runs are in memory: the one being written, a full one being handed over, and the next.
pub(crate) struct Sorter {
    run_size: usize,
    run: Mutex<Vec<Record>>,
    /// Held while a full run is handed to the writer, which takes one when it has written the one before.
    to_writer: Mutex<Sender<Vec<Record>>>,
    writer: JoinHandle<Vec<File>>,
    /// The writer's error, which stops it.
    failed: Arc<Mutex<Option<io::Error>>>,
}

impl Sorter {
    /// A sorter whose runs go into unnamed temporary files in `dir`.
    pub(crate) fn new(dir: &Path) -> io::Result<Self> {
        Self::with_run(dir, RUN)
    }

    fn with_run(dir: &Path, run_size: usize) -> io::Result<Self> {
        let (tx, rx) = bounded(0);
        let failed = Arc::new(Mutex::new(None));
        let (dir, slot) = (dir.to_path_buf(), Arc::clone(&failed));
        let writer = thread::Builder::new()
            .name("osmflow-sort".to_owned())
            .spawn(move || {
                write_runs(&dir, &rx).unwrap_or_else(|e| {
                    *slot.lock().unwrap_or_else(PoisonError::into_inner) = Some(e);
                    Vec::new()
                })
            })?;
        Ok(Self {
            run_size,
            run: Mutex::new(Vec::new()),
            to_writer: Mutex::new(tx),
            writer,
            failed,
        })
    }

    /// Adds `records`. A full run is handed to the writer, while other threads go on adding to the next; a thread
    /// that fills that one too waits for the writer to take it.
    pub(crate) fn extend(&self, records: impl IntoIterator<Item = Record>) -> io::Result<()> {
        if !self.add(records) {
            return Ok(());
        }
        let tx = self
            .to_writer
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let full = {
            let mut run = self.run.lock().unwrap_or_else(PoisonError::into_inner);
            // Handed over by another thread while this one waited
            if run.len() < self.run_size {
                return Ok(());
            }
            // With room for what waiting threads add past a full run
            mem::replace(
                &mut *run,
                Vec::with_capacity(self.run_size + self.run_size / 16),
            )
        };
        tx.send(full).map_err(|_| self.error())
    }

    /// Adds `records` to the run; whether it is full.
    fn add(&self, records: impl IntoIterator<Item = Record>) -> bool {
        let mut run = self.run.lock().unwrap_or_else(PoisonError::into_inner);
        run.extend(records);
        run.len() >= self.run_size
    }

    /// The writer's error, once it stopped.
    fn error(&self) -> io::Error {
        let failed = self
            .failed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        failed.unwrap_or_else(|| io::Error::other("the thread writing sorted runs stopped"))
    }

    /// The records added, sorted, without duplicates.
    pub(crate) fn finish(self) -> io::Result<Sorted> {
        drop(self.to_writer);
        let files = self
            .writer
            .join()
            .map_err(|_| io::Error::other("the thread writing sorted runs panicked"))?;
        if let Some(e) = self
            .failed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            return Err(e);
        }
        let mut run = self
            .run
            .into_inner()
            .unwrap_or_else(PoisonError::into_inner);
        run.par_sort_unstable();
        run.dedup();
        let mut sources: Vec<Source> = files
            .into_iter()
            .map(|f| Source::File(BufReader::with_capacity(1 << 20, f)))
            .collect();
        sources.push(Source::Memory(run.into_iter()));
        let mut heap = BinaryHeap::new();
        for (i, s) in sources.iter_mut().enumerate() {
            if let Some(p) = s.next()? {
                heap.push(Reverse((p, i)));
            }
        }
        Ok(Sorted {
            sources,
            heap,
            last: None,
        })
    }
}

/// The writer: each run received sorted, on a pool made for the first, and written to a temporary file in `dir`.
fn write_runs(dir: &Path, runs: &Receiver<Vec<Record>>) -> io::Result<Vec<File>> {
    let mut pool = None;
    let mut files = Vec::new();
    for mut run in runs {
        if pool.is_none() {
            pool = Some(
                ThreadPoolBuilder::new()
                    .thread_name(|i| format!("osmflow-sort-{i}"))
                    .build()
                    .map_err(io::Error::other)?,
            );
        }
        if let Some(pool) = &pool {
            pool.install(|| run.par_sort_unstable());
        }
        run.dedup();
        let mut w = BufWriter::with_capacity(1 << 20, tempfile::tempfile_in(dir)?);
        for (a, b) in run {
            w.write_all(&a.to_le_bytes())?;
            w.write_all(&b.to_le_bytes())?;
        }
        let mut file = w.into_inner().map_err(io::IntoInnerError::into_error)?;
        io::Seek::rewind(&mut file)?;
        files.push(file);
    }
    Ok(files)
}

/// A sorted run: the one left in memory, or one written out.
enum Source {
    Memory(vec::IntoIter<Record>),
    File(BufReader<File>),
}

impl Source {
    fn next(&mut self) -> io::Result<Option<Record>> {
        match self {
            Self::Memory(it) => Ok(it.next()),
            Self::File(r) => {
                let mut b = [0; 16];
                match r.read_exact(&mut b) {
                    Ok(()) => {
                        let (a, c) = b.split_at(8);
                        let at = |s: &[u8]| u64::from_le_bytes(s.try_into().unwrap_or_default());
                        Ok(Some((at(a), at(c))))
                    }
                    Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => Ok(None),
                    Err(e) => Err(e),
                }
            }
        }
    }
}

/// The sorted records of a [`Sorter`].
pub(crate) struct Sorted {
    sources: Vec<Source>,
    heap: BinaryHeap<Reverse<(Record, usize)>>,
    last: Option<Record>,
}

impl Iterator for Sorted {
    type Item = io::Result<Record>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let Reverse((pair, i)) = self.heap.pop()?;
            match self.sources[i].next() {
                Ok(Some(p)) => self.heap.push(Reverse((p, i))),
                Ok(None) => {}
                Err(e) => return Some(Err(e)),
            }
            // Two runs can hold the same record
            if self.last != Some(pair) {
                self.last = Some(pair);
                return Some(Ok(pair));
            }
        }
    }
}

impl iter::FusedIterator for Sorted {}

#[cfg(test)]
mod tests {
    use rayon::prelude::*;

    use super::*;

    #[test]
    fn runs_from_the_thread_pool_merge_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let sorter = Sorter::with_run(dir.path(), 100).unwrap();
        // Rayon jobs add the same records, in chunks, as an import's do, so runs overlap and hold duplicates
        let records: Vec<Record> = (0..1000_u64).map(|i| (i * 7919 % 1000, i % 3)).collect();
        (0..8).into_par_iter().for_each(|_| {
            for chunk in records.chunks(7) {
                sorter.extend(chunk.iter().copied()).unwrap();
            }
        });
        let merged = sorter.finish().unwrap();
        assert!(merged.sources.len() > 50, "{}", merged.sources.len());
        let got: Vec<Record> = merged.map(Result::unwrap).collect();
        let mut expected = records;
        expected.sort_unstable();
        expected.dedup();
        assert_eq!(got, expected);
    }
}
