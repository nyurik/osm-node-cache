//! The `osmflow` command: dumps OpenStreetMap PBF files as text or GeoJSON, and summarizes them.

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
        Err(e) => {
            eprintln!("osmflow: {e:#}");
            ExitCode::FAILURE
        }
    }
}
