//! NixOS deployment orchestration from a local inventory.

mod deployment;
mod error;
mod inventory;
mod process;
mod status;
mod store_path;
mod update;

use std::{path::PathBuf, process::ExitCode};

use clap::Parser;
use display_full_error::DisplayFullErrorExt;

use crate::{error::Error, inventory::Inventory};

/// Deploys NixOS hosts from an inventory.
#[derive(Debug, Parser)]
#[command(version)]
struct Cli {
    /// Selects the host inventory.
    #[arg(long, env = "DANCE_INVENTORY", default_value = "inventory.toml")]
    inventory: PathBuf,
    /// Updates selected flakes before reviewing hosts.
    #[arg(long)]
    update: bool,
    /// Limits deployment to these inventory host names.
    #[arg(value_name = "HOST")]
    hosts: Vec<String>,
}

/// Runs the deployment workflow.
async fn run() -> Result<(), Error> {
    let cli = Cli::parse();
    let inventory = Inventory::load(&cli.inventory)?.select(&cli.hosts)?;
    if cli.update {
        update::flakes(&inventory).await?;
    }
    deployment::deploy(inventory).await
}

/// Reports whether the deployment workflow succeeded.
#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {}", error.display_full());
            ExitCode::FAILURE
        }
    }
}
