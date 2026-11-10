//! What an input is, from its contents rather than its name: a PBF file, an osmChange file, a replication state file,
//! or a replication stream in a local directory or on a server.

use std::fs::{self, File};
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result, bail};
use osmflow::PbfReader;
use osmflow::osc::{
    Diffs, OscError, Replication, Start, State, StateSource, parse_timestamp, path_sequence, plan,
};

use crate::select::ChangeArgs;

/// What `INPUT` is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Input {
    /// Anything that is none of the others.
    Pbf(PathBuf),
    /// A file that is gzip, or starts with `<`.
    Osc(PathBuf),
    /// A file with a `sequenceNumber=` line.
    State(PathBuf),
    /// A directory, read as a replication stream laid out as the servers lay theirs out.
    Dir(PathBuf),
    /// An `http://` or `https://` URL of a replication server.
    Url(String),
}

/// The bytes of a file looked at to tell what it is: a state file is a few lines.
const SNIFF: u64 = 4096;

impl Input {
    pub fn of(path: &Path) -> Result<Self> {
        if let Some(url) = path
            .to_str()
            .filter(|p| p.starts_with("http://") || p.starts_with("https://"))
        {
            return Ok(Self::Url(url.trim_end_matches('/').to_owned()));
        }
        let cannot = || format!("cannot read {}", path.display());
        if fs::metadata(path).with_context(cannot)?.is_dir() {
            return Ok(Self::Dir(path.to_owned()));
        }
        let mut start = Vec::new();
        File::open(path)
            .and_then(|f| f.take(SNIFF).read_to_end(&mut start))
            .with_context(cannot)?;
        Ok(Self::sniff(path, &start))
    }

    /// What a file starting with `start` is.
    fn sniff(path: &Path, start: &[u8]) -> Self {
        let path = path.to_owned();
        if start.starts_with(&[0x1f, 0x8b]) || start.trim_ascii_start().starts_with(b"<") {
            Self::Osc(path)
        } else if start
            .split(|&b| b == b'\n')
            .any(|line| line.trim_ascii_start().starts_with(b"sequenceNumber"))
        {
            Self::State(path)
        } else {
            Self::Pbf(path)
        }
    }

    pub fn name(&self) -> String {
        match self {
            Self::Pbf(p) | Self::Osc(p) | Self::State(p) | Self::Dir(p) => p.display().to_string(),
            Self::Url(url) => url.clone(),
        }
    }
}

/// A `--seq` value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Seqs {
    /// From the first to the last, or to the latest state; one diff when both are the same.
    Range(u64, Option<u64>),
    /// The latest diff.
    Latest,
}

pub fn parse_seqs(s: &str) -> Result<Seqs> {
    if s == "latest" {
        return Ok(Seqs::Latest);
    }
    let number = |n: &str| {
        n.parse::<u64>()
            .with_context(|| format!("{s:?} is not N, A..B, A.. or latest"))
    };
    match s.split_once("..") {
        Some((a, "")) => Ok(Seqs::Range(number(a)?, None)),
        Some((a, b)) => {
            let (a, b) = (number(a)?, number(b)?);
            if b < a {
                bail!("{s:?} ends before it starts");
            }
            Ok(Seqs::Range(a, Some(b)))
        }
        None => {
            let n = number(s)?;
            Ok(Seqs::Range(n, Some(n)))
        }
    }
}

/// A `--since` value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Since {
    /// A time, in milliseconds since 1970.
    Time(i64),
    /// A PBF file or a state file.
    File(PathBuf),
}

pub fn parse_since(s: &str) -> Result<Since> {
    let time = parse_timestamp(s).or_else(|| {
        (s.len() == 10)
            .then(|| parse_timestamp(&format!("{s}T00:00:00Z")))
            .flatten()
    });
    match time {
        Some(ms) => Ok(Since::Time(ms)),
        None if Path::new(s).exists() => Ok(Since::File(PathBuf::from(s))),
        None => bail!("{s:?} is neither a file nor a time, as 2026-11-09T21:00:00Z or 2026-11-09"),
    }
}

/// A replication stream, opened from a directory or from a server through `--cache`.
pub struct Stream {
    replication: Replication,
    /// The directory, when the stream is local: a cache is read through the server.
    dir: Option<PathBuf>,
    /// The server's URL, when the stream is remote.
    url: Option<String>,
}

