//! Replication streams: state files, the paths of sequence numbers, and reading diffs in order from a local directory
//! or a server.

use std::fs::{self, File};
use std::io::{self, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, sync_channel};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;
use std::{process, thread};

use flate2::bufread::MultiGzDecoder;

use crate::osc::error::{OscError, Result, StateError};
use crate::osc::value::timestamp_ms;
use crate::osc::{Change, OscReader};

/// How often a downloaded file that fails its check (a state file that does not parse, a diff that is not complete
/// gzip) is fetched, and the pause between tries.
const CHECKED_TRIES: u32 = 3;
const CHECK_RETRY_PAUSE: Duration = Duration::from_secs(1);

/// A replication state: the sequence number of a stream's latest diff, or of one diff, and the time it covers
/// changes up to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct State {
    /// The diff's number in its stream; diff `n` is at the path [`sequence_path`]`(n)`.
    pub sequence: u64,
    /// In milliseconds since 1970.
    pub timestamp_ms: i64,
}

impl State {
    /// Reads a state file.
    pub fn read(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let text = fs::read_to_string(path).map_err(|source| OscError::File {
            path: path.to_owned(),
            source,
        })?;
        text.parse().map_err(|kind| OscError::State {
            origin: path.display().to_string(),
            kind,
        })
    }
}

impl FromStr for State {
    type Err = StateError;

    /// Parses a state file: Java properties, of which `sequenceNumber` and `timestamp` are required and the rest is
    /// ignored. Comment lines (`#` or `!`) are skipped, and a backslash escapes the character after it (the
    /// timestamp's colons are written `\:`).
    fn from_str(text: &str) -> Result<Self, StateError> {
        let (mut sequence, mut timestamp) = (None, None);
        for line in text.lines() {
            let line = line.trim();
            if line.starts_with(['#', '!']) {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let value = unescape(value.trim());
            let bad = |key| StateError::Value {
                key,
                value: value.clone(),
            };
            match key.trim() {
                "sequenceNumber" => {
                    sequence = Some(value.parse().map_err(|_| bad("sequenceNumber"))?);
                }
                "timestamp" => {
                    timestamp =
                        Some(timestamp_ms(value.as_bytes()).ok_or_else(|| bad("timestamp"))?);
                }
                _ => {}
            }
        }
        Ok(Self {
            sequence: sequence.ok_or(StateError::Missing("sequenceNumber"))?,
            timestamp_ms: timestamp.ok_or(StateError::Missing("timestamp"))?,
        })
    }
}

/// A properties value with each backslash escape replaced by the character it escapes.
fn unescape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        out.push(if c == '\\' {
            chars.next().unwrap_or('\\')
        } else {
            c
        });
    }
    out
}

/// The path of a sequence number in a replication stream, without its extension: `AAA/BBB/CCC`, its digits split in
/// groups of three, as in `007/363/600` for 7363600. Add `.osc.gz` for the diff, `.state.txt` for its state.
/// Sequences from a billion on get a longer first group.
#[must_use]
pub fn sequence_path(sequence: u64) -> String {
    format!(
        "{:03}/{:03}/{:03}",
        sequence / 1_000_000,
        sequence / 1000 % 1000,
        sequence % 1000
    )
}

/// The sequence number of a path in a replication stream: its last three components, as [`sequence_path`] makes
/// them, with or without `.osc.gz` or `.state.txt`. Anything before them (a directory or a URL) is ignored.
#[must_use]
pub fn path_sequence(path: &str) -> Option<u64> {
    let path = path
        .strip_suffix(".osc.gz")
        .or_else(|| path.strip_suffix(".state.txt"))
        .unwrap_or(path);
    let mut parts = path.rsplit('/');
    let (c, b, a) = (parts.next()?, parts.next()?, parts.next()?);
    let digits = |s: &str| s.len() >= 3 && s.bytes().all(|d| d.is_ascii_digit());
    if !digits(a)
        || b.len() != 3
        || c.len() != 3
        || !digits(b)
        || !digits(c)
        || (a.len() > 3 && a.starts_with('0'))
    {
        return None;
    }
    let (a, b, c): (u64, u64, u64) = (a.parse().ok()?, b.parse().ok()?, c.parse().ok()?);
    a.checked_mul(1_000_000)?.checked_add(b * 1000 + c)
}

