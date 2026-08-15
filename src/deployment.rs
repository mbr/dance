//! Concurrent review and deployment coordination.

mod host;

use std::{
    io::{self, IsTerminal},
    num::NonZeroU32,
};

use dialoguer::{Confirm, console::style, theme::ColorfulTheme};
use futures::future::join_all;
use tabled::Table;

use self::host::{ActivationOutcome, HostRow, ReviewOutcome, ReviewedHost};
use crate::{
    error::Error,
    inventory::Inventory,
    logs::RunLog,
    status::{self, ProgressDisplay},
    update::{self, UpdateOutcome},
};

/// Configures review, approval, and cleanup for a deployment.
#[derive(Clone, Copy)]
pub(crate) struct DeployOptions {
    /// Selects whether flake inputs are updated during review.
    pub(crate) update_flakes: bool,
    /// Selects whether interactive approval has been bypassed.
    pub(crate) approved: bool,
    /// Selects whether configured verifier outputs are skipped.
    pub(crate) skip_verifier: bool,
    /// Selects whether failed deployments restore their reviewed systems.
    pub(crate) rollback: bool,
    /// Selects post-deployment cleanup and generation retention.
    pub(crate) keep_generations: Option<NonZeroU32>,
}

/// Contains the actionable hosts and any failure found during review.
struct ReviewResult {
    /// Contains hosts whose desired systems differ from their active systems.
    outdated: Vec<ReviewedHost>,
    /// Contains the first failure encountered while collecting information.
    first_error: Option<Error>,
}

/// Reviews the selected hosts without activating them.
pub(crate) async fn review(
    inventory: Inventory,
    update_flakes: bool,
    logs: &RunLog,
) -> Result<(), Error> {
    let result = collect(inventory, update_flakes, logs).await?;
    result.first_error.map_or(Ok(()), Err)
}

/// Reviews and deploys the selected hosts.
pub(crate) async fn deploy(
    inventory: Inventory,
    options: DeployOptions,
    logs: &RunLog,
) -> Result<(), Error> {
    let ReviewResult {
        outdated,
        first_error,
    } = collect(inventory, options.update_flakes, logs).await?;
    if outdated.is_empty() {
        return first_error.map_or(Ok(()), Err);
    }

    if !options.approved && !confirm()? {
        return first_error.map_or(Ok(()), Err);
    }

    let activation = activate(
        outdated,
        options.keep_generations,
        !options.skip_verifier,
        options.rollback,
        logs,
    )
    .await;
    first_error.map_or(activation, Err)
}

/// Collects and displays the active and desired state of selected hosts.
async fn collect(
    inventory: Inventory,
    update_flakes: bool,
    logs: &RunLog,
) -> Result<ReviewResult, Error> {
    let flakes = inventory.flake_groups();
    let hosts = inventory.into_iter().collect::<Vec<_>>();
    let mut rows = hosts.iter().map(HostRow::collecting).collect::<Vec<_>>();
    let progress = status::review(hosts.len(), update_flakes)?;

    let deployed = join_all(
        hosts
            .iter()
            .map(|host| host::deployed(host, logs, &progress)),
    );
    let local = async {
        let updates = if update_flakes {
            update::flakes(flakes, logs, &progress).await
        } else {
            UpdateOutcome::default()
        };
        let expected = join_all(hosts.iter().map(|host| async {
            if updates.failed(host.flake_path()) {
                host::skip_expected(host, &progress);
                None
            } else {
                Some(host::expected(host, logs, &progress).await)
            }
        }))
        .await;
        (updates, expected)
    };
    let (deployed, (mut updates, expected)) = tokio::join!(deployed, local);

    let mut outdated = Vec::new();
    let mut up_to_date = 0;
    let mut failed = 0;
    let mut first_error = updates.take_error();
    for (((row, host), deployed), expected) in
        rows.iter_mut().zip(hosts).zip(deployed).zip(expected)
    {
        let outcome = match deployed {
            Err(error) => Err(error),
            Ok(deployed) => match expected {
                Some(Ok(expected)) => Ok(host::review(host, deployed, expected)),
                Some(Err(error)) => Err(error),
                None => {
                    row.fail();
                    failed += 1;
                    continue;
                }
            },
        };

        match outcome {
            Ok(ReviewOutcome::UpToDate(host_row)) => {
                *row = host_row;
                up_to_date += 1;
            }
            Ok(ReviewOutcome::OutOfDate(host_row, host)) => {
                *row = host_row;
                outdated.push(*host);
            }
            Err(error) => {
                row.fail();
                failed += 1;
                first_error.get_or_insert(error);
            }
        }
    }

    progress.finish_and_clear();
    rows.sort_by_key(HostRow::needs_activation);
    if io::stdout().is_terminal() {
        println!("{}", Table::new(rows));
    } else {
        println!(
            "review: {up_to_date} up to date, {} out of date, {failed} failed",
            outdated.len()
        );
    }

    Ok(ReviewResult {
        outdated,
        first_error,
    })
}

