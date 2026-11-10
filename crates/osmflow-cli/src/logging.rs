//! The log, on standard error: what `RUST_LOG` chooses, raised by `-v`, in the format `RUST_LOG_FORMAT` names.

use std::env;
use std::io::{self, IsTerminal as _};
use std::str::FromStr;

use tracing_subscriber::filter::{EnvFilter, FilterExt as _, LevelFilter, Targets};
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;
use tracing_subscriber::{Layer, Registry, fmt};

/// How log lines are written, from `RUST_LOG_FORMAT`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum LogFormat {
    /// The level and the message, with its fields: `INFO downloaded url=… bytes=113`.
    #[default]
    Bare,
    /// [`Bare`](Self::Bare) with the time, the target and the spans, on one line.
    Compact,
    /// The time, the level, the spans, the target and the message.
    Full,
    /// Several lines for each event, for reading closely.
    Pretty,
    /// One JSON object for each event.
    Json,
}

impl FromStr for LogFormat {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "bare" => Ok(Self::Bare),
            "compact" => Ok(Self::Compact),
            "full" => Ok(Self::Full),
            "pretty" => Ok(Self::Pretty),
            "json" | "jsonl" => Ok(Self::Json),
            _ => Err(format!(
                "RUST_LOG_FORMAT={s:?} is not bare, compact, full, pretty or json"
            )),
        }
    }
}

impl LogFormat {
    /// The format `RUST_LOG_FORMAT` names, or with a warning for one it does not, the default.
    #[expect(clippy::print_stderr, reason = "the log is not started yet")]
    fn from_env() -> Self {
        match env::var("RUST_LOG_FORMAT").ok().as_deref() {
            None | Some("") => Self::default(),
            Some(s) => s.parse().unwrap_or_else(|e| {
                eprintln!("osmflow: {e}; writing the log as bare lines");
                Self::default()
            }),
        }
    }

    /// The layer that writes events in this format to standard error, with colors when it is a terminal.
    fn layer(self) -> Box<dyn Layer<Registry> + Send + Sync> {
        let ansi = io::stderr().is_terminal();
        let layer = fmt::layer().with_writer(io::stderr).with_ansi(ansi);
        match self {
            Self::Bare => layer.compact().without_time().with_target(false).boxed(),
            Self::Compact => layer.compact().boxed(),
            Self::Full => layer.boxed(),
            Self::Pretty => layer.pretty().boxed(),
            Self::Json => layer.json().with_ansi(false).boxed(),
        }
    }
}

/// Starts the log: what `RUST_LOG` asks for, warnings by default, and osmflow's events at the level `verbose` asks
/// for. An event either one passes is logged, so `-v` adds to `RUST_LOG` and never hides what it shows.
pub fn start(verbose: u8) {
    let env = EnvFilter::builder()
        .with_default_directive(LevelFilter::WARN.into())
        .from_env_lossy();
    let level = match verbose {
        0 => LevelFilter::WARN,
        1 => LevelFilter::INFO,
        2 => LevelFilter::DEBUG,
        _ => LevelFilter::TRACE,
    };
    let filter = env.or(Targets::new().with_target("osmflow", level));
    // This fails only when a subscriber is already set, and nothing else sets one.
    let _ = tracing_subscriber::registry()
        .with(LogFormat::from_env().layer().with_filter(filter))
        .try_init();
}

#[cfg(test)]
mod tests {
    use tracing::dispatcher::{self, Dispatch};
    use tracing::info;

    use super::*;

    #[test]
    fn formats_parse_in_any_case() {
        for (name, format) in [
            ("bare", LogFormat::Bare),
            ("Compact", LogFormat::Compact),
            ("FULL", LogFormat::Full),
            ("pretty", LogFormat::Pretty),
            ("json", LogFormat::Json),
            ("jsonl", LogFormat::Json),
        ] {
            assert_eq!(name.parse(), Ok(format), "{name}");
        }
        assert!(
            "xml"
                .parse::<LogFormat>()
                .unwrap_err()
                .contains("bare, compact")
        );
    }

    #[test]
    fn every_format_writes_events() {
        for format in [
            LogFormat::Bare,
            LogFormat::Compact,
            LogFormat::Full,
            LogFormat::Pretty,
            LogFormat::Json,
        ] {
            let dispatch = Dispatch::new(tracing_subscriber::registry().with(format.layer()));
            dispatcher::with_default(&dispatch, || {
                info!(bytes = 113, "downloaded");
            });
        }
    }
}
