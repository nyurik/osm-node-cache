//! Replication: state files, sequence paths, and reading streams from a local directory and from a server (a mock
//! [`Fetch`], and with the `http` feature a local HTTP server), in order and with the cache.

use std::collections::{HashMap, VecDeque};
use std::fmt::Write as _;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use std::{fs, mem, thread};

use flate2::Compression;
use flate2::write::GzEncoder;
use osmflow::osc::{
    Diff, Fetch, Located, Object, OscError, PARALLEL, Replication, ServerStates, State, StateError,
    StateSource as _, locate, path_sequence, sequence_path,
};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/osc")
        .join(name)
}

/// An empty directory for one test.
fn temp_dir(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn state_files() {
    let planet = State {
        sequence: 7_363_600,
        timestamp_ms: 1_794_124_821_000,
    };
    assert_eq!(
        State::read(fixture("planet/minute/007/363/600.state.txt")).unwrap(),
        planet
    );
    // The same state with what Java properties allow: CRLF line ends, `!` comments, blank lines, white space around
    // keys and values, and a key that is not read.
    assert_eq!(
        "#Sun Nov 08 08:00:47 UTC 2026\r\n! another comment style\r\n\r\n  sequenceNumber = 7363600  \r\n\
         txnMaxQueried=123\r\ntimestamp=2026-11-08T08\\:00\\:21Z\r\n"
            .parse(),
        Ok(planet)
    );
    // Geofabrik's form: a comment naming the planet's sequence, the timestamp first.
    assert_eq!(
        "# original OSM minutely replication sequence number 7363541\ntimestamp=2026-11-07T21\\:21\\:47Z\n\
         sequenceNumber=4640\n"
            .parse(),
        Ok(State {
            sequence: 4640,
            timestamp_ms: 1_794_086_507_000
        })
    );
    assert_eq!(
        "sequenceNumber=1\ntimestamp=1970-01-01T00\\:00\\:01Z".parse(),
        Ok(State {
            sequence: 1,
            timestamp_ms: 1000
        })
    );

    let bad = |text: &str| text.parse::<State>().unwrap_err();
    // A field missing, or the file cut off before it.
    assert_eq!(
        bad("#Sun Nov 08 08:00:47 UTC 2026\nsequenceNumber=73636"),
        StateError::Missing("timestamp")
    );
    assert_eq!(
        bad("timestamp=2026-11-08T08\\:00\\:21Z"),
        StateError::Missing("sequenceNumber")
    );
    assert!(matches!(
        bad("sequenceNumber=-1\ntimestamp=2026-11-08T08\\:00\\:21Z"),
        StateError::Value {
            key: "sequenceNumber",
            ..
        }
    ));
    // A timestamp without its `Z`.
    assert!(matches!(
        bad("sequenceNumber=1\ntimestamp=2026-11-08T08:00:21"),
        StateError::Value {
            key: "timestamp",
            ..
        }
    ));

    // Errors in a file name it.
    let dir = temp_dir("state-files");
    let path = dir.join("bad.state.txt");
    fs::write(&path, "sequenceNumber=1").unwrap();
    assert!(matches!(
        State::read(&path),
        Err(OscError::State { origin, kind: StateError::Missing("timestamp") })
            if origin == path.display().to_string()
    ));
    assert!(matches!(
        State::read(dir.join("none.state.txt")),
        Err(OscError::File { .. })
    ));
}

#[test]
fn sequence_paths() {
    for (sequence, path) in [
        (0, "000/000/000"),
        (7, "000/000/007"),
        (999, "000/000/999"),
        (1000, "000/001/000"),
        (124_081, "000/124/081"),
        (7_363_600, "007/363/600"),
        (999_999_999, "999/999/999"),
        (1_000_000_000, "1000/000/000"),
        (u64::MAX, "18446744073709/551/615"),
    ] {
        assert_eq!(sequence_path(sequence), path);
        assert_eq!(path_sequence(path), Some(sequence), "{path}");
        assert_eq!(
            path_sequence(&format!("{path}.osc.gz")),
            Some(sequence),
            "{path}"
        );
        assert_eq!(
            path_sequence(&format!("https://x/minute/{path}.state.txt")),
            Some(sequence),
            "{path}"
        );
    }
    for bad in [
        "",
        "000/000",
        "00/000/000",
        "000/00/000",
        "000/000/0000",
        "000/000/00a",
        "0000/000/000",
        "000/000/000.osc",
        "000/000/000.txt",
        "18446744073709/551/616",
        "/000/000",
        "000//000/000x",
    ] {
        assert_eq!(path_sequence(bad), None, "{bad:?}");
    }
}

/// The XML of a diff with `nodes` nodes, numbered from `sequence * 1_000_000`.
fn diff_xml(sequence: u64, nodes: u64) -> String {
    let mut xml = String::from("<osmChange version=\"0.6\"><modify>\n");
    for i in 0..nodes {
        let id = sequence * 1_000_000 + i;
        writeln!(xml, r#"<node id="{id}" version="2" lat="1.5" lon="-2.5"><tag k="seq" v="{sequence}"/></node>"#).unwrap();
    }
    xml + "</modify></osmChange>\n"
}

fn gzip(data: &[u8]) -> Vec<u8> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
    encoder.write_all(data).unwrap();
    encoder.finish().unwrap()
}

fn state_text(sequence: u64) -> String {
    format!(
        "#Sun Nov 08 08:00:47 UTC 2026\nsequenceNumber={sequence}\ntimestamp=2026-11-08T08\\:{:02}\\:00Z\n",
        sequence % 60
    )
}

fn state(sequence: u64) -> State {
    State {
        sequence,
        timestamp_ms: 1_794_124_800_000 + 60_000 * (sequence % 60).cast_signed(),
    }
}

/// The files of a stream whose latest state is `latest`, holding diffs `sequences`, the first with `first_nodes`
/// nodes and the rest with one: relative path and content.
fn stream(
    latest: u64,
    sequences: impl IntoIterator<Item = u64>,
    first_nodes: u64,
) -> Vec<(String, Vec<u8>)> {
    let mut files = vec![("state.txt".to_owned(), state_text(latest).into_bytes())];
    for (i, sequence) in sequences.into_iter().enumerate() {
        let nodes = if i == 0 { first_nodes } else { 1 };
        let path = sequence_path(sequence);
        files.push((
            format!("{path}.state.txt"),
            state_text(sequence).into_bytes(),
        ));
        files.push((
            format!("{path}.osc.gz"),
            gzip(diff_xml(sequence, nodes).as_bytes()),
        ));
    }
    files
}

fn write_stream(dir: &Path, files: &[(String, Vec<u8>)]) {
    for (path, data) in files {
        let path = dir.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, data).unwrap();
    }
}

