//! Concurrent review and deployment coordination.

mod host;
mod operations;
mod review;
mod rollback;

use std::{
    fmt::{self, Display, Formatter},
    io::{self, IsTerminal},
    num::NonZeroU32,
};

use dialoguer::{Confirm, console::style, theme::ColorfulTheme};
use futures::future::join_all;
use tabled::Table;

use self::{
    host::{LockedHost, Outcome as HostOutcome, ValidatedHost},
    review::{HostRow, ReviewOutcome, ReviewedHost},
};
use crate::{
    error::Error,
    git::PendingUpdates,
    inventory::Inventory,
    process::ProcessContext,
    status::{self, ProgressDisplay},
    update::{self, UpdateOutcome},
};

/// Configures review, approval, and cleanup for a deployment.
#[derive(Clone, Copy)]
pub(crate) struct DeployOptions {
    /// Selects whether flake inputs are updated during review.
    pub(crate) update_flakes: bool,
    /// Selects whether changed flake locks are committed after deployment.
    pub(crate) commit_flakes: bool,
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
    /// Contains updated locks awaiting successful deployment.
    pending_updates: PendingUpdates,
}

/// Aggregates deployment outcomes for terminal output and exit status.
#[derive(Default)]
struct ActivationSummary {
    /// Counts successful activations.
    activated: usize,
    /// Counts deployment failures left in place.
    failed: usize,
    /// Counts failures followed by successful restoration.
    rolled_back: usize,
    /// Counts hosts that need operator recovery.
    recovery_required: usize,
    /// Counts successful activations whose cleanup failed.
    cleanup_failed: usize,
    /// Retains the first deployment error.
    first_error: Option<Error>,
}

impl ActivationSummary {
    /// Adds one host outcome.
    fn record(&mut self, outcome: HostOutcome) {
        let error = match outcome {
            HostOutcome::Activated => {
                self.activated += 1;
                return;
            }
            HostOutcome::Failed(error) => {
                self.failed += 1;
                error
            }
            HostOutcome::RolledBack(error) => {
                self.rolled_back += 1;
                error
            }
            HostOutcome::RecoveryRequired(error) => {
                self.recovery_required += 1;
                error
            }
            HostOutcome::CleanupFailed(error) => {
                self.activated += 1;
                self.cleanup_failed += 1;
                error
            }
        };
        self.first_error.get_or_insert(error);
    }

    /// Returns the first deployment failure when present.
    fn result(self) -> Result<(), Error> {
        self.first_error.map_or(Ok(()), Err)
    }
}

impl Display for ActivationSummary {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "deployment: {} activated, {} failed",
            self.activated, self.failed
        )?;
        for (count, label) in [
            (self.rolled_back, "rolled back"),
            (self.recovery_required, "recovery required"),
            (self.cleanup_failed, "cleanup failed"),
        ] {
            if count != 0 {
                write!(formatter, ", {count} {label}")?;
            }
        }
        Ok(())
    }
}

/// Reviews the selected hosts without activating them.
pub(crate) async fn review(
    inventory: Inventory,
    update_flakes: bool,
    processes: &ProcessContext<'_>,
) -> Result<(), Error> {
    let result = collect(inventory, update_flakes, false, processes).await?;
    result.first_error.map_or(Ok(()), Err)
}

/// Reviews and deploys the selected hosts.
pub(crate) async fn deploy(
    inventory: Inventory,
    options: DeployOptions,
    processes: &ProcessContext<'_>,
) -> Result<(), Error> {
    let ReviewResult {
        outdated,
        first_error,
        pending_updates,
    } = collect(
        inventory,
        options.update_flakes,
        options.commit_flakes,
        processes,
    )
    .await?;
    if let Some(error) = first_error {
        return Err(error);
    }
    if outdated.is_empty() {
        return pending_updates.commit(processes).await;
    }

    if !options.approved && !confirm()? {
        return Ok(());
    }

    activate_hosts(outdated, options.activation, processes).await?;
    pending_updates.commit(processes).await
}