/// Gets files from a replication server.
pub trait Fetch: Send + Sync {
    /// The body of `url`, or `None` if the server has no such file (HTTP 404). Retrying failed requests is up to the
    /// implementation.
    fn fetch(&self, url: &str) -> io::Result<Option<Vec<u8>>>;
}

/// One decoded diff of a replication stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diff {
    /// The diff's own state file: its sequence number, and the time it covers changes up to.
    pub state: State,
    /// The diff's changes, in file order.
    pub changes: Vec<Change>,
}

/// A replication stream, read from a local directory laid out as the servers lay theirs out (`state.txt` and
/// `AAA/BBB/CCC.osc.gz` with `AAA/BBB/CCC.state.txt`), or from a server through such a directory used as a cache.
///
/// From a server, every file is downloaded once into the cache, checked (a state file must parse and name its
/// sequence, a diff must be complete gzip), written to a temporary file and renamed into place, so the cache only
/// ever holds whole files and is never downloaded again. Only the latest state, `state.txt`, is fetched anew each time
/// [`state`](Self::state) is called. Nothing past the server's latest state is downloaded, even if it exists. A cache
/// can later be read as a local directory.
pub struct Replication {
    source: Arc<Source>,
    read_ahead: usize,
}

struct Source {
    dir: PathBuf,
    remote: Option<Remote>,
}

struct Remote {
    /// The base URL, without a trailing slash.
    base: String,
    fetch: Box<dyn Fetch>,
    /// The latest sequence the server has published, as last read.
    latest: Mutex<Option<u64>>,
}

impl Replication {
    /// Reads a stream from a local directory.
    pub fn local(dir: impl Into<PathBuf>) -> Self {
        Self::new(dir.into(), None)
    }

    /// Reads a stream from the server at `base_url` (such as `https://planet.openstreetmap.org/replication/minute`)
    /// with `fetch`, through the cache directory `cache_dir`.
    pub fn remote(
        base_url: &str,
        cache_dir: impl Into<PathBuf>,
        fetch: impl Fetch + 'static,
    ) -> Self {
        let remote = Remote {
            base: base_url.trim_end_matches('/').to_owned(),
            fetch: Box::new(fetch),
            latest: Mutex::new(None),
        };
        Self::new(cache_dir.into(), Some(remote))
    }

    /// Reads a stream from the server at `base_url` over HTTP, with [`HttpFetch`](crate::osc::HttpFetch)'s defaults,
    /// through the cache directory `cache_dir`.
    #[cfg(feature = "http")]
    pub fn http(base_url: &str, cache_dir: impl Into<PathBuf>) -> Self {
        use crate::osc::HttpFetch;
        Self::remote(base_url, cache_dir, HttpFetch::new())
    }

    fn new(dir: PathBuf, remote: Option<Remote>) -> Self {
        let read_ahead = thread::available_parallelism().map_or(2, |n| n.get().clamp(2, 4));
        let source = Arc::new(Source { dir, remote });
        Self { source, read_ahead }
    }

    /// Sets how many diffs [`diffs`](Self::diffs) decodes ahead of the one being read, at least 1; by default, the
    /// number of cores, from 2 to 4. Each decoded diff is held in memory whole: about 1 GB for a day diff of the
    /// planet, against a few MB for a minute diff.
    #[must_use]
    pub fn with_read_ahead(mut self, diffs: usize) -> Self {
        self.read_ahead = diffs.max(1);
        self
    }

