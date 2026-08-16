//! Concurrent review and deployment coordination.

mod activation;
mod operations;
mod review;
mod rollback;

use std::{
    io::{self, IsTerminal},
    num::NonZeroU32,
};

use dialoguer::{Confirm, console::style, theme::ColorfulTheme};
use futures::future::join_all;
use tabled::Table;

use self::{
    activation::Summary as ActivationSummary,
    review::{HostRow, ReviewOutcome, ReviewedHost},
};
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
    /// Configures work performed after approval.
    pub(crate) activation: ActivationOptions,
}

/// Configures activation, verification, rollback, and cleanup.
#[derive(Clone, Copy)]
pub(crate) struct ActivationOptions {
    /// Selects whether a configured verifier is included.
    pub(crate) include_verifier: bool,
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

    let activation = activate(outdated, options.activation, logs).await;
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
    let progress = status::review(hosts.len(), update_flakes)?;

    let deployed = join_all(
        hosts
            .iter()
            .map(|host| review::deployed(host, logs, &progress)),
    );
    let local = async {
        let updates = if update_flakes {
            update::flakes(flakes, logs, &progress).await
        } else {
            UpdateOutcome::default()
        };
        let expected = join_all(hosts.iter().map(|host| async {
            if updates.failed(host.flake_path()) {
                review::skip_expected(host, &progress);
                None
            } else {
                Some(review::expected(host, logs, &progress).await)
            }
        }))
        .await;
        (updates.into_error(), expected)
    };
    let (deployed, (update_error, expected)) = tokio::join!(deployed, local);

    let mut rows = Vec::with_capacity(hosts.len());
    let mut outdated = Vec::new();
    let mut up_to_date = 0;
    let mut failed = 0;
    let mut first_error = update_error;
    for ((host, deployed), expected) in hosts.into_iter().zip(deployed).zip(expected) {
        let failed_row = HostRow::failed(&host);
        match (deployed, expected) {
            (Ok(deployed), Some(Ok(expected))) => match review::review(host, deployed, &expected) {
                ReviewOutcome::UpToDate(row) => {
                    rows.push(row);
                    up_to_date += 1;
                }
                ReviewOutcome::OutOfDate(row, host) => {
                    rows.push(row);
                    outdated.push(host);
                }
            },
            (Err(error), _) | (_, Some(Err(error))) => {
                rows.push(failed_row);
                failed += 1;
                first_error.get_or_insert(error);
            }
            (_, None) => {
                rows.push(failed_row);
                failed += 1;
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

/// Tests, verifies, and persistently activates all reviewed hosts concurrently.
async fn activate(
    reviewed: Vec<ReviewedHost>,
    options: ActivationOptions,
    logs: &RunLog,
) -> Result<(), Error> {
    let display = ProgressDisplay::new()?;
    let progresses = display.hosts(reviewed.iter().map(ReviewedHost::id));
    let outcomes = join_all(reviewed.into_iter().zip(progresses.iter()).map(
        |(host, progress)| async move {
            let id = host.id().to_owned();
            let outcome = activation::activate(host, options, logs, progress).await;
            outcome.finish_progress(&id, progress);
            outcome
        },
    ))
    .await;

    let mut summary = ActivationSummary::default();
    for outcome in outcomes {
        summary.record(outcome);
    }
    println!("{summary}");
    summary.result()
}