/// Checks a diff made by [`stream`].
fn check(diff: &Diff, sequence: u64, nodes: usize) {
    assert_eq!(diff.state, state(sequence));
    assert_eq!(diff.changes.len(), nodes);
    let Object::Node(node) = &diff.changes[0].object else {
        unreachable!("not a node");
    };
    assert_eq!(node.id, (sequence * 1_000_000).cast_signed());
    assert_eq!(node.tags.get("seq"), Some(sequence.to_string().as_str()));
}

fn sequences(diffs: impl Iterator<Item = Result<Diff, OscError>>) -> Vec<u64> {
    diffs.map(|d| d.unwrap().state.sequence).collect()
}

#[test]
fn local_stream() {
    let dir = temp_dir("replication-local");
    // Sequence 6 exists, but the latest state is 5.
    write_stream(&dir, &stream(5, 1..=6, 1));
    let repl = Replication::local(&dir);
    assert_eq!(repl.dir(), dir);
    assert_eq!(repl.state().unwrap(), state(5));
    assert_eq!(repl.sequence_state(3).unwrap(), state(3));
    assert_eq!(repl.diff_file(3).unwrap(), dir.join("000/000/003.osc.gz"));
    check(&repl.diff(4).unwrap(), 4, 1);
    let diffs: Vec<Diff> = repl.diffs(2..).unwrap().map(Result::unwrap).collect();
    assert_eq!(diffs.len(), 4);
    for (diff, sequence) in diffs.iter().zip(2..) {
        check(diff, sequence, 1);
    }
    assert_eq!(repl.diffs(6..).unwrap().count(), 0);
    assert_eq!(repl.diffs(5..).unwrap().count(), 1);
    // A range with an end reads just its diffs, past the latest state too, and needs no `state.txt`.
    assert_eq!(sequences(repl.diffs(5..7).unwrap()), [5, 6]);
    assert_eq!(repl.diffs(4..4).unwrap().count(), 0);
    fs::rename(dir.join("state.txt"), dir.join("state.bak")).unwrap();
    assert_eq!(sequences(repl.diffs(2..=3).unwrap()), [2, 3]);
    assert!(
        repl.diffs(2..).is_err(),
        "an open range needs the latest state"
    );
    fs::rename(dir.join("state.bak"), dir.join("state.txt")).unwrap();

    // A state file naming another sequence is an error, as it is from a server.
    fs::write(dir.join("000/000/004.state.txt"), state_text(3)).unwrap();
    let error = repl.diff(4).unwrap_err();
    assert!(
        matches!(
            &error,
            OscError::State { origin, kind: StateError::Value { key: "sequenceNumber", value } }
                if origin.ends_with("004.state.txt") && value == "3"
        ),
        "{error}"
    );
}

