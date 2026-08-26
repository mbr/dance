//! Lease-protected deployment state machine for one reviewed host.

use indicatif::ProgressBar;

use super::{ActivationOptions, operations, review::ReviewedHost, rollback};
use crate::{
    error::Error,
    lock::DeploymentLease,
    process::ProcessContext,
    status::{self, DeploymentPhase},
    verification,
};

/// Owns a reviewed host and its acquired deployment lease.
pub(super) struct LockedHost {
    /// Contains the reviewed host state.
    reviewed: ReviewedHost,
    /// Guards the host against overlapping deployment.
    lease: DeploymentLease,
}

/// Owns a locked host whose active system was revalidated.
pub(super) struct ValidatedHost {
    /// Contains the reviewed host state.
    reviewed: ReviewedHost,
    /// Guards the host throughout activation and recovery.
    lease: DeploymentLease,
}

/// Describes the result of one host deployment.
pub(super) enum Outcome {
    /// The host was activated and cleaned successfully.
    Activated,
    /// The deployment failed without requiring operator recovery.
    Failed(Error),
    /// The host restored its reviewed system after a deployment failure.
    RolledBack(Error),
    /// Live restoration failed and the host requires operator recovery.
    RecoveryRequired(Error),
    /// The host was activated but could not be cleaned.
    CleanupFailed(Error),
    /// The host was activated but its deployment lease could not be released cleanly.
    LeaseReleaseFailed(Error),
}

impl Outcome {
    /// Finalizes progress output for a completed host operation.
    pub(super) fn finish_progress(&self, host: &str, progress: &ProgressBar) {
        match self {
            Self::Activated => status::finish(progress, host),
            Self::RecoveryRequired(_) => status::recovery_required(progress),
            Self::Failed(_)
            | Self::RolledBack(_)
            | Self::CleanupFailed(_)
            | Self::LeaseReleaseFailed(_) => {
                status::fail(progress);
            }
        }
    }
}

impl LockedHost {
    /// Acquires the reviewed host's deployment lease.
    pub(super) async fn acquire(
        reviewed: ReviewedHost,
        progress: &ProgressBar,
    ) -> Result<Self, Error> {
        status::set_phase(progress, reviewed.id(), DeploymentPhase::Locking);
        let lease = DeploymentLease::acquire(reviewed.id(), &reviewed.host.ssh_url())
            .await
            .map_err(Error::Lock)?;
        Ok(Self { reviewed, lease })
    }

    /// Revalidates the active system while monitoring lease loss.
    pub(super) async fn revalidate(
        mut self,
        processes: &ProcessContext<'_>,
        progress: &ProgressBar,
    ) -> Result<ValidatedHost, Error> {
        let host_processes = processes.child();
        let active = {
            let validation =
                operations::deployed_store_path(&self.reviewed.host, &host_processes, progress);
            tokio::pin!(validation);
            tokio::select! {
                biased;
                error = self.lease.lost() => {
                    host_processes.cancel();
                    let _ = validation.await;
                    return Err(Error::Lock(error));
                }
                active = &mut validation => active,
            }
        };
        let active = match active {
            Ok(active) => active,
            Err(error) => {
                self.abandon().await;
                return Err(error);
            }
        };
        if active != self.reviewed.deployed {
            let error = Error::DeploymentStateChanged {
                host: self.reviewed.id().to_owned(),
                reviewed: self.reviewed.deployed.clone(),
                active,
            };
            self.abandon().await;
            return Err(error);
        }
        Ok(ValidatedHost {
            reviewed: self.reviewed,
            lease: self.lease,
        })
    }

    /// Releases a lease after fleet-level work is abandoned.
    pub(super) async fn abandon(self) {
        release_abandoned(&self.reviewed, self.lease).await;
    }
}

