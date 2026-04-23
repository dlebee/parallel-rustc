//! parallel-rustc: wavefront parallel compilation planner for Cargo workspaces.
//!
//! v0.0.0: compute the compile-order DAG from `cargo metadata` and print
//! the parallel phases. No actual rustc invocation yet — see spec/0.0.0.md.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

mod graph;
mod metadata;
mod plan;

#[derive(Parser, Debug)]
#[command(
    name = "parallel-rustc",
    version,
    about = "Wavefront parallel compilation planner for Cargo workspaces"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Compute and print the parallel compilation plan (phases).
    Plan {
        /// Path to Cargo.toml (workspace root or package).
        #[arg(long, value_name = "PATH")]
        manifest_path: Option<PathBuf>,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Command::Plan { manifest_path } => match run_plan(manifest_path.as_deref()) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("parallel-rustc: error: {e}");
                ExitCode::FAILURE
            }
        },
    }
}

fn run_plan(manifest_path: Option<&std::path::Path>) -> Result<(), String> {
    let meta = metadata::load(manifest_path).map_err(|e| format!("cargo metadata: {e}"))?;
    let dag = graph::build(&meta).map_err(|e| format!("graph build: {e}"))?;
    let phases = graph::phases(&dag).map_err(|e| format!("phase computation: {e}"))?;
    plan::print(&meta, &dag, &phases);
    Ok(())
}