/// A stream's states, with the latest one already read, as a directory may have no `state.txt`.
struct States<'a>(&'a Replication, State);

impl StateSource for States<'_> {
    fn latest(&self) -> Result<State, OscError> {
        Ok(self.1)
    }

    fn get(&self, sequence: u64) -> Result<Option<State>, OscError> {
        self.0.get(sequence)
    }
}

impl Stream {
    /// Opens a stream input for reading its diffs, decoding `read_ahead` of them ahead; `None` for a server without
    /// a `cache`, whose state files are all that can be read.
    pub fn open(
        input: &Input,
        cache: Option<&Path>,
        read_ahead: Option<usize>,
    ) -> Result<Option<Self>> {
        let (replication, dir, url) = match input {
            Input::Dir(dir) => {
                if cache.is_some() {
                    bail!(
                        "--cache is for a replication server; {} is read where it is",
                        dir.display()
                    );
                }
                (Replication::local(dir), Some(dir.clone()), None)
            }
            Input::Url(url) => {
                let Some(cache) = cache else {
                    return Ok(None);
                };
                (remote(url, cache)?, None, Some(url.clone()))
            }
            Input::Pbf(_) | Input::Osc(_) | Input::State(_) => {
                unreachable!("only directories and servers are streams")
            }
        };
        let replication = match read_ahead {
            Some(n) => replication.with_read_ahead(n),
            None => replication,
        };
        Ok(Some(Self {
            replication,
            dir,
            url,
        }))
    }

    /// The latest state: `state.txt`, or in a directory without one, the highest diff it has.
    pub fn latest(&self) -> Result<State> {
        match &self.dir {
            Some(dir) if !dir.join("state.txt").exists() => {
                let Some(&last) = local_sequences(dir)?.last() else {
                    bail!("{} has neither a state.txt nor any diff", dir.display());
                };
                Ok(self.replication.sequence_state(last)?)
            }
            _ => Ok(self.replication.state()?),
        }
    }

    /// The diffs `--seq` or `--since` chooses.
    pub fn chosen(&self, changes: &ChangeArgs) -> Result<Diffs> {
        match (changes.seq, &changes.since) {
            (Some(seqs), _) => self.diffs(seqs),
            (None, Some(since)) => self.since(since),
            (None, None) => unreachable!("check makes sure a stream has --seq or --since"),
        }
    }

    /// The diffs after `since`, up to the latest state.
    fn since(&self, since: &Since) -> Result<Diffs> {
        let latest = self.latest()?;
        let start = match since {
            Since::Time(ms) => Start::Time { timestamp_ms: *ms },
            Since::File(path) => start_after(
                path,
                &States(&self.replication, latest),
                self.url.as_deref(),
            )?,
        };
        let first = match plan(&[&States(&self.replication, latest)], start, None)?.first() {
            Some(step) => step.from.sequence + 1,
            None => latest.sequence + 1,
        };
        Ok(self.replication.diffs(first..=latest.sequence)?)
    }

    /// The diffs `seqs` asks for.
    fn diffs(&self, seqs: Seqs) -> Result<Diffs> {
        // The latest state is read here, not by `diffs`, as a directory may have no `state.txt`.
        let (first, last) = match seqs {
            Seqs::Range(first, Some(last)) => (first, last),
            Seqs::Range(first, None) => (first, self.latest()?.sequence),
            Seqs::Latest => {
                let last = self.latest()?.sequence;
                (last, last)
            }
        };
        Ok(self.replication.diffs(first..=last)?)
    }
}

/// Whether two URLs name the same stream: alike but for `http` or `https`, and a trailing `/`.
fn same_server(a: &str, b: &str) -> bool {
    let bare = |u: &str| {
        let u = u.trim_end_matches('/');
        u.strip_prefix("https://")
            .or_else(|| u.strip_prefix("http://"))
            .unwrap_or(u)
            .to_owned()
    };
    bare(a) == bare(b)
}

