//! [`Fetch`] over HTTP, with `ureq`.

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher as _, Hasher as _};
use std::time::{Duration, Instant};
use std::{io, thread};

use tracing::{info, warn};

use crate::osc::Fetch;

/// The longest `Retry-After` honoured; a server asking for longer is retried after this.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(600);
/// The longest pause between tries when the server does not say.
const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// The largest body read: several times a planet day diff, so only a runaway server reaches it.
const MAX_BODY: u64 = 2 << 30;
/// The longest a body may take to arrive, however slowly it comes: a stalled mirror must not block the caller for good.
const BODY_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// Fetches files over HTTP or HTTPS, one request at a time, following redirects. Requests answered with 408, 429 or a
/// 5xx status, or failing to connect or to read, are retried after the pause the server's `Retry-After` asks for (in
/// seconds), or else after 1, 2, 4 ... up to 60 seconds, each with a small random addition, so clients do not retry in
/// step. A request that cannot succeed by trying again, such as a malformed URL or a failed TLS handshake, fails at
/// once. A body over 2 GiB, or one that takes over 30 minutes, is an error.
#[derive(Debug, Clone)]
pub struct HttpFetch {
    agent: ureq::Agent,
    retries: u32,
}

impl Default for HttpFetch {
    fn default() -> Self {
        Self::new()
    }
}

impl HttpFetch {
    /// Retries a request 8 times, and names itself `osmflow/<version>` in the `User-Agent`.
    #[must_use]
    pub fn new() -> Self {
        let config = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .user_agent(concat!("osmflow/", env!("CARGO_PKG_VERSION")))
            .timeout_connect(Some(Duration::from_secs(30)))
            .timeout_recv_response(Some(Duration::from_secs(60)))
            .timeout_recv_body(Some(BODY_TIMEOUT))
            .build();
        Self {
            agent: config.into(),
            retries: 8,
        }
    }

    /// Sets how many times a failed request is retried.
    #[must_use]
    pub fn with_retries(mut self, retries: u32) -> Self {
        self.retries = retries;
        self
    }
}

impl Fetch for HttpFetch {
    fn fetch(&self, url: &str) -> io::Result<Option<Vec<u8>>> {
        let started = Instant::now();
        let mut attempt = 0;
        loop {
            let (error, delay) = match self.agent.get(url).call() {
                Ok(mut response) => {
                    let status = response.status().as_u16();
                    if response.status().is_success() {
                        let body = response
                            .body_mut()
                            .with_config()
                            .limit(MAX_BODY)
                            .read_to_vec();
                        match body {
                            Ok(body) => {
                                info!(
                                    url,
                                    bytes = body.len(),
                                    took = ?started.elapsed(),
                                    "downloaded"
                                );
                                return Ok(Some(body));
                            }
                            Err(e) if transient(&e) => (e.into_io(), backoff(attempt)),
                            Err(e) => return Err(e.into_io()),
                        }
                    } else if status == 404 {
                        info!(url, "not on the server");
                        return Ok(None);
                    } else {
                        let retry_after = response
                            .headers()
                            .get("retry-after")
                            .and_then(|v| v.to_str().ok());
                        let error = io::Error::other(format!("HTTP status {status}"));
                        match retry_delay(status, retry_after, attempt) {
                            Some(delay) => (error, delay),
                            None => return Err(error),
                        }
                    }
                }
                Err(e) if transient(&e) => (e.into_io(), backoff(attempt)),
                Err(e) => return Err(e.into_io()),
            };
            if attempt >= self.retries {
                return Err(error);
            }
            let delay = delay + jitter(delay);
            warn!(url, %error, after = ?delay, "trying again");
            thread::sleep(delay);
            attempt += 1;
        }
    }
}

/// Whether trying a failed request again can help: the network failed, not the request or the server's answer.
fn transient(error: &ureq::Error) -> bool {
    matches!(
        error,
        ureq::Error::Io(_)
            | ureq::Error::Timeout(_)
            | ureq::Error::ConnectionFailed
            | ureq::Error::HostNotFound
    )
}

/// How long to wait before retrying a request answered with `status` and the `Retry-After` header `retry_after`, after
/// `attempt` retries; `None` if such an answer is not retried.
fn retry_delay(status: u16, retry_after: Option<&str>, attempt: u32) -> Option<Duration> {
    if !matches!(status, 408 | 429 | 500..=599) {
        return None;
    }
    let asked = retry_after
        .and_then(|v| v.trim().parse().ok())
        .map(Duration::from_secs);
    Some(asked.map_or_else(|| backoff(attempt), |d| d.min(MAX_RETRY_AFTER)))
}

/// The pause before retry `attempt` (from 0) when the server does not ask for one: 1 s, doubling, at most a minute.
fn backoff(attempt: u32) -> Duration {
    Duration::from_secs(1 << attempt.min(6)).min(MAX_BACKOFF)
}

/// A random addition to a pause: up to a tenth of it, plus up to 100 ms.
fn jitter(delay: Duration) -> Duration {
    let random = RandomState::new().build_hasher().finish();
    let range = u64::try_from(delay.as_millis() / 10)
        .unwrap_or(u64::MAX)
        .saturating_add(100);
    Duration::from_millis(random % range)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delays() {
        let s = Duration::from_secs;
        assert_eq!(retry_delay(503, None, 0), Some(s(1)));
        assert_eq!(retry_delay(500, None, 3), Some(s(8)));
        assert_eq!(retry_delay(599, None, 30), Some(s(60)));
        assert_eq!(retry_delay(429, Some("7"), 5), Some(s(7)));
        assert_eq!(retry_delay(408, Some(" 0 "), 2), Some(s(0)));
        assert_eq!(retry_delay(429, Some("86400"), 0), Some(MAX_RETRY_AFTER));
        // A date, or anything else that is not a number of seconds, falls back to the backoff.
        assert_eq!(
            retry_delay(503, Some("Wed, 21 Oct 2026 07:28:00 GMT"), 1),
            Some(s(2))
        );
        for status in [400, 401, 403, 404, 410, 418, 301, 600] {
            assert_eq!(retry_delay(status, Some("1"), 0), None, "{status}");
        }
        for d in [s(0), s(1), s(60)] {
            let j = jitter(d);
            assert!(j < d / 10 + Duration::from_millis(100), "{j:?} for {d:?}");
        }
    }
}