#[test]
fn read_ahead_keeps_order() {
    // The first diff takes far longer to decode than the rest, which are decoded ahead and must wait for it.
    let dir = temp_dir("replication-order");
    write_stream(&dir, &stream(20, 1..=20, 200_000));
    for read_ahead in [1, 2, 4, 16] {
        let mut diffs = Replication::local(&dir)
            .with_read_ahead(read_ahead)
            .diffs(1..)
            .unwrap();
        check(&diffs.next().unwrap().unwrap(), 1, 200_000);
        assert_eq!(
            sequences(diffs),
            (2..=20).collect::<Vec<_>>(),
            "read ahead {read_ahead}"
        );
    }
    // Dropping the iterator early stops the threads reading ahead.
    let first = Replication::local(&dir).diffs(1..).unwrap().take(2).count();
    assert_eq!(first, 2);
}

#[test]
fn errors_end_the_stream() {
    let dir = temp_dir("replication-missing");
    write_stream(&dir, &stream(5, [1, 2, 4, 5], 1));
    let mut diffs = Replication::local(&dir).diffs(1..).unwrap();
    assert_eq!(diffs.next().unwrap().unwrap().state.sequence, 1);
    assert_eq!(diffs.next().unwrap().unwrap().state.sequence, 2);
    let error = diffs.next().unwrap().unwrap_err();
    assert!(
        matches!(&error, OscError::File { path, .. } if path.ends_with("000/000/003.state.txt")),
        "{error}"
    );
    assert!(diffs.next().is_none());

    let empty = temp_dir("replication-empty");
    assert!(matches!(
        Replication::local(&empty).diffs(1..),
        Err(OscError::File { .. })
    ));
}

/// A server for [`Fetch`]: files by URL, answers to give first for some URLs (`None` for a 404), and every URL
/// requested.
#[derive(Default)]
struct Server {
    files: HashMap<String, Vec<u8>>,
    first: HashMap<String, VecDeque<Option<Vec<u8>>>>,
    requests: Vec<String>,
}

#[derive(Clone, Default)]
struct Mock(Arc<Mutex<Server>>);

const BASE: &str = "https://example.org/replication/minute";

impl Mock {
    fn new(files: &[(String, Vec<u8>)]) -> Self {
        let mock = Self::default();
        mock.0.lock().unwrap().files = files
            .iter()
            .map(|(p, d)| (format!("{BASE}/{p}"), d.clone()))
            .collect();
        mock
    }

