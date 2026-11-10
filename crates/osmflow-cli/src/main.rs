//! The `osmflow` command: dumps OpenStreetMap PBF and osmChange files as text or GeoJSON, summarizes them, and finds
//! replication sequences.

use std::io;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

mod changes;
mod dump;
mod input;
mod logging;
mod output;
mod select;
mod seq;
mod stats;
#[cfg(feature = "sync")]
mod sync;
mod write;

#[derive(Debug, Parser)]
#[command(
    name = "osmflow",
    version,
    about = "Dumps and summarizes OpenStreetMap PBF and osmChange files"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
    /// Log more: downloads; twice, file reads; three times, every probe.
    ///
    /// The log goes to standard error. `RUST_LOG` chooses what is logged as usual, with warnings by default; each `-v`
    /// adds osmflow's own events of one more level to that: info (each download, with its size and time), then debug
    /// (each state file and diff read, and each step of a plan), then trace (each state a search reads).
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    verbose: u8,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Writes objects or changes as text or GeoJSON.
    ///
    /// The objects of a PBF file, or the changes of an osmChange file or a replication stream: all, or those chosen
    /// by ID, type and tag, and for changes by action, changeset and user.
    Dump(dump::Args),
    /// Summarizes a PBF file, osmChange input or replication stream.
    ///
    /// A PBF file's header, blocks, object counts, ID ranges and extent; osmChange input's changes by type and
    /// action, extent, time span, changesets and users; or a replication stream's latest state.
    Stats(stats::Args),
    /// Finds a replication stream's sequence at a time or file.
    ///
    /// Reads state files only: the latest, and a few more for a time. Prints the last state at or before the time, or
    /// where the file is up to, and where `--since` of `dump` and `stats` starts from it.
    Seq(seq::Args),
    /// Keeps OpenStreetMap data in PostgreSQL, current from replication.
    ///
    /// Imports a PBF file into the database's osm schema, applies the replication diffs after it, and rebuilds the
    /// node file and reverse index kept beside the database if they are lost.
    #[cfg(feature = "sync")]
    Sync(sync::Args),
}

#[expect(clippy::print_stderr, reason = "errors are reported on standard error")]
fn main() -> ExitCode {
    let cli = Cli::parse();
    logging::start(cli.verbose);
    let result = match cli.command {
        Command::Dump(args) => dump::run(&args),
        Command::Stats(args) => stats::run(&args),
        Command::Seq(args) => seq::run(&args),
        #[cfg(feature = "sync")]
        Command::Sync(args) => sync::run(&args),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        // The reader of the output went away, as `head` does once it has its lines: nothing is left to do.
        Err(e) if is_broken_pipe(&e) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("osmflow: {}", message(&e));
            ExitCode::FAILURE
        }
    }
}

/// The error and its causes, joined by `: `, without a cause the message before it already ends with: some errors put
/// their source in their own message, which `{e:#}` would print twice.
fn message(e: &anyhow::Error) -> String {
    let mut out = String::new();
    for cause in e.chain() {
        let s = cause.to_string();
        if !out.ends_with(&s) {
            if !out.is_empty() {
                out.push_str(": ");
            }
            out.push_str(&s);
        }
    }
    out
}

/// Whether `e` comes from writing to a pipe whose reader has closed it.
fn is_broken_pipe(e: &anyhow::Error) -> bool {
    e.chain()
        .filter_map(|cause| cause.downcast_ref::<io::Error>())
        .any(|cause| cause.kind() == io::ErrorKind::BrokenPipe)
}

#[cfg(test)]
mod tests {
    use std::error::Error;
    use std::fmt;

    use anyhow::Context as _;
    use clap::CommandFactory as _;
    use osmflow::osc::COMMIT_DELAY;

    use super::*;

    #[test]
    fn a_cause_in_its_errors_message_is_printed_once() {
        /// An error that puts its source in its message, and returns it as its source too.
        #[derive(Debug)]
        struct Wrapped(io::Error);
        impl fmt::Display for Wrapped {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "a.osc.gz: {}", self.0)
            }
        }
        impl Error for Wrapped {
            fn source(&self) -> Option<&(dyn Error + 'static)> {
                Some(&self.0)
            }
        }
        let e = Err::<(), _>(Wrapped(io::Error::other("gone")))
            .context("updating")
            .unwrap_err();
        assert_eq!(message(&e), "updating: a.osc.gz: gone");
    }

    #[test]
    fn the_help_and_readme_give_the_commit_delay() {
        let delay = format!("{} minutes", COMMIT_DELAY.as_secs() / 60);
        let words = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
        let mut cli = Cli::command();
        for name in ["dump", "stats"] {
            let help = cli
                .find_subcommand_mut(name)
                .unwrap()
                .render_long_help()
                .to_string();
            assert!(words(&help).contains(&delay), "{name}: {help}");
        }
        assert!(words(include_str!("../README.md")).contains(&delay));
    }

    #[test]
    fn broken_pipes_are_recognized_through_context() {
        let broken = Err::<(), _>(io::Error::from(io::ErrorKind::BrokenPipe)).context("writing");
        assert!(is_broken_pipe(&broken.unwrap_err()));
        let other = Err::<(), _>(io::Error::from(io::ErrorKind::NotFound)).context("writing");
        assert!(!is_broken_pipe(&other.unwrap_err()));
    }
}