impl ValidatedHost {
    /// Runs activation and recovery while owning the host lease.
    pub(super) async fn activate(
        self,
        options: ActivationOptions,
        processes: &ProcessContext<'_>,
        progress: &ProgressBar,
    ) -> Outcome {
        let Self {
            reviewed,
            mut lease,
        } = self;
        let host_processes = processes.child();
        let deployment = run_activation(&reviewed, options, &host_processes, progress);
        tokio::pin!(deployment);
        let outcome = tokio::select! {
            biased;
            error = lease.lost() => {
                host_processes.cancel();
                let _ = deployment.await;
                Outcome::RecoveryRequired(Error::Lock(error))
            }
            outcome = &mut deployment => {
                match lease.release().await {
                    Ok(()) => outcome,
                    Err(error) if matches!(outcome, Outcome::Activated) => {
                        Outcome::LeaseReleaseFailed(Error::Lock(error))
                    }
                    Err(error) => {
                        tracing::warn!(
                            host = reviewed.id(),
                            error = %error,
                            "failed to release deployment lease",
                        );
                        outcome
                    }
                }
            }
        };
        outcome.finish_progress(reviewed.id(), progress);
        outcome
    }

    /// Releases a validated lease when another host prevents activation.
    pub(super) async fn abandon(self) {
        release_abandoned(&self.reviewed, self.lease).await;
    }
}

/// Releases an abandoned lease without replacing the primary fleet error.
async fn release_abandoned(reviewed: &ReviewedHost, lease: DeploymentLease) {
    if let Err(error) = lease.release().await {
        tracing::warn!(
            host = reviewed.id(),
            error = %error,
            "failed to release abandoned deployment lease",
        );
    }
}

/// Handles one failed deployment step according to the rollback policy.
async fn fail(
    reviewed: &ReviewedHost,
    options: ActivationOptions,
    persistence: rollback::Persistence,
    trigger: Error,
    processes: &ProcessContext<'_>,
    progress: &ProgressBar,
) -> Outcome {
    if !options.rollback {
        return Outcome::Failed(trigger);
    }

    status::set_phase(progress, reviewed.host.id(), DeploymentPhase::RollingBack);
    let recovery_processes = processes.recovery();
    match rollback::restore(
        &reviewed.host,
        &reviewed.deployed,
        persistence,
        &recovery_processes,
        progress,
    )
    .await
    {
        Ok(()) => Outcome::RolledBack(trigger),
        Err(rollback) => Outcome::RecoveryRequired(Error::RollbackIncomplete {
            host: reviewed.host.id().to_owned(),
            trigger: Box::new(trigger),
            rollback: Box::new(rollback),
        }),
    }
}

/// Temporarily activates, verifies, persists, and cleans one host configuration.
async fn run_activation(
    reviewed: &ReviewedHost,
    options: ActivationOptions,
    processes: &ProcessContext<'_>,
    progress: &ProgressBar,
) -> Outcome {
    status::set_phase(progress, reviewed.host.id(), DeploymentPhase::Testing);
    if let Err(error) = operations::test(&reviewed.host, processes, progress).await {
        return fail(
            reviewed,
            options,
            rollback::Persistence::Runtime,
            error,
            processes,
            progress,
        )
        .await;
    }

    if options.include_verifier
        && let Err(error) = verification::run_configured(&reviewed.host, processes, progress).await
    {
        return fail(
            reviewed,
            options,
            rollback::Persistence::Runtime,
            error,
            processes,
            progress,
        )
        .await;
    }

    status::set_phase(progress, reviewed.host.id(), DeploymentPhase::Switching);
    if let Err(error) = operations::switch(&reviewed.host, processes, progress).await {
        return fail(
            reviewed,
            options,
            rollback::Persistence::System,
            error,
            processes,
            progress,
        )
        .await;
    }

    if let Some(keep_generations) = options.keep_generations {
        status::set_phase(
            progress,
            reviewed.host.id(),
            DeploymentPhase::CollectingGarbage,
        );
        if let Err(error) =
            operations::collect_garbage(&reviewed.host, keep_generations, processes, progress).await
        {
            return Outcome::CleanupFailed(error);
        }
    }

    Outcome::Activated
}
