//! parallel-rustc: wavefront parallel compilation for Cargo workspaces.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

use parallel_rustc::builder::BuildConfig;
use parallel_rustc::{bench, builder, graph, metadata, plan};

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
        #[arg(long, value_name = "PATH")]
        manifest_path: Option<PathBuf>,

        /// Show only workspace members in the plan output, not external dependencies.
        #[arg(long, default_value_t = false)]
        workspace_only: bool,
    },
    /// Build the workspace using phase-driven parallelism.
    ///
    /// Default strategy is v0.2.0: record rustc invocations via
    /// `parallel-rustc-wrapper` and replay them in parallel phases. Pass
    /// `--strategy v1` to use the older per-crate `cargo build -p` approach.
    Build {
        #[arg(long, value_name = "PATH")]
        manifest_path: Option<PathBuf>,

        #[arg(long, default_value_t = false)]
        release: bool,

        /// Max parallel processes per phase.
        #[arg(short = 'j', long, default_value_t = default_jobs())]
        jobs: usize,

        #[arg(long, default_value_t = false)]
        workspace_only: bool,

        /// Build strategy: "v2" (RUSTC_WRAPPER record/replay, default) or
        /// "v1" (per-crate `cargo build -p`).
        #[arg(long, default_value = "v2")]
        strategy: String,
    },
    /// Cold-build the workspace three ways (serial, cargo -jN, parallel-rustc v2)
    /// and print a comparison table.
    Bench {
        #[arg(long, value_name = "PATH")]
        manifest_path: Option<PathBuf>,

        #[arg(long, default_value_t = false)]
        release: bool,

        #[arg(short = 'j', long, default_value_t = default_jobs())]
        jobs: usize,

        #[arg(long, default_value_t = false)]
        workspace_only: bool,
    },
}

fn default_jobs() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let rt = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("parallel-rustc: failed to start tokio runtime: {e}");
            return ExitCode::FAILURE;
        }
    };

    let result: Result<(), String> = rt.block_on(async move {
        match cli.command {
            Command::Plan { manifest_path, workspace_only } => {
                run_plan(manifest_path.as_deref(), workspace_only)
            }
            Command::Build { manifest_path, release, jobs, workspace_only, strategy } => {
                let config = BuildConfig {
                    manifest_path,
                    release,
                    jobs,
                    workspace_only,
                };
                run_build_cmd(&config, &strategy).await
            }
            Command::Bench { manifest_path, release, jobs, workspace_only } => {
                let config = BuildConfig {
                    manifest_path,
                    release,
                    jobs,
                    workspace_only,
                };
                run_bench_cmd(&config).await
            }
        }
    });

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("parallel-rustc: error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run_plan(manifest_path: Option<&std::path::Path>, workspace_only: bool) -> Result<(), String> {
    let meta = metadata::load(manifest_path).map_err(|e| format!("cargo metadata: {e}"))?;
    let dag = graph::build(&meta).map_err(|e| format!("graph build: {e}"))?;
    let phases = graph::phases(&dag).map_err(|e| format!("phase computation: {e}"))?;
    plan::print(&meta, &dag, &phases, workspace_only);
    Ok(())
}

async fn run_build_cmd(config: &BuildConfig, strategy: &str) -> Result<(), String> {
    match strategy {
        "v2" | "wrapper" => {
            builder::run_build_v2(config).await.map(|_| ())
        }
        "v1" | "cargo" => {
            let meta = metadata::load(config.manifest_path.as_deref())
                .map_err(|e| format!("cargo metadata: {e}"))?;
            let dag = graph::build(&meta).map_err(|e| format!("graph build: {e}"))?;
            let phases = graph::phases(&dag).map_err(|e| format!("phase computation: {e}"))?;
            builder::run_build(&meta, &dag, &phases, config).await.map(|_| ())
        }
        other => Err(format!("unknown --strategy {other}; expected v1|v2")),
    }
}

async fn run_bench_cmd(config: &BuildConfig) -> Result<(), String> {
    let meta = metadata::load(config.manifest_path.as_deref()).map_err(|e| format!("cargo metadata: {e}"))?;
    let dag = graph::build(&meta).map_err(|e| format!("graph build: {e}"))?;
    let phases = graph::phases(&dag).map_err(|e| format!("phase computation: {e}"))?;
    bench::run_bench(&meta, &dag, &phases, config).await
}
