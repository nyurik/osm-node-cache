//! A server's state files, read over HTTP without a cache.

use std::cell::{OnceCell, RefCell};
use std::collections::HashMap;

use osmflow::osc::{Fetch, HttpFetch, OscError, State, StateSource, sequence_path};

/// The state files of a server, read without a cache: for looking sequences up, which reads a few of them. Each is
/// downloaded once, and kept in memory.
pub struct RemoteStates<F = HttpFetch> {
    url: String,
    fetch: F,
    read: RefCell<HashMap<u64, Option<State>>>,
    latest: OnceCell<State>,
}

impl RemoteStates {
    pub fn new(url: &str) -> Self {
        Self::with_fetch(url, HttpFetch::new())
    }
}

impl<F: Fetch> RemoteStates<F> {
    /// The state files at `url`, downloaded with `fetch`.
    pub fn with_fetch(url: &str, fetch: F) -> Self {
        Self {
            url: url.to_owned(),
            fetch,
            read: RefCell::default(),
            latest: OnceCell::new(),
        }
    }

    /// The state file at `relative`, or `None` if the server has none there.
    fn fetch_state(&self, relative: &str) -> Result<Option<State>, OscError> {
        let url = format!("{}/{relative}", self.url);
        let body = match self.fetch.fetch(&url) {
            Ok(Some(body)) => body,
            Ok(None) => return Ok(None),
            Err(source) => return Err(OscError::Fetch { url, source }),
        };
        String::from_utf8_lossy(&body)
            .parse()
            .map(Some)
            .map_err(|kind| OscError::State { origin: url, kind })
    }
}

impl<F: Fetch> StateSource for RemoteStates<F> {
    fn latest(&self) -> Result<State, OscError> {
        if let Some(&latest) = self.latest.get() {
            return Ok(latest);
        }
        let latest = self
            .fetch_state("state.txt")?
            .ok_or_else(|| OscError::NoStream {
                url: format!("{}/state.txt", self.url),
            })?;
        Ok(*self.latest.get_or_init(|| latest))
    }

    fn get(&self, sequence: u64) -> Result<Option<State>, OscError> {
        if let Some(&state) = self.read.borrow().get(&sequence) {
            return Ok(state);
        }
        let path = sequence_path(sequence);
        let state = self.fetch_state(&format!("{path}.state.txt"))?;
        self.read.borrow_mut().insert(sequence, state);
        Ok(state)
    }
}
