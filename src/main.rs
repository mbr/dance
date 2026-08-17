//! NixOS deployment orchestration from a local inventory.

mod cli;
mod deployment;
mod error;
mod inventory;
mod logs;
mod process;
mod status;
mod store_path;
mod update;
mod verification;

use std::{path::PathBuf, process::ExitCode};

use clap::Parser;
use display_full_error::DisplayFullErrorExt;

use crate::{
    cli::{Cli, Command},
    error::Error,
    inventory::Inventory,
    logs::RunLog,
};

/// Runs the selected workflow.
async fn run(cli: Cli, logs: &RunLog) -> Result<(), Error> {
    let inventory = Inventory::load(&cli.inventory).map_err(Error::InventoryLoad)?;
    match cli.command {
        Command::Review(args) => {
            let inventory = inventory.select(&args.ids)?;
            deployment::review(inventory, args.update, logs).await
        }
        Command::Deploy(args) => {
            let inventory = inventory.select(&args.workflow.ids)?;
            let options = deployment::DeployOptions {
                update_flakes: args.workflow.update,
                approved: args.yes,
                activation: deployment::ActivationOptions {
                    include_verifier: !args.skip_verifier,
                    rollback: !args.no_rollback,
                    keep_generations: (!args.no_gc).then_some(args.keep_generations),
                },
            };
            deployment::deploy(inventory, options, logs).await
        }
        Command::Verify(args) => {
            let inventory = inventory.select(&args.ids)?;
            verification::verify(inventory, logs).await
        }
    }
}

/// Reports a workflow result and its retained diagnostic directory.
fn report(result: Result<(), Error>, logs: Result<Option<PathBuf>, Error>) -> ExitCode {
    let succeeded = result.is_ok() && logs.is_ok();
    if let Err(error) = result {
        eprintln!("error: {}", error.display_full());
    }
    let path = match logs {
        Ok(path) => path,
        Err(error) => {
            eprintln!("error: {}", error.display_full());
            None
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
