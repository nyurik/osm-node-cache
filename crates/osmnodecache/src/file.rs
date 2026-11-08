//! Opening, locking and syncing the files of a store.

use std::fs::{File, OpenOptions, TryLockError};
use std::io;
use std::path::{Path, PathBuf};

use crate::{Error, Result};

/// Adds the file path to I/O errors.
pub(crate) trait At<T> {
    fn at(self, path: &Path) -> Result<T>;
}

impl<T> At<T> for io::Result<T> {
    fn at(self, path: &Path) -> Result<T> {
        self.map_err(|source| Error::Io {
            path: path.to_path_buf(),
            source,
        })
    }
}

/// Opens `path` to read and write, creating it if it is missing, and locks it for this store
/// alone. Returns the file, and whether it is new or empty.
pub(crate) fn open_rw(path: &Path) -> Result<(File, bool)> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .at(path)?;
    lock(&file, path, false)?;
    let empty = file.metadata().at(path)?.len() == 0;
    Ok((file, empty))
}

/// Opens an existing `path` to read, and locks it against writers.
#[allow(
    clippy::wildcard_enum_match_arm,
    reason = "every other I/O error is passed on"
)]
pub(crate) fn open_ro(path: &Path) -> Result<File> {
    let file = File::open(path).map_err(|e| match e.kind() {
        io::ErrorKind::NotFound => Error::NotFound(path.to_path_buf()),
        _ => Error::Io {
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
        Err(TryLockError::WouldBlock) => Err(Error::Locked(path.to_path_buf())),
        Err(TryLockError::Error(e)) => Err(e).at(path),
    }
}

/// The path of a file kept next to the store: `<path>.<ext>`.
pub(crate) fn sidecar(path: &Path, ext: &str) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".");
    s.push(ext);
    PathBuf::from(s)
}

/// Syncs the directory holding `path`, so a file created or renamed in it is kept.
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
        std::os::unix::fs::FileExt::write_all_at(file, buf, offset)
    }
    #[cfg(windows)]
    {
        let mut done = 0;
        while done < buf.len() {
            done += std::os::windows::fs::FileExt::seek_write(
                file,
                &buf[done..],
                offset + done as u64,
            )?;
        }
        Ok(())
    }
}

/// Reads all of `buf` from `offset` of `file`.
pub(crate) fn read_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::FileExt::read_exact_at(file, buf, offset)
    }
    #[cfg(windows)]
    {
        let mut done = 0;
        while done < buf.len() {
            let n = std::os::windows::fs::FileExt::seek_read(
                file,
                &mut buf[done..],
                offset + done as u64,
            )?;
            if n == 0 {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            done += n;
        }
        Ok(())
    }
}

/// The CRC-32 of `bytes`, which a store keeps to find damaged data.
#[cfg(any(feature = "paged", feature = "dense"))]
pub(crate) fn crc(bytes: &[u8]) -> u32 {
    crc32fast::hash(bytes)
}

/// Reads all of `path` into the page cache, with as many threads as cores reading chunks of it
/// in turn: the OS's own read-ahead keeps too few requests in flight for one reader to get the
/// most from a network disk.
pub(crate) fn preload(path: &Path) -> Result<()> {
    const CHUNK: u64 = 64 << 20;
    let file = File::open(path).at(path)?;
    let len = file.metadata().at(path)?.len();
    let threads = std::thread::available_parallelism().map_or(4, usize::from) as u64;
    std::thread::scope(|s| {
        let readers: Vec<_> = (0..threads)
            .map(|t| {
                let file = &file;
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
        pub(crate) fn new(name: &str) -> Self {
            let file = Self(env::temp_dir().join(format!("osmnodecache-{}-{name}", process::id())));
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
