//! NixOS deployment orchestration from a local inventory.

mod cli;
mod deployment;
mod error;
mod git;
mod inventory;
mod lock;
mod logs;
mod process;
mod status;
mod store_path;
mod update;
mod verification;

use std::{path::PathBuf, process::ExitCode};

use clap::Parser;
use display_full_error::DisplayFullErrorExt;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::{EnvFilter, filter::LevelFilter};

use crate::{
    cli::{Cli, Command},
    error::Error,
    inventory::Inventory,
    logs::RunLog,
    process::ProcessContext,
};

/// Runs the selected workflow.
async fn run(cli: Cli, processes: &ProcessContext<'_>) -> Result<(), Error> {
    let inventory = Inventory::load(&cli.inventory).map_err(Error::Inventory)?;
    match cli.command {
        Command::Review(args) => {
            let inventory = inventory.select(&args.ids).map_err(Error::Inventory)?;
            deployment::review(inventory, args.update, processes).await
        }
        Command::Deploy(args) => {
            let inventory = inventory
                .select(&args.workflow.ids)
                .map_err(Error::Inventory)?;
            let options = deployment::DeployOptions {
                update_flakes: args.update_flakes(),
                commit_flakes: args.commit_flakes(),
                approved: args.yes,
                activation: deployment::ActivationOptions {
                    include_verifier: !args.skip_verifier,
                    rollback: !args.no_rollback,
                    keep_generations: (!args.no_gc).then_some(args.keep_generations),
                },
            };
            deployment::deploy(inventory, options, processes).await
        }
        Command::Verify(args) => {
            let inventory = inventory.select(&args.ids).map_err(Error::Inventory)?;
            verification::verify(inventory, processes).await
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
    let mut logs = match RunLog::start(cli.logs, cli.logs_dir.as_deref()) {
        Ok(logs) => logs,
        Err(error) => return report(Err(Error::Log(error)), Ok(None)),
    };
    if let Some(writer) = logs.take_writer() {
        let filter = EnvFilter::builder()
            .with_default_directive(LevelFilter::INFO.into())
            .from_env_lossy();
        let subscriber = tracing_subscriber::fmt()
            .compact()
            .with_ansi(false)
            .with_env_filter(filter)
            .with_target(false)
            .with_writer(writer)
            .finish();
        if let Err(source) = tracing::subscriber::set_global_default(subscriber) {
            let error = Error::InstallTracing { source };
            let error_message = error.display_full().to_string();
            return report(
                Err(error),
                logs.finish(Some(&error_message)).map_err(Error::Log),
            );
        }
        logs.record_start(&cli.inventory);
    }
    let result = {
        let cancellation = CancellationToken::new();
        let processes = ProcessContext::new(&logs, cancellation.clone());
        let workflow = run(cli, &processes);
        tokio::pin!(workflow);
        tokio::select! {
            result = &mut workflow => result,
            signal = tokio::signal::ctrl_c() => {
                match signal {
                    Ok(()) => cancellation.cancel(),
                    Err(source) => eprintln!("error: failed to listen for interruption: {source}"),
                }
                workflow.await
            }
        }
    };
    let error_message = result
        .as_ref()
        .err()
        .map(|error| error.display_full().to_string());
    let log_directory = logs.finish(error_message.as_deref()).map_err(Error::Log);
    report(result, log_directory)
}
