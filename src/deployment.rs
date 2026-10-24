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
    host::Outcome as HostOutcome,
    review::{HostRow, ReviewOutcome, ReviewedHost, Validity},
};
use crate::{
    error::Error,
    inventory::{Host, Inventory},
    process::ProcessContext,
    status::{self, ProgressDisplay},
    update::{self, UpdateOutcome},
    verification::ConfiguredOutcome,
};

/// Configures review, approval, and cleanup for a deployment.
#[derive(Clone, Copy)]
pub(crate) struct DeployOptions {
    /// Selects whether flake inputs are updated during review.
    pub(crate) update_flakes: bool,
    /// Selects whether changed flake locks are committed before evaluation.
    pub(crate) commit_flakes: bool,
    /// Selects whether interactive approval has been bypassed.
    pub(crate) approved: bool,
    /// Configures work performed after approval.
    pub(crate) host: HostOptions,
}

/// Configures activation, verification, rollback, and cleanup.
#[derive(Clone, Copy)]
pub(crate) struct HostOptions {
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

/// Aggregates deployment outcomes for terminal output and exit status.
#[derive(Default)]
struct DeploymentSummary {
    /// Counts successful activations.
    activated: usize,
    /// Identifies deployment failures left in place.
    failed: Vec<String>,
    /// Identifies failures followed by successful restoration.
    rolled_back: Vec<String>,
    /// Identifies hosts that need operator recovery.
    recovery_required: Vec<String>,
    /// Identifies successful activations whose cleanup failed.
    cleanup_failed: Vec<String>,
    /// Retains the first deployment error.
    first_error: Option<Error>,
}

impl DeploymentSummary {
    /// Adds one host outcome.
    fn record(&mut self, host: String, outcome: HostOutcome) {
        let error = match outcome {
            HostOutcome::Activated => {
                self.activated += 1;
                return;
            }
            HostOutcome::Failed(error) => {
                self.failed.push(host);
                error
            }
            HostOutcome::RolledBack(error) => {
                self.rolled_back.push(host);
                error
            }
            HostOutcome::RecoveryRequired(error) => {
                self.recovery_required.push(host);
                error
            }
            HostOutcome::CleanupFailed(error) => {
                self.activated += 1;
                self.cleanup_failed.push(host);
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

impl Display for DeploymentSummary {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "deployment: {} activated, {} failed",
            self.activated,
            self.failed.len()
        )?;
        for (count, label) in [
            (self.rolled_back.len(), "rolled back"),
            (self.recovery_required.len(), "recovery required"),
            (self.cleanup_failed.len(), "cleanup failed"),
        ] {
            if count != 0 {
                write!(formatter, ", {count} {label}")?;
            }
        }
        for (hosts, label) in [
            (&self.failed, "failed"),
            (&self.rolled_back, "rolled back"),
            (&self.recovery_required, "recovery required"),
            (&self.cleanup_failed, "cleanup failed"),
        ] {
            if !hosts.is_empty() {
                write!(formatter, "\n{label}: {}", hosts.join(", "))?;
            }
        }
        Ok(())
    }
}

/// Reports the selected hosts without activating them.
pub(crate) async fn status(
    inventory: Inventory,
    update_flakes: bool,
    processes: &ProcessContext<'_>,
) -> Result<(), Error> {
    let result = collect(inventory, update_flakes, false, true, processes).await?;
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
    } = collect(
        inventory,
        options.update_flakes,
        options.commit_flakes,
        false,
        processes,
    )
    .await?;
    if outdated.is_empty() {
        return first_error.map_or(Ok(()), Err);
    }

    if !options.approved && !confirm()? {
        return first_error.map_or(Ok(()), Err);
    }

    let deployment = deploy_hosts(outdated, options.host, processes).await;
    if let Some(error) = first_error {
        return Err(error);
    }
    deployment
}

/// Collects and displays the active and desired state of selected hosts.
async fn collect(
    inventory: Inventory,
    update_flakes: bool,
    commit_flakes: bool,
    validate_current: bool,
    processes: &ProcessContext<'_>,
) -> Result<ReviewResult, Error> {
    let flakes = inventory.flake_groups();
    let hosts = inventory.into_iter().collect::<Vec<_>>();
    let display = ProgressDisplay::new()?;
    let progresses = display.hosts(hosts.iter().map(Host::id));
    let initial_phase = if update_flakes {
        status::DeploymentPhase::UpdatingInputs
    } else {
        status::DeploymentPhase::CheckingStatus
    };
    for (host, progress) in hosts.iter().zip(&progresses) {
        status::set_phase(progress, host.id(), initial_phase);
    }
    let flakes = flakes
        .into_iter()
        .map(|(path, _)| {
            let dependent = hosts
                .iter()
                .zip(&progresses)
                .filter(|(host, _)| host.flake_path() == path.as_path())
                .map(|(_, progress)| progress.clone())
                .collect();
            (path, dependent)
        })
        .collect();

    let deployed = join_all(
        hosts
            .iter()
            .zip(&progresses)
            .map(|(host, progress)| review::deployed(host, processes, progress)),
    );
    let local = async {
        let updates = if update_flakes {
            update::flakes(flakes, commit_flakes, processes).await?
        } else {
            UpdateOutcome::default()
        };
        let expected = join_all(hosts.iter().zip(&progresses).map(|(host, progress)| async {
            if updates.failed(host.flake_path()) {
                review::skip_expected(progress);
                None
            } else {
                status::set_phase(progress, host.id(), status::DeploymentPhase::CheckingStatus);
                Some(review::expected(host, processes, progress).await)
            }
        }));
        let validity = async {
            if validate_current {
                join_all(hosts.iter().zip(&progresses).map(|(host, progress)| async {
                    if updates.failed(host.flake_path()) {
                        review::skip_validity(progress);
                        None
                    } else {
                        Some(review::validity(host, processes, progress).await)
                    }
                }))
                .await
            } else {
                std::iter::repeat_with(|| None).take(hosts.len()).collect()
            }
        };
        let (expected, validity) = tokio::join!(expected, validity);
        Ok::<_, Error>((updates, expected, validity))
    };
    let (deployed, local) = tokio::join!(deployed, local);
    let (updates, expected, validity) = local?;
    let update_error = updates.into_error();

    let mut rows = Vec::with_capacity(hosts.len());
    let mut outdated = Vec::new();
    let mut up_to_date = 0;
    let mut failed = 0;
    let mut validation_failed = 0;
    let mut first_error = update_error;
    for (((host, deployed), expected), validity) in
        hosts.into_iter().zip(deployed).zip(expected).zip(validity)
    {
        let valid = match validity {
            Some(Ok(ConfiguredOutcome::Verified)) => Validity::Valid,
            Some(Ok(ConfiguredOutcome::Missing)) => Validity::Missing,
            Some(Err(error)) => {
                validation_failed += 1;
                first_error.get_or_insert(error);
                Validity::Failed
            }
            None => Validity::Unchecked,
        };
        match (deployed, expected) {
            (Ok(deployed), Some(Ok(expected))) => {
                match review::review(host, deployed, &expected, valid) {
                    ReviewOutcome::UpToDate(row) => {
                        rows.push(row);
                        up_to_date += 1;
                    }
                    ReviewOutcome::OutOfDate(row, host) => {
                        rows.push(row);
                        outdated.push(host);
                    }
                }
            }
            (Err(error), _) | (_, Some(Err(error))) => {
                rows.push(HostRow::failed(&host, valid));
                failed += 1;
                first_error.get_or_insert(error);
            }
            (_, None) => {
                rows.push(HostRow::failed(&host, valid));
                failed += 1;
            }
        }
    }

    for progress in progresses {
        progress.finish_and_clear();
    }
    rows.sort_by_key(HostRow::needs_activation);
    if io::stdout().is_terminal() {
        println!("{}", Table::new(rows));
    } else {
        print!(
            "status: {up_to_date} up to date, {} out of date, {failed} failed",
            outdated.len()
        );
        if validate_current {
            print!(", {validation_failed} validation failed");
        }
        println!();
        for row in rows {
            row.report_revision();
        }
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

/// Deploys all reviewed hosts independently and concurrently.
async fn deploy_hosts(
    reviewed: Vec<ReviewedHost>,
    options: HostOptions,
    processes: &ProcessContext<'_>,
) -> Result<(), Error> {
    let display = ProgressDisplay::new()?;
    let progresses = display.hosts(reviewed.iter().map(ReviewedHost::id));
    let hosts = reviewed
        .iter()
        .map(|host| host.id().to_owned())
        .collect::<Vec<_>>();
    let outcomes = join_all(
        reviewed
            .into_iter()
            .zip(&progresses)
            .map(|(host, progress)| host::deploy(host, options, processes, progress)),
    )
    .await;

    let mut summary = DeploymentSummary::default();
    for ((host, outcome), progress) in hosts.into_iter().zip(outcomes).zip(progresses) {
        outcome.finish_progress(&host, &progress);
        summary.record(host, outcome);
    }
    println!("{summary}");
    summary.result()
}

#[cfg(test)]
mod tests {
    use super::{DeploymentSummary, HostOutcome};
    use crate::error::Error;

    /// Checks that successful restoration remains visible and fails the workflow.
    #[test]
    fn rolled_back_hosts_are_reported_as_failures() {
        let mut summary = DeploymentSummary::default();
        summary.record(
            "www".to_owned(),
            HostOutcome::RolledBack(Error::ConfirmationRequired),
        );

        assert_eq!(
            summary.to_string(),
            "deployment: 0 activated, 0 failed, 1 rolled back\nrolled back: www"
        );
        assert!(summary.result().is_err());
    }
}
