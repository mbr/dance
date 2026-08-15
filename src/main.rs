//! NixOS deployment orchestration from a local inventory.

mod deployment;
mod error;
mod inventory;
mod logs;
mod process;
mod status;
mod store_path;
mod update;

use std::{num::NonZeroU32, path::PathBuf, process::ExitCode};

use clap::Parser;
use display_full_error::DisplayFullErrorExt;

use crate::{
    error::Error,
    inventory::Inventory,
    logs::{LogMode, RunLog},
};

/// Deploys NixOS hosts from an inventory.
#[derive(Debug, Parser)]
#[command(version)]
struct Cli {
    /// Selects the host inventory.
    #[arg(long, env = "DANCE_INVENTORY", default_value = "inventory.toml")]
    inventory: PathBuf,
    /// Updates selected flakes during review.
    #[arg(long)]
    update: bool,
    /// Deploys without requesting confirmation.
    #[arg(short, long, conflicts_with = "review_only")]
    yes: bool,
    /// Stops after updating and collecting host information.
    #[arg(long)]
    review_only: bool,
    /// Retains this many system generations after deployment.
    #[arg(long, default_value = "2")]
    keep_generations: NonZeroU32,
    /// Skips post-deployment garbage collection.
    #[arg(long, conflicts_with = "keep_generations")]
    no_gc: bool,
    /// Selects when command logs are retained.
    #[arg(long, env = "DANCE_LOGS", value_enum, default_value = "on-error")]
    logs: LogMode,
    /// Overrides the command log directory.
    #[arg(long, env = "DANCE_LOGS_DIR")]
    logs_dir: Option<PathBuf>,
    /// Limits deployment to these inventory IDs.
    #[arg(value_name = "ID")]
    ids: Vec<String>,
}

/// Runs the deployment workflow.
async fn run(cli: Cli, logs: &RunLog) -> Result<(), Error> {
    let mode = if cli.yes {
        deployment::Mode::Approved
    } else if cli.review_only {
        deployment::Mode::ReviewOnly
    } else {
        deployment::Mode::Confirm
    };
    let options = deployment::Options {
        update_flakes: cli.update,
        mode,
        keep_generations: (!cli.no_gc).then_some(cli.keep_generations),
    };
    let inventory = Inventory::load(&cli.inventory)?.select(&cli.ids)?;
    deployment::deploy(inventory, options, logs).await
}

/// Reports a workflow result and its retained diagnostic directory.
fn report(result: Result<(), Error>, logs: Result<Option<PathBuf>, Error>) -> ExitCode {
    let (succeeded, path) = match (result, logs) {
        (Ok(()), Ok(path)) => (true, path),
        (Err(error), Ok(path)) => {
            eprintln!("error: {}", error.display_full());
            (false, path)
        }
        (Ok(()), Err(error)) => {
            eprintln!("error: {}", error.display_full());
            (false, None)
        }
        (Err(workflow), Err(logs)) => {
            eprintln!("error: {}", workflow.display_full());
            eprintln!("error: {}", logs.display_full());
            (false, None)
        }
    };
    println!("status: {}", if succeeded { "succeeded" } else { "failed" });
    if let Some(path) = path {
        println!("logs: {}", path.display());
    }
    if succeeded {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// Reports whether the deployment workflow succeeded.
#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let logs = match RunLog::start(cli.logs, cli.logs_dir.as_deref(), &cli.inventory) {
        Ok(logs) => logs,
        Err(error) => return report(Err(error), Ok(None)),
    };
    let result = run(cli, &logs).await;
    let error_message = result
        .as_ref()
        .err()
        .map(|error| error.display_full().to_string());
    let log_directory = logs.finish(error_message.as_deref());
    report(result, log_directory)
}