    /// The local directory: the source, or the cache.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.source.dir
    }

    /// The stream's latest state: the local `state.txt`, or the server's, fetched now and saved in the cache.
    pub fn state(&self) -> Result<State> {
        self.source.latest()
    }

    /// The state file of one diff, downloaded first if needed.
    pub fn sequence_state(&self, sequence: u64) -> Result<State> {
        self.source.state_file(sequence)
    }

    /// The local path of one diff, downloaded first if needed.
    pub fn diff_file(&self, sequence: u64) -> Result<PathBuf> {
        self.source.diff_file(sequence)
    }

    /// One diff, with its state, downloaded first if needed and decoded on the calling thread.
    pub fn diff(&self, sequence: u64) -> Result<Diff> {
        self.source.diff(sequence)
    }

    /// The diffs from `start` up to the latest state (none if `start` is past it), in order. Each diff is downloaded
    /// in turn, one at a time, on a background thread, and decoded on a thread of its own, up to
    /// [`with_read_ahead`](Self::with_read_ahead) diffs ahead of the one being read. The iterator ends after an error.
    pub fn diffs(&self, start: u64) -> Result<Diffs> {
        let latest = self.state()?.sequence;
        let source = Arc::clone(&self.source);
        let (tx, rx) = sync_channel(self.read_ahead - 1);
        spawn(move || {
            for sequence in start..=latest {
                let (diff_tx, diff_rx) = sync_channel(1);
                // A failed download is handed out in its turn; a thread that cannot start drops its sender, which
                // the iterator reports.
                let failed = match source.prepare(sequence) {
                    Ok(()) => {
                        let source = Arc::clone(&source);
                        spawn(move || {
                            // The receiver is gone if the iterator was dropped, and then the diff is not needed.
                            let _ = diff_tx.send(source.diff(sequence));
                        })
                        .is_err()
                    }
                    Err(e) => {
                        let _ = diff_tx.send(Err(e));
                        true
                    }
                };
                if tx.send(diff_rx).is_err() || failed {
                    return;
                }
            }
        })?;
        Ok(Diffs { rx, done: false })
    }
}

fn spawn(f: impl FnOnce() + Send + 'static) -> Result<()> {
    thread::Builder::new()
        .name("osmflow-diffs".to_owned())
        .spawn(f)?;
    Ok(())
}

/// The diffs of a replication stream, in order; see [`Replication::diffs`].
pub struct Diffs {
    rx: Receiver<Receiver<Result<Diff>>>,
    done: bool,
}

impl Iterator for Diffs {
    type Item = Result<Diff>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        let diff = self
            .rx
            .recv()
            .ok()?
            .recv()
            .unwrap_or(Err(OscError::ReadAhead));
        self.done = diff.is_err();
        Some(diff)
    }
}

impl Source {
    fn path(&self, sequence: u64, extension: &str) -> (String, PathBuf) {
        let relative = format!("{}{extension}", sequence_path(sequence));
        let path = self.dir.join(&relative);
        (relative, path)
    }

    fn latest(&self) -> Result<State> {
        let path = self.dir.join("state.txt");
        let Some(remote) = &self.remote else {
            return State::read(&path);
        };
        let (_, body, state) = remote.fetch_checked("state.txt", parse_state)?;
        write_atomic(&path, &body)?;
        *remote.latest.lock().unwrap_or_else(PoisonError::into_inner) = Some(state.sequence);
        Ok(state)
    }

    /// Downloads a diff and its state file into the cache, unless they are there already; for a local source, only
    /// reads the state file.
    fn prepare(&self, sequence: u64) -> Result<()> {
        self.state_file(sequence)?;
        self.diff_file(sequence)?;
        Ok(())
    }

    fn state_file(&self, sequence: u64) -> Result<State> {
        let (relative, path) = self.path(sequence, ".state.txt");
        match &self.remote {
            Some(remote) if !path.exists() => {
                self.check_published(remote, sequence)?;
                let (url, body, state) = remote.fetch_checked(&relative, parse_state)?;
                if state.sequence != sequence {
                    return Err(OscError::State {
                        origin: url,
                        kind: StateError::Value {
                            key: "sequenceNumber",
                            value: state.sequence.to_string(),
                        },
                    });
                }
                write_atomic(&path, &body)?;
                Ok(state)
            }
            _ => State::read(&path),
        }
    }

