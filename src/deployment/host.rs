//! Lock-protected deployment state machine for one reviewed host.

use indicatif::ProgressBar;

use super::{ActivationOptions, operations, review::ReviewedHost, rollback};
use crate::{
    error::Error,
    process::ProcessContext,
    ssh::lock::DeploymentLock,
    status::{self, DeploymentPhase},
    verification::PreparedVerifier,
};

/// Owns a reviewed host and its optional prebuilt verifier.
pub(super) struct PreparedHost {
    /// Contains the reviewed host state.
    reviewed: ReviewedHost,
    /// Contains the verifier built before any host activation.
    verifier: Option<PreparedVerifier>,
}

/// Owns a prepared host and its acquired deployment lock.
pub(super) struct LockedHost {
    /// Contains the reviewed host state.
    reviewed: ReviewedHost,
    /// Contains the verifier built before any host activation.
    verifier: Option<PreparedVerifier>,
    /// Guards the host against overlapping deployment.
    _lock: DeploymentLock,
}

/// Owns a locked host whose active system was revalidated.
pub(super) struct ValidatedHost {
    /// Contains the reviewed host state.
    reviewed: ReviewedHost,
    /// Contains the verifier built before any host activation.
    verifier: Option<PreparedVerifier>,
    /// Guards the host throughout activation and recovery.
    _lock: DeploymentLock,
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
}

impl Outcome {
    /// Finalizes progress output for a completed host operation.
    pub(super) fn finish_progress(&self, host: &str, progress: &ProgressBar) {
        match self {
            Self::Activated => status::finish(progress, host),
            Self::Failed(_) => status::deployment_failed(progress, host),
            Self::RolledBack(_) => status::rolled_back(progress, host),
            Self::RecoveryRequired(_) => status::recovery_required(progress, host),
            Self::CleanupFailed(_) => status::cleanup_failed(progress, host),
        }
    }
}

impl PreparedHost {
    /// Builds the reviewed host's verifier without modifying the host.
    pub(super) async fn prepare(
        reviewed: ReviewedHost,
        include_verifier: bool,
        processes: &ProcessContext<'_>,
        progress: &ProgressBar,
    ) -> Result<Self, Error> {
        let verifier = if include_verifier {
            status::set_phase(progress, reviewed.id(), DeploymentPhase::BuildingVerifier);
            PreparedVerifier::prepare(&reviewed.host, processes, progress).await?
        } else {
            None
        };
        Ok(Self { reviewed, verifier })
    }

    /// Returns the inventory identity of the prepared host.
    pub(super) fn id(&self) -> &str {
        self.reviewed.id()
    }
}

impl LockedHost {
    /// Acquires the prepared host's deployment lock.
    pub(super) async fn acquire(
        prepared: PreparedHost,
        progress: &ProgressBar,
    ) -> Result<Self, Error> {
        status::set_phase(progress, prepared.id(), DeploymentPhase::Locking);
        let lock = DeploymentLock::acquire(prepared.id(), &prepared.reviewed.host.ssh_url())
            .await
            .map_err(Error::Lock)?;
        Ok(Self {
            reviewed: prepared.reviewed,
            verifier: prepared.verifier,
            _lock: lock,
        })
    }

    /// Revalidates the active system while holding the deployment lock.
    pub(super) async fn revalidate(
        self,
        processes: &ProcessContext<'_>,
        progress: &ProgressBar,
    ) -> Result<ValidatedHost, Error> {
        let active =
            operations::deployed_store_path(&self.reviewed.host, processes, progress).await;
        let active = active?;
        if active != self.reviewed.deployed {
            let error = Error::DeploymentStateChanged {
                host: self.reviewed.id().to_owned(),
                reviewed: self.reviewed.deployed.clone(),
                active,
            };
            return Err(error);
        }
        Ok(ValidatedHost {
            reviewed: self.reviewed,
            verifier: self.verifier,
            _lock: self._lock,
        })
    }
}

impl ValidatedHost {
    /// Returns the inventory identity of the validated host.
    pub(super) fn id(&self) -> &str {
        self.reviewed.id()
    }

    /// Runs activation and recovery while owning the host lock.
    pub(super) async fn activate(
        self,
        options: ActivationOptions,
        processes: &ProcessContext<'_>,
        progress: &ProgressBar,
    ) -> Outcome {
        let outcome = run_activation(
            &self.reviewed,
            self.verifier.as_ref(),
            options,
            processes,
            progress,
        )
        .await;
        outcome.finish_progress(self.reviewed.id(), progress);
        outcome
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
    verifier: Option<&PreparedVerifier>,
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

    if let Some(verifier) = verifier {
        status::set_phase(
            progress,
            reviewed.host.id(),
            DeploymentPhase::RunningVerifier,
        );
        if let Err(error) = verifier.run(&reviewed.host, processes, progress).await {
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