    fn answer_first(&self, path: &str, answer: Option<&[u8]>) {
        let mut server = self.0.lock().unwrap();
        server
            .first
            .entry(format!("{BASE}/{path}"))
            .or_default()
            .push_back(answer.map(<[u8]>::to_vec));
    }

    /// The paths requested since the last call.
    fn requests(&self) -> Vec<String> {
        let requests = mem::take(&mut self.0.lock().unwrap().requests);
        requests
            .iter()
            .map(|r| r.strip_prefix(&format!("{BASE}/")).unwrap().to_owned())
            .collect()
    }
}

impl Fetch for Mock {
    fn fetch(&self, url: &str) -> io::Result<Option<Vec<u8>>> {
        let mut server = self.0.lock().unwrap();
        server.requests.push(url.to_owned());
        if let Some(answer) = server.first.get_mut(url).and_then(VecDeque::pop_front) {
            return Ok(answer);
        }
        Ok(server.files.get(url).cloned())
    }
}

/// Every file under `dir`, relative to it with `/` between its parts on every OS, sorted.
fn files_in(dir: &Path) -> Vec<String> {
    fn walk(dir: &Path, root: &Path, out: &mut Vec<String>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(&path, root, out);
            } else {
                let parts: Vec<_> = path
                    .strip_prefix(root)
                    .unwrap()
                    .iter()
                    .map(|part| part.to_string_lossy())
                    .collect();
                out.push(parts.join("/"));
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out);
    out.sort();
    out
}

/// Panics when asked for a URL ending in its suffix, as a buggy fetcher might.
struct Panicking(Mock, &'static str);

impl Fetch for Panicking {
    #[expect(
        clippy::panic_in_result_fn,
        reason = "the panic is what the test needs"
    )]
    fn fetch(&self, url: &str) -> io::Result<Option<Vec<u8>>> {
        assert!(!url.ends_with(self.1), "a buggy fetcher");
        self.0.fetch(url)
    }
}

#[test]
fn a_panicking_fetcher_is_an_error_not_the_end() {
    let cache = temp_dir("replication-panic");
    let mock = Mock::new(&stream(5, 1..=5, 1));
    let repl = Replication::remote(BASE, &cache, Panicking(mock, "000/000/004.state.txt"));
    let diffs: Vec<_> = repl.diffs(3..).unwrap().collect();
    assert_eq!(diffs.len(), 2, "{diffs:?}");
    assert_eq!(diffs[0].as_ref().unwrap().state.sequence, 3);
    assert!(matches!(diffs[1], Err(OscError::ReadAhead)), "{diffs:?}");
}

#[test]
fn a_server_without_state_is_not_a_stream() {
    let cache = temp_dir("replication-no-state");
    let repl = Replication::remote(BASE, &cache, Mock::new(&[]));
    let error = repl.state().unwrap_err();
    assert!(matches!(error, OscError::NoStream { .. }), "{error}");
}

#[test]
fn remote_stream_is_cached() {
    let cache = temp_dir("replication-remote");
    // The server has sequence 6, but its latest state is 5.
    let files = stream(5, 1..=6, 1);
    let mock = Mock::new(&files);
    let repl = Replication::remote(&format!("{BASE}/"), &cache, mock.clone());
    assert_eq!(sequences(repl.diffs(3..).unwrap()), [3, 4, 5]);
    let mut expected = vec!["state.txt".to_owned()];
    for s in 3..=5 {
        expected.push(format!("000/000/00{s}.state.txt"));
        expected.push(format!("000/000/00{s}.osc.gz"));
    }
    assert_eq!(mock.requests(), expected);

    // Cached files are never downloaded again; only the latest state is.
    assert_eq!(sequences(repl.diffs(3..).unwrap()), [3, 4, 5]);
    assert_eq!(mock.requests(), ["state.txt"]);
    check(&repl.diff(4).unwrap(), 4, 1);
    assert_eq!(mock.requests(), Vec::<String>::new());

    // Nothing past the latest state is downloaded, though the server has it.
    let error = repl.diff(6).unwrap_err();
    assert!(
        matches!(
            error,
            OscError::NotPublished {
                sequence: 6,
                latest: 5
            }
        ),
        "{error}"
    );
    assert_eq!(mock.requests(), ["state.txt"]);

    // The cache holds whole files only, and reads as a local stream.
    let mut cached: Vec<String> = expected.clone();
    cached.sort();
    assert_eq!(files_in(&cache), cached);
    for (path, data) in &files {
        if cached.contains(path) {
            assert_eq!(&fs::read(cache.join(path)).unwrap(), data, "{path}");
        }
    }
    assert_eq!(
        sequences(Replication::local(&cache).diffs(3..).unwrap()),
        [3, 4, 5]
    );

    // Once the server publishes more, it is read.
    mock.0
        .lock()
        .unwrap()
        .files
        .insert(format!("{BASE}/state.txt"), state_text(6).into_bytes());
    assert_eq!(sequences(repl.diffs(5..).unwrap()), [5, 6]);
    assert_eq!(
        mock.requests(),
        ["state.txt", "000/000/006.state.txt", "000/000/006.osc.gz"]
    );
}

