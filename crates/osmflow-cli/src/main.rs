//! The `osmflow` command: dumps OpenStreetMap PBF files as text or GeoJSON, and summarizes them.

use std::io;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

mod dump;
mod output;
mod select;
mod stats;

#[derive(Debug, Parser)]
#[command(name = "osmflow", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Writes the objects of a PBF file as text or GeoJSON, all or only those chosen by ID, type and tag.
    Dump(dump::Args),
    /// Summarizes a PBF file: its header, blocks, object counts, ID ranges, extent and time span.
    Stats(stats::Args),
}

#[expect(clippy::print_stderr, reason = "errors are reported on standard error")]
fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Dump(args) => dump::run(&args),
        Command::Stats(args) => stats::run(&args),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        // The reader of the output went away, as `head` does once it has its lines: nothing is left to do.
        Err(e) if is_broken_pipe(&e) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("osmflow: {e:#}");
            ExitCode::FAILURE
        }
    }
}

/// Whether `e` comes from writing to a pipe whose reader has closed it.
fn is_broken_pipe(e: &anyhow::Error) -> bool {
    e.chain()
        .filter_map(|cause| cause.downcast_ref::<io::Error>())
        .any(|cause| cause.kind() == io::ErrorKind::BrokenPipe)
}

#[cfg(test)]
mod tests {
    use anyhow::Context as _;

    use super::*;

    #[test]
    fn broken_pipes_are_recognized_through_context() {
        let broken = Err::<(), _>(io::Error::from(io::ErrorKind::BrokenPipe)).context("writing");
        assert!(is_broken_pipe(&broken.unwrap_err()));
        let other = Err::<(), _>(io::Error::from(io::ErrorKind::NotFound)).context("writing");
        assert!(!is_broken_pipe(&other.unwrap_err()));
    }
}