/// Requests approval for an interactive deployment.
fn confirm() -> Result<bool, Error> {
    if !(io::stdin().is_terminal() && io::stderr().is_terminal()) {
        return Err(Error::ConfirmationRequired);
    }

    let theme = ColorfulTheme {
        success_prefix: style(">".to_owned()).for_stderr().green(),
        ..ColorfulTheme::default()
    };
    Confirm::with_theme(&theme)
        .with_prompt("Deploy the outdated systems?")
        .default(false)
        .interact()
        .map_err(|source| Error::Confirmation { source })
}

/// Builds, prepares, and activates all reviewed hosts concurrently.
async fn activate(
    reviewed: Vec<ReviewedHost>,
    keep_generations: Option<NonZeroU32>,
    include_verifier: bool,
    rollback: bool,
    logs: &RunLog,
) -> Result<(), Error> {
    let display = ProgressDisplay::new()?;
    let progresses = display.hosts(reviewed.iter().map(ReviewedHost::id));
    let outcomes = join_all(reviewed.into_iter().zip(progresses.iter()).map(
        |(host, progress)| async move {
            let outcome = host::activate(
                host,
                keep_generations,
                include_verifier,
                rollback,
                logs,
                progress,
            )
            .await;
            if !matches!(&outcome, ActivationOutcome::Activated) {
                status::fail(progress);
            }
            outcome
        },
    ))
    .await;

    let mut activated = 0;
    let mut failed = 0;
    let mut verification_failed = 0;
    let mut rolled_back = 0;
    let mut rollback_pending = 0;
    let mut cleanup_failed = 0;
    let mut first_error = None;
    for outcome in outcomes {
        match outcome {
            ActivationOutcome::Activated => activated += 1,
            ActivationOutcome::ActivationFailed(error) => {
                failed += 1;
                first_error.get_or_insert(error);
            }
            ActivationOutcome::VerificationFailed(error) => {
                activated += 1;
                verification_failed += 1;
                first_error.get_or_insert(error);
            }
            ActivationOutcome::RolledBack(error) => {
                rolled_back += 1;
                first_error.get_or_insert(error);
            }
            ActivationOutcome::RollbackPending(error) => {
                rollback_pending += 1;
                first_error.get_or_insert(error);
            }
            ActivationOutcome::CleanupFailed(error) => {
                activated += 1;
                cleanup_failed += 1;
                first_error.get_or_insert(error);
            }
        }
    }
    let mut summary = format!("deployment: {activated} activated, {failed} failed");
    if verification_failed != 0 {
        summary.push_str(&format!(", {verification_failed} verification failed"));
    }
    if rolled_back != 0 {
        summary.push_str(&format!(", {rolled_back} rolled back"));
    }
    if rollback_pending != 0 {
        summary.push_str(&format!(", {rollback_pending} rollback pending"));
    }
    if cleanup_failed != 0 {
        summary.push_str(&format!(", {cleanup_failed} cleanup failed"));
    }
    println!("{summary}");

    first_error.map_or(Ok(()), Err)
}