    fn diff_file(&self, sequence: u64) -> Result<PathBuf> {
        let (relative, path) = self.path(sequence, ".osc.gz");
        if let Some(remote) = &self.remote
            && !path.exists()
        {
            self.check_published(remote, sequence)?;
            let (_, body, ()) = remote.fetch_checked(&relative, check_gzip)?;
            write_atomic(&path, &body)?;
        }
        Ok(path)
    }

    fn diff(&self, sequence: u64) -> Result<Diff> {
        let state = self.state_file(sequence)?;
        let changes = OscReader::open(self.diff_file(sequence)?)?.collect::<Result<_>>()?;
        Ok(Diff { state, changes })
    }

    /// Fails unless the server has published `sequence`, by the latest state read, or read now if that is older.
    fn check_published(&self, remote: &Remote, sequence: u64) -> Result<()> {
        let known = *remote.latest.lock().unwrap_or_else(PoisonError::into_inner);
        let latest = match known {
            Some(latest) if latest >= sequence => latest,
            _ => self.latest()?.sequence,
        };
        if sequence > latest {
            return Err(OscError::NotPublished { sequence, latest });
        }
        Ok(())
    }
}

impl Remote {
    /// Fetches the file at `relative`, and checks it, fetching it again a few times while the check fails. Returns
    /// the URL, the body, and what the check returned.
    fn fetch_checked<T>(
        &self,
        relative: &str,
        check: impl Fn(&str, &[u8]) -> Result<T>,
    ) -> Result<(String, Vec<u8>, T)> {
        let url = format!("{}/{relative}", self.base);
        let mut tries = 0;
        loop {
            let body = match self.fetch.fetch(&url) {
                Ok(Some(body)) => body,
                Ok(None) => return Err(OscError::NotFound { url }),
                Err(source) => return Err(OscError::Fetch { url, source }),
            };
            tries += 1;
            match check(&url, &body) {
                Ok(checked) => return Ok((url, body, checked)),
                Err(e) if tries >= CHECKED_TRIES => return Err(e),
                Err(_) => thread::sleep(CHECK_RETRY_PAUSE),
            }
        }
    }
}

fn parse_state(url: &str, body: &[u8]) -> Result<State> {
    let error = |kind| OscError::State {
        origin: url.to_owned(),
        kind,
    };
    let text = str::from_utf8(body).map_err(|_| {
        error(StateError::Value {
            key: "file",
            value: "not UTF-8".to_owned(),
        })
    })?;
    text.parse().map_err(error)
}

/// Checks that a body is complete gzip: every member decompresses, and its trailer matches. A downloaded diff is so
/// decompressed once to be checked, before it is cached and parsed.
fn check_gzip(url: &str, body: &[u8]) -> Result<()> {
    let mut decoder = MultiGzDecoder::new(body);
    let mut buf = vec![0; 1 << 16];
    loop {
        match decoder.read(&mut buf) {
            Ok(0) => return Ok(()),
            Ok(_) => {}
            Err(source) => {
                return Err(OscError::Fetch {
                    url: url.to_owned(),
                    source,
                });
            }
        }
    }
}

/// Writes a file whole or not at all: to a temporary file beside it, synced, then renamed into place.
fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    // Numbers the temporary files, so that writers of one file in one process never share one.
    static COUNT: AtomicU64 = AtomicU64::new(0);
    let file_error = |path: &Path| {
        let path = path.to_owned();
        move |source| OscError::File { path, source }
    };
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(file_error(dir))?;
    }
    let mut name = path.file_name().unwrap_or_default().to_owned();
    name.push(format!(
        ".{}-{}.tmp",
        process::id(),
        COUNT.fetch_add(1, Ordering::Relaxed)
    ));
    let temp = path.with_file_name(name);
    let written = File::create(&temp).and_then(|mut file| {
        file.write_all(data)?;
        file.sync_all()
    });
    written
        .and_then(|()| fs::rename(&temp, path))
        .map_err(|source| {
            let _ = fs::remove_file(&temp);
            OscError::File {
                path: path.to_owned(),
                source,
            }
        })
}
