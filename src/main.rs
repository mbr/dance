//! NixOS deployment orchestration from a local inventory.

mod deployment;
mod error;
mod inventory;
mod logs;
mod process;
mod status;
mod store_path;
mod update;

use std::{path::PathBuf, process::ExitCode};

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
    let inventory = Inventory::load(&cli.inventory)?.select(&cli.ids)?;
    deployment::deploy(inventory, cli.update, logs).await
}

/// Reports whether the deployment workflow succeeded.
#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let logs = match RunLog::start(cli.logs, cli.logs_dir.as_deref(), &cli.inventory) {
        Ok(logs) => logs,
        Err(error) => {
            eprintln!("error: {}", error.display_full());
            println!("status: failed");
            return ExitCode::FAILURE;
        }
    };
    let result = run(cli, &logs).await;
    let error_message = result
        .as_ref()
        .err()
        .map(|error| error.display_full().to_string());
    let log_directory = logs.finish(error_message.as_deref());

    match (result, log_directory) {
        (Ok(()), Ok(path)) => {
            println!("status: succeeded");
            if let Some(path) = path {
                println!("logs: {}", path.display());
            }
            ExitCode::SUCCESS
        }
        (Err(error), Ok(path)) => {
            eprintln!("error: {}", error.display_full());
            println!("status: failed");
            if let Some(path) = path {
                println!("logs: {}", path.display());
            }
            ExitCode::FAILURE
        }
        (Ok(()), Err(error)) => {
            eprintln!("error: {}", error.display_full());
            println!("status: failed");
            ExitCode::FAILURE
        }
        (Err(workflow), Err(logs)) => {
            eprintln!("error: {}", workflow.display_full());
            eprintln!("error: {}", logs.display_full());
            println!("status: failed");
            ExitCode::FAILURE
        }
    }
}