#[test]
fn a_search_by_time_downloads_state_files_once_and_no_diff() {
    // Diffs 10 to 59 are on the server, the older ones deleted.
    let cache = temp_dir("replication-locate");
    let mock = Mock::new(&stream(59, 10..=59, 1));
    let repl = Replication::remote(BASE, &cache, mock.clone());
    let t = state(30).timestamp_ms + 1000;
    assert_eq!(locate(&repl, t).unwrap(), Located::At(state(30)));
    let requests = mock.requests();
    assert!(
        requests.len() <= 6 && requests.iter().all(|r| r.ends_with("state.txt")),
        "{requests:?}"
    );
    // Searched again, only the latest state is downloaded: the rest are in the cache.
    assert_eq!(locate(&repl, t).unwrap(), Located::At(state(30)));
    assert_eq!(mock.requests(), ["state.txt"]);
    // Before the oldest diff kept, the server answers 404 for the older states.
    assert_eq!(
        locate(&repl, state(5).timestamp_ms).unwrap(),
        Located::BeforeOldest(state(10))
    );
    // A local directory has no state for a diff it lacks.
    let local = Replication::local(&cache);
    assert_eq!(local.get(30).unwrap(), Some(state(30)));
    assert_eq!(local.get(1000).unwrap(), None);
}

#[test]
fn server_states_are_checked_and_downloaded_once_without_a_cache() {
    // Diffs 10 to 59, the older ones deleted; state 20 is misrouted, and names sequence 21.
    let mut files = stream(59, 10..=59, 1);
    let misrouted = format!("{}.state.txt", sequence_path(20));
    files.retain(|f| f.0 != misrouted);
    files.push((misrouted, state_text(21).into_bytes()));
    let mock = Mock::new(&files);
    let states = ServerStates::new(BASE, mock.clone());

    let t = state(30).timestamp_ms + 1000;
    assert_eq!(locate(&states, t).unwrap(), Located::At(state(30)));
    let requests = mock.requests();
    assert!(
        requests.len() <= 6 && requests.iter().all(|r| r.ends_with("state.txt")),
        "{requests:?}"
    );
    // Searched again, nothing is downloaded: the states are kept in memory, the latest too.
    assert_eq!(locate(&states, t).unwrap(), Located::At(state(30)));
    assert_eq!(mock.requests(), Vec::<String>::new());

    // A deleted state, and one past the latest, are not there; the second is not asked for.
    assert_eq!(states.get(5).unwrap(), None);
    assert_eq!(states.get(60).unwrap(), None);
    assert_eq!(mock.requests(), ["000/000/005.state.txt"]);
    // A state file that names another sequence is an error.
    assert!(matches!(states.get(20), Err(OscError::State { .. })));
    // A truncated state file is fetched again.
    mock.answer_first("000/000/040.state.txt", Some(b"sequenceNumber=40\n"));
    assert_eq!(states.get(40).unwrap(), Some(state(40)));
    // A server without a `state.txt` is not a stream.
    let nowhere = ServerStates::new("https://example.org/nowhere", mock.clone());
    assert!(matches!(nowhere.latest(), Err(OscError::NoStream { .. })));
}

