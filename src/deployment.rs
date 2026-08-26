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
use futures::future::{join_all, select_all};
use tabled::Table;

use self::{
    activation::Summary as ActivationSummary,
    review::{HostRow, ReviewOutcome, ReviewedHost},
};
use crate::{
    error::Error,
    git::PendingUpdates,
    inventory::Inventory,
    lock::DeploymentLease,
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

    activate(outdated, options.activation, processes).await?;
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

/// Acquires every host lease without beginning partial activation.
async fn acquire_leases(
    reviewed: &[ReviewedHost],
    progresses: &[indicatif::ProgressBar],
) -> Result<Vec<DeploymentLease>, Error> {
    let results = join_all(
        reviewed
            .iter()
            .zip(progresses)
            .map(|(host, progress)| async move {
                status::set_phase(progress, host.id(), status::DeploymentPhase::Locking);
                DeploymentLease::acquire(host.id(), &host.host.ssh_url()).await
            }),
    )
    .await;
    let mut leases = Vec::with_capacity(results.len());
    let mut first_error = None;
    for result in results {
        match result {
            Ok(lease) => leases.push(lease),
            Err(error) => {
                first_error.get_or_insert(Error::Lock(error));
            }
        }
    }
    if let Some(error) = first_error {
        release_leases(leases).await;
        for progress in progresses {
            status::fail(progress);
        }
        Err(error)
    } else {
        Ok(leases)
    }
}

/// Releases a group of leases after work is abandoned.
async fn release_leases(leases: Vec<DeploymentLease>) {
    for result in join_all(leases.into_iter().map(DeploymentLease::release)).await {
        if let Err(error) = result {
            tracing::warn!(error = %error, "failed to release abandoned deployment lease");
        }
    }
}

/// Verifies that reviewed host state remains current under the leases.
async fn revalidate(
    reviewed: &[ReviewedHost],
    progresses: &[indicatif::ProgressBar],
    leases: &mut [DeploymentLease],
    processes: &ProcessContext<'_>,
) -> Result<(), Error> {
    let active =
        join_all(reviewed.iter().zip(progresses).map(|(host, progress)| {
            operations::deployed_store_path(&host.host, processes, progress)
        }));
    let losses = leases
        .iter_mut()
        .map(|lease| Box::pin(lease.lost()))
        .collect::<Vec<_>>();
    let active = tokio::select! {
        active = active => active,
        (error, _, _) = select_all(losses) => return Err(Error::Lock(error)),
    };
    for (host, active) in reviewed.iter().zip(active) {
        let active = active?;
        if active != host.deployed {
            return Err(Error::DeploymentStateChanged {
                host: host.id().to_owned(),
                reviewed: host.deployed.clone(),
                active,
            });
        }
    }
    Ok(())
}

/// Activates one host while continuously monitoring and then releasing its lease.
async fn activate_with_lease(
    host: ReviewedHost,
    options: ActivationOptions,
    processes: &ProcessContext<'_>,
    progress: &indicatif::ProgressBar,
    mut lease: DeploymentLease,
) -> activation::Outcome {
    let host_processes = processes.child();
    let deployment = activation::activate(&host, options, &host_processes, progress);
    tokio::pin!(deployment);
    let outcome = tokio::select! {
        biased;
        error = lease.lost() => {
            host_processes.cancel();
            let _ = deployment.await;
            activation::Outcome::RecoveryRequired(Error::Lock(error))
        }
        outcome = &mut deployment => {
            match lease.release().await {
                Ok(()) => outcome,
                Err(error) if matches!(outcome, activation::Outcome::Activated) => {
                    activation::Outcome::LeaseReleaseFailed(Error::Lock(error))
                }
                Err(error) => {
                    tracing::warn!(
                        host = host.id(),
                        error = %error,
                        "failed to release deployment lease",
                    );
                    outcome
                }
            }
        }
    };
    outcome.finish_progress(host.id(), progress);
    outcome
}

/// Tests, verifies, and persistently activates all reviewed hosts concurrently.
async fn activate(
    reviewed: Vec<ReviewedHost>,
    options: ActivationOptions,
    processes: &ProcessContext<'_>,
) -> Result<(), Error> {
    let display = ProgressDisplay::new()?;
    let progresses = display.hosts(reviewed.iter().map(ReviewedHost::id));
    let mut leases = acquire_leases(&reviewed, &progresses).await?;
    if let Err(error) = revalidate(&reviewed, &progresses, &mut leases, processes).await {
        release_leases(leases).await;
        for progress in &progresses {
            status::fail(progress);
        }
        return Err(error);
    }

    let outcomes = join_all(reviewed.into_iter().zip(progresses.iter()).zip(leases).map(
        |((host, progress), lease)| activate_with_lease(host, options, processes, progress, lease),
    ))
    .await;

    let mut summary = ActivationSummary::default();
    for outcome in outcomes {
        summary.record(outcome);
    }
    println!("{summary}");
    summary.result()
}