/// Collects and displays the active and desired state of selected hosts.
async fn collect(
    inventory: Inventory,
    update_flakes: bool,
    commit_flakes: bool,
    processes: &ProcessContext<'_>,
) -> Result<ReviewResult, Error> {
    let flakes = inventory.flake_groups();
    let hosts = inventory.into_iter().collect::<Vec<_>>();
    let progress = status::review(hosts.len(), update_flakes)?;

    let deployed = join_all(
        hosts
            .iter()
            .map(|host| review::deployed(host, processes, &progress)),
    );
    let local = async {
        let updates = if update_flakes {
            update::flakes(flakes, commit_flakes, processes, &progress).await
        } else {
            UpdateOutcome::default()
        };
        let expected = join_all(hosts.iter().map(|host| async {
            if updates.failed(host.flake_path()) {
                review::skip_expected(host, &progress);
                None
            } else {
                Some(review::expected(host, processes, &progress).await)
            }
        }))
        .await;
        (updates, expected)
    };
    let (deployed, (updates, expected)) = tokio::join!(deployed, local);
    let (update_error, pending_updates) = updates.into_parts();

    let mut rows = Vec::with_capacity(hosts.len());
    let mut outdated = Vec::new();
    let mut up_to_date = 0;
    let mut failed = 0;
    let mut first_error = update_error;
    for ((host, deployed), expected) in hosts.into_iter().zip(deployed).zip(expected) {
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
                rows.push(HostRow::failed(&host));
                failed += 1;
                first_error.get_or_insert(error);
            }
            (_, None) => {
                rows.push(HostRow::failed(&host));
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
        pending_updates,
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

/// Acquires every host lock without beginning partial activation.
async fn acquire_hosts(
    reviewed: Vec<ReviewedHost>,
    progresses: &[indicatif::ProgressBar],
) -> Result<Vec<LockedHost>, Error> {
    let results = join_all(
        reviewed
            .into_iter()
            .zip(progresses)
            .map(|(host, progress)| LockedHost::acquire(host, progress)),
    )
    .await;
    let mut locked = Vec::with_capacity(results.len());
    let mut first_error = None;
    for result in results {
        match result {
            Ok(host) => locked.push(host),
            Err(error) => {
                first_error.get_or_insert(error);
            }
        }
    }
    if let Some(error) = first_error {
        for progress in progresses {
            status::fail(progress);
        }
        Err(error)
    } else {
        Ok(locked)
    }
}

/// Revalidates every locked host before allowing any activation.
async fn revalidate_hosts(
    locked: Vec<LockedHost>,
    progresses: &[indicatif::ProgressBar],
    processes: &ProcessContext<'_>,
) -> Result<Vec<ValidatedHost>, Error> {
    let results = join_all(
        locked
            .into_iter()
            .zip(progresses)
            .map(|(host, progress)| host.revalidate(processes, progress)),
    )
    .await;
    let mut validated = Vec::with_capacity(results.len());
    let mut first_error = None;
    for result in results {
        match result {
            Ok(host) => validated.push(host),
            Err(error) => {
                first_error.get_or_insert(error);
            }
        }
    }
    if let Some(error) = first_error {
        for progress in progresses {
            status::fail(progress);
        }
        Err(error)
    } else {
        Ok(validated)
    }
}

/// Tests, verifies, and persistently activates all reviewed hosts concurrently.
async fn activate_hosts(
    reviewed: Vec<ReviewedHost>,
    options: ActivationOptions,
    processes: &ProcessContext<'_>,
) -> Result<(), Error> {
    let display = ProgressDisplay::new()?;
    let progresses = display.hosts(reviewed.iter().map(ReviewedHost::id));
    let locked = acquire_hosts(reviewed, &progresses).await?;
    let validated = revalidate_hosts(locked, &progresses, processes).await?;
    let outcomes = join_all(
        validated
            .into_iter()
            .zip(progresses.iter())
            .map(|(host, progress)| host.activate(options, processes, progress)),
    )
    .await;

    let mut summary = ActivationSummary::default();
    for outcome in outcomes {
        summary.record(outcome);
    }
    println!("{summary}");
    summary.result()
}
