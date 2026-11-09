//! Opening, locking and syncing the files of a store.

use std::fs::{File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};
use std::{io, thread};

use memmap2::Mmap;

use super::{NodeStoreError, Result};

/// Adds the file path to I/O errors.
pub(crate) trait At<T> {
    fn at(self, path: &Path) -> Result<T>;
}

impl<T> At<T> for io::Result<T> {
    fn at(self, path: &Path) -> Result<T> {
        self.map_err(|source| NodeStoreError::Io {
            path: path.to_path_buf(),
            source,
        })
    }
}

/// The most a store's file holds before its header is written: a store grows its new file to one
/// header page, then writes the header, so a crash in between leaves this many zeros.
const UNWRITTEN_HEADER: u64 = 4096;

/// Opens `path` to read and write, creating it if it is missing, and locks it for this store
/// alone. Returns the file, and whether it is new: empty, or holding only the zeros of a store
/// whose creation stopped before its header was written. A header is never all zeros.
pub(crate) fn open_rw(path: &Path) -> Result<(File, bool)> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .at(path)?;
    lock(&file, path, false)?;
    let len = file.metadata().at(path)?.len();
    let new = len == 0
        || len <= UNWRITTEN_HEADER && {
            let mut start = vec![0; usize::try_from(len).unwrap_or(0)];
            read_at(&file, &mut start, 0).at(path)?;
            start.iter().all(|&b| b == 0)
        };
    Ok((file, new))
}

/// Opens an existing `path` to read, and locks it against writers.
#[allow(
    clippy::wildcard_enum_match_arm,
    reason = "every other I/O error is passed on"
)]
pub(crate) fn open_ro(path: &Path) -> Result<File> {
    let file = File::open(path).map_err(|e| match e.kind() {
        io::ErrorKind::NotFound => NodeStoreError::NotFound(path.to_path_buf()),
        _ => NodeStoreError::Io {
            path: path.to_path_buf(),
            source: e,
        },
    })?;
    lock(&file, path, true)?;
    Ok(file)
}

fn lock(file: &File, path: &Path, shared: bool) -> Result<()> {
    let res = if shared {
        file.try_lock_shared()
    } else {
        file.try_lock()
    };
    match res {
        Ok(()) => Ok(()),
        Err(TryLockError::WouldBlock) => Err(NodeStoreError::Locked(path.to_path_buf())),
        Err(TryLockError::Error(e)) => Err(e).at(path),
    }
}

/// Maps all of `file` for reading.
#[expect(
    unsafe_code,
    reason = "a memory map of a file is unsafe: the file may change under it"
)]
pub(crate) fn map_file(file: &File) -> io::Result<Mmap> {
    // SAFETY: a store writes to its file only through `&mut self`, so no slice of its map is alive
    // while the mapped bytes change, and it shrinks the file only after replacing the map. Other
    // stores keep to the file's lock. A process that ignores the lock, which is only advisory on
    // Unix, and changes or truncates the file could still break the map, which no memory map can
    // prevent.
    unsafe { Mmap::map(file) }
}

/// The path of a file kept next to the store: `<path>.<ext>`.
pub(crate) fn sidecar(path: &Path, ext: &str) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".");
    s.push(ext);
    PathBuf::from(s)
}

/// Syncs the directory holding `path`, so a file created or renamed in it is kept. Windows keeps directory entries
/// with the files, so there is nothing to sync there.
#[cfg_attr(
    not(unix),
    expect(
        clippy::unnecessary_wraps,
        reason = "only Unix syncs directories, which can fail"
    )
)]
pub(crate) fn sync_dir(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        let dir = match path.parent() {
            Some(d) if !d.as_os_str().is_empty() => d,
            _ => Path::new("."),
        };
        File::open(dir).and_then(|d| d.sync_all()).at(dir)?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

/// Writes all of `buf` at `offset` of `file`.
pub(crate) fn write_at(file: &File, buf: &[u8], offset: u64) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileExt as _;
        file.write_all_at(buf, offset)
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileExt as _;
        let mut done = 0;
        while done < buf.len() {
            match file.seek_write(&buf[done..], offset + done as u64) {
                Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                Ok(n) => done += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
}

/// Reads all of `buf` from `offset` of `file`.
pub(crate) fn read_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileExt as _;
        file.read_exact_at(buf, offset)
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileExt as _;
        let mut done = 0;
        while done < buf.len() {
            match file.seek_read(&mut buf[done..], offset + done as u64) {
                Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
                Ok(n) => done += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
}

/// The CRC-32 of `bytes`, which a store keeps to find damaged data.
pub(crate) fn crc(bytes: &[u8]) -> u32 {
    crc32fast::hash(bytes)
}

/// Reads all of `file`, the store's file at `path`, into the page cache, with as many threads as
/// cores reading chunks of it in turn: the OS's own read-ahead keeps too few requests in flight for
/// one reader to get the most from a network disk. It reads through the store's own handle, as
/// Windows locks a file against reads through any other while a writer holds it.
pub(crate) fn preload(file: &File, path: &Path) -> Result<()> {
    const CHUNK: u64 = 64 << 20;
    let len = file.metadata().at(path)?.len();
    let threads = thread::available_parallelism().map_or(4, usize::from) as u64;
    thread::scope(|s| {
        let readers: Vec<_> = (0..threads)
            .map(|t| {
                s.spawn(move || -> io::Result<()> {
                    let mut buf = vec![0; 1 << 20];
                    let mut chunk = t * CHUNK;
                    while chunk < len {
                        let end = (chunk + CHUNK).min(len);
                        let mut at = chunk;
                        while at < end {
                            let n = usize::try_from((end - at).min(buf.len() as u64)).unwrap();
                            read_at(file, &mut buf[..n], at)?;
                            at += n as u64;
                        }
                        chunk += threads * CHUNK;
                    }
                    Ok(())
                })
            })
            .collect();
        readers
            .into_iter()
            .try_for_each(|r| r.join().expect("preload thread panicked"))
            .at(path)
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use std::path::PathBuf;
    use std::{env, fs, process};

    /// A path in the temp directory, removed when dropped with the files kept next to it, unless
    /// `KEEP_TEST_FILES` is set at build time.
    pub(crate) struct TestFile(pub PathBuf);

    impl TestFile {
        /// `name` may hold a module path: its `:`, which Windows does not allow in file names, becomes `-`.
        pub(crate) fn new(name: &str) -> Self {
            let name = name.replace(':', "-");
            let file =
                Self(env::temp_dir().join(format!("osmflow-nodes-{}-{name}", process::id())));
            file.remove();
            file
        }

        fn remove(&self) {
            let _ = fs::remove_file(&self.0);
            let _ = fs::remove_dir_all(&self.0);
            for ext in ["dir", "undo"] {
                let _ = fs::remove_file(super::sidecar(&self.0, ext));
            }
        }
    }

    impl Drop for TestFile {
        fn drop(&mut self) {
            if option_env!("KEEP_TEST_FILES").is_none() {
                self.remove();
            }
        }
    }
}