/// A server that answers slowly, counting the requests it answers at once.
#[derive(Default)]
struct Slow {
    files: HashMap<String, Vec<u8>>,
    asked: Mutex<Vec<String>>,
    now: AtomicUsize,
    most: AtomicUsize,
}

/// A [`Slow`] server as a [`Fetch`], which [`ServerStates`] owns.
struct SlowFetch(Arc<Slow>);

impl Fetch for SlowFetch {
    fn fetch(&self, url: &str) -> io::Result<Option<Vec<u8>>> {
        let slow = &self.0;
        let at_once = slow.now.fetch_add(1, Ordering::SeqCst) + 1;
        slow.most.fetch_max(at_once, Ordering::SeqCst);
        thread::sleep(Duration::from_millis(20));
        slow.asked.lock().unwrap().push(url.to_owned());
        slow.now.fetch_sub(1, Ordering::SeqCst);
        Ok(slow.files.get(url).cloned())
    }
}

#[test]
fn server_states_are_read_ahead_three_at_a_time() {
    let files = stream(59, 10..=59, 1)
        .into_iter()
        .map(|(p, d)| (format!("{BASE}/{p}"), d))
        .collect();
    let slow = Arc::new(Slow {
        files,
        ..Slow::default()
    });
    let states = ServerStates::new(BASE, SlowFetch(Arc::clone(&slow)));
    states.prefetch(20..=39);
    assert_eq!(slow.most.load(Ordering::SeqCst), PARALLEL);
    // The latest state, to know what is published, and each of the 20 once.
    assert_eq!(slow.asked.lock().unwrap().len(), 21);
    assert_eq!(states.get(30).unwrap(), Some(state(30)));
    assert_eq!(
        slow.asked.lock().unwrap().len(),
        21,
        "read ahead, not asked again"
    );
}

#[test]
fn remote_files_are_checked() {
    let cache = temp_dir("replication-checked");
    let files = stream(3, 1..=3, 1);
    let mock = Mock::new(&files);
    let repl = Replication::remote(BASE, &cache, mock.clone());

    // A truncated state file is fetched again.
    mock.answer_first("state.txt", Some(b"sequenceNumber=3\n"));
    assert_eq!(repl.state().unwrap(), state(3));
    assert_eq!(mock.requests(), ["state.txt", "state.txt"]);

    // A diff that is not complete gzip is fetched again, a few times, and then is an error; nothing is cached.
    let gz = &files
        .iter()
        .find(|f| f.0 == "000/000/001.osc.gz")
        .unwrap()
        .1;
    for _ in 0..3 {
        mock.answer_first("000/000/001.osc.gz", Some(&gz[..gz.len() - 4]));
    }
    let error = repl.diff_file(1).unwrap_err();
    assert!(
        matches!(&error, OscError::Fetch { url, .. } if url.ends_with("001.osc.gz")),
        "{error}"
    );
    assert!(!cache.join("000/000/001.osc.gz").exists());
    assert!(repl.diff_file(1).is_ok(), "fetched again, whole");

    // A state file naming another sequence is an error.
    mock.answer_first("000/000/002.state.txt", Some(state_text(7).as_bytes()));
    let error = repl.sequence_state(2).unwrap_err();
    assert!(matches!(error, OscError::State { .. }), "{error}");
    // It is not cached, and one in the cache is checked too.
    let cached = cache.join("000/000/002.state.txt");
    assert!(!cached.exists());
    fs::write(&cached, state_text(7)).unwrap();
    let error = repl.sequence_state(2).unwrap_err();
    assert!(matches!(error, OscError::State { .. }), "{error}");
    fs::remove_file(&cached).unwrap();

    // A published file the server does not have is an error, in its turn.
    mock.0
        .lock()
        .unwrap()
        .files
        .remove(&format!("{BASE}/000/000/003.osc.gz"));
    let mut diffs = repl.diffs(2..).unwrap();
    assert_eq!(diffs.next().unwrap().unwrap().state.sequence, 2);
    let error = diffs.next().unwrap().unwrap_err();
    assert!(
        matches!(&error, OscError::NotFound { url } if url.ends_with("003.osc.gz")),
        "{error}"
    );
    assert!(diffs.next().is_none());

    let no_files: Vec<String> = files_in(&cache)
        .into_iter()
        .filter(|f| f.contains("tmp"))
        .collect();
    assert!(no_files.is_empty(), "{no_files:?}");
}