/// Where the changes after `path` start in the stream of `source`, whose server is at `url`: after the state of a state
/// file, or after a PBF file by the replication fields of its header. Its sequence is taken only with its base URL, as
/// Geofabrik's extracts give them, and only for that server; otherwise its replication timestamp is the time of a
/// snapshot, as planet files have.
pub fn start_after(path: &Path, source: &dyn StateSource, url: Option<&str>) -> Result<Start> {
    let name = path.display();
    match Input::of(path)? {
        Input::State(path) => Ok(Start::After {
            stream: 0,
            state: State::read(&path)?,
        }),
        Input::Pbf(path) => {
            let reader = PbfReader::open(&path)?;
            let header = reader.header();
            let base_url = header.replication_base_url.as_deref();
            if let (Some(sequence), Some(url)) = (header.replication_sequence, url)
                && base_url.is_some_and(|u| same_server(u, url))
            {
                let sequence = u64::try_from(sequence)
                    .with_context(|| format!("{name} has a negative replication sequence"))?;
                let state = source.get(sequence)?.with_context(|| {
                    format!("{name} is up to sequence {sequence}, which {url} no longer has")
                })?;
                return Ok(Start::After { stream: 0, state });
            }
            match header.replication_timestamp {
                Some(seconds) => Ok(Start::Snapshot {
                    timestamp_ms: seconds * 1000,
                }),
                None => {
                    bail!("{name} has no replication timestamp in its header: give --since a time")
                }
            }
        }
        Input::Osc(_) | Input::Dir(_) | Input::Url(_) => {
            bail!("--since takes a PBF file, a state file or a time, and {name} is none of them")
        }
    }
}

#[cfg(feature = "http")]
#[expect(
    clippy::unnecessary_wraps,
    reason = "it fails without the http feature"
)]
fn remote(url: &str, cache: &Path) -> Result<Replication> {
    Ok(Replication::http(url, cache))
}

#[cfg(not(feature = "http"))]
fn remote(url: &str, _cache: &Path) -> Result<Replication> {
    Err(no_http(url))
}

#[cfg(not(feature = "http"))]
fn no_http(url: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "{url}: this osmflow was built without the http feature, so it reads no servers"
    )
}

/// A server's latest state, read without a cache.
#[cfg(feature = "http")]
pub fn remote_state(url: &str) -> Result<State> {
    use crate::remote::RemoteStates;

    Ok(RemoteStates::new(url).latest()?)
}

#[cfg(not(feature = "http"))]
pub fn remote_state(url: &str) -> Result<State> {
    Err(no_http(url))
}

/// Calls `f` with the state files of the stream `input`, its latest state, and its server's URL: through
/// [`Stream`] for a directory or a server with a `cache`, and straight from a server without one.
pub fn with_states<T>(
    input: &Input,
    cache: Option<&Path>,
    f: impl FnOnce(&dyn StateSource, State, Option<&str>) -> Result<T>,
) -> Result<T> {
    if let Some(stream) = Stream::open(input, cache, None)? {
        let latest = stream.latest()?;
        return f(
            &States(&stream.replication, latest),
            latest,
            stream.url.as_deref(),
        );
    }
    let Input::Url(url) = input else {
        unreachable!("a stream that does not open is a server without a cache");
    };
    remote_with_states(url, f)
}

#[cfg(feature = "http")]
fn remote_with_states<T>(
    url: &str,
    f: impl FnOnce(&dyn StateSource, State, Option<&str>) -> Result<T>,
) -> Result<T> {
    use crate::remote::RemoteStates;

    let states = RemoteStates::new(url);
    let latest = states.latest()?;
    f(&states, latest, Some(url))
}

#[cfg(not(feature = "http"))]
fn remote_with_states<T>(
    url: &str,
    _f: impl FnOnce(&dyn StateSource, State, Option<&str>) -> Result<T>,
) -> Result<T> {
    Err(no_http(url))
}

/// The sequences of the diffs a local stream has, in order: every `AAA/BBB/CCC.osc.gz` three directories down.
pub fn local_sequences(dir: &Path) -> Result<Vec<u64>> {
    let mut found = Vec::new();
    let mut walk = vec![(dir.to_owned(), 0)];
    while let Some((path, depth)) = walk.pop() {
        for entry in
            fs::read_dir(&path).with_context(|| format!("cannot read {}", path.display()))?
        {
            let path = entry?.path();
            if depth < 2 && path.is_dir() {
                walk.push((path, depth + 1));
            } else if depth == 2
                && let Some(sequence) = path
                    .to_str()
                    .filter(|p| p.ends_with(".osc.gz"))
                    .and_then(|p| path_sequence(&p.replace('\\', "/")))
            {
                found.push(sequence);
            }
        }
    }
    found.sort_unstable();
    Ok(found)
}