#[cfg(feature = "http")]
mod http {
    use std::io::{BufRead as _, BufReader, Write as _};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::{Duration, Instant};

    use osmflow::osc::{Fetch as _, HttpFetch, Replication};

    use super::{gzip, state_text, temp_dir};

    /// A response: status, extra headers, body.
    type Response = (u16, &'static str, Vec<u8>);

    /// Serves `responses` in order, one per connection, and returns the server's base URL and the paths requested.
    fn serve(responses: Vec<Response>) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&requests);
        thread::spawn(move || {
            for (status, headers, body) in responses {
                let (mut stream, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                log.lock()
                    .unwrap()
                    .push(line.split(' ').nth(1).unwrap().to_owned());
                while line != "\r\n" {
                    line.clear();
                    reader.read_line(&mut line).unwrap();
                }
                let head = format!(
                    "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n",
                    body.len()
                );
                stream.write_all(head.as_bytes()).unwrap();
                stream.write_all(&body).unwrap();
            }
        });
        (base, requests)
    }

    #[test]
    fn a_request_that_cannot_succeed_is_not_retried() {
        // With the default 8 retries, retrying this would take minutes.
        let start = Instant::now();
        HttpFetch::new().fetch("not a url").unwrap_err();
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn retries() {
        let (base, requests) = serve(vec![
            (503, "Retry-After: 0\r\n", Vec::new()),
            (429, "Retry-After: 0\r\n", Vec::new()),
            (200, "", b"hello".to_vec()),
            (404, "", Vec::new()),
            (400, "", Vec::new()),
            (500, "Retry-After: 0\r\n", Vec::new()),
            (500, "Retry-After: 0\r\n", Vec::new()),
            (500, "Retry-After: 0\r\n", Vec::new()),
        ]);
        let fetch = HttpFetch::new().with_retries(2);
        assert_eq!(
            fetch.fetch(&format!("{base}/a")).unwrap(),
            Some(b"hello".to_vec())
        );
        assert_eq!(fetch.fetch(&format!("{base}/b")).unwrap(), None);
        assert!(
            fetch.fetch(&format!("{base}/c")).is_err(),
            "400 is not retried"
        );
        assert!(
            fetch.fetch(&format!("{base}/d")).is_err(),
            "retries run out"
        );
        assert_eq!(
            *requests.lock().unwrap(),
            ["/a", "/a", "/a", "/b", "/c", "/d", "/d", "/d"]
        );
    }

    #[test]
    fn replication_over_http() {
        let diff = gzip(super::diff_xml(7, 3).as_bytes());
        let (base, requests) = serve(vec![
            (200, "", state_text(7).into_bytes()),
            (503, "Retry-After: 0\r\n", Vec::new()),
            (200, "", state_text(7).into_bytes()),
            (200, "", diff),
        ]);
        let cache = temp_dir("replication-http");
        let repl = Replication::http(&format!("{base}/minute"), &cache);
        let diffs: Vec<_> = repl.diffs(7..).unwrap().map(Result::unwrap).collect();
        assert_eq!(diffs.len(), 1);
        super::check(&diffs[0], 7, 3);
        assert_eq!(
            *requests.lock().unwrap(),
            [
                "/minute/state.txt",
                "/minute/000/000/007.state.txt",
                "/minute/000/000/007.state.txt",
                "/minute/000/000/007.osc.gz"
            ]
        );
    }
}