/// The state file beside an osmChange file named by its sequence, as `956.state.txt` is beside `956.osc.gz`.
pub fn sibling_state(path: &Path) -> Option<State> {
    let name = path.to_str()?.replace('\\', "/");
    path_sequence(&name)?;
    let state = PathBuf::from(name.strip_suffix(".osc.gz")?.to_owned() + ".state.txt");
    State::read(state).ok()
}

/// How long ago `ms`, in milliseconds since 1970, was, by this computer's clock: `2d 3h`, `5h 12m`, `3m`.
pub fn ago(ms: i64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX));
    let minutes = (now - ms) / 60_000;
    let (days, hours, minutes) = (minutes / 1440, minutes / 60 % 24, minutes % 60);
    match (days, hours) {
        (0, 0) => format!("{minutes}m"),
        (0, _) => format!("{hours}h {minutes}m"),
        _ => format!("{days}d {hours}h"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inputs_are_told_apart_by_their_start() {
        let p = Path::new("x");
        assert_eq!(Input::sniff(p, b"\x1f\x8b\x08"), Input::Osc(p.to_owned()));
        assert_eq!(
            Input::sniff(p, b"  \n<?xml version='1.0'?>"),
            Input::Osc(p.to_owned())
        );
        assert_eq!(
            Input::sniff(
                p,
                b"#Thu Oct 29\nsequenceNumber=4956\ntimestamp=2026-10-29T21\\:21\\:26Z\n"
            ),
            Input::State(p.to_owned())
        );
        assert_eq!(
            Input::sniff(p, b"\0\0\0\x0e\n\tOSMHeader"),
            Input::Pbf(p.to_owned())
        );
        assert_eq!(
            Input::of(Path::new(
                "https://planet.openstreetmap.org/replication/minute/"
            ))
            .unwrap(),
            Input::Url("https://planet.openstreetmap.org/replication/minute".to_owned())
        );
    }

    #[test]
    fn sequences_parse() {
        assert_eq!(parse_seqs("7").unwrap(), Seqs::Range(7, Some(7)));
        assert_eq!(parse_seqs("5..9").unwrap(), Seqs::Range(5, Some(9)));
        assert_eq!(parse_seqs("5..").unwrap(), Seqs::Range(5, None));
        assert_eq!(parse_seqs("latest").unwrap(), Seqs::Latest);
        for bad in ["9..5", "..5", "x", "5...", ""] {
            assert!(parse_seqs(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn servers_match_but_for_their_scheme_and_a_trailing_slash() {
        let url = "https://download.geofabrik.de/europe/andorra-updates";
        assert!(same_server(
            "http://download.geofabrik.de/europe/andorra-updates/",
            url
        ));
        assert!(same_server(url, url));
        assert!(!same_server(
            "https://download.geofabrik.de/europe/monaco-updates",
            url
        ));
    }

    #[test]
    fn since_is_a_time_or_a_file() {
        let day = 20_401 * 86_400_000;
        assert_eq!(parse_since("2025-11-09").unwrap(), Since::Time(day));
        assert_eq!(
            parse_since("2025-11-09T21:00:00Z").unwrap(),
            Since::Time(day + 21 * 3_600_000)
        );
        assert_eq!(
            parse_since("Cargo.toml").unwrap(),
            Since::File(PathBuf::from("Cargo.toml"))
        );
        for bad in ["2025-13-01", "2025-11-09T21:00", "no such file"] {
            assert!(parse_since(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn durations_are_short() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis();
        let now = i64::try_from(now).unwrap();
        assert_eq!(ago(now - 3 * 60_000), "3m");
        assert_eq!(ago(now - (5 * 60 + 12) * 60_000), "5h 12m");
        assert_eq!(ago(now - (2 * 1440 + 3 * 60) * 60_000), "2d 3h");
    }
}
