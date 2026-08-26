//! Lock-protected deployment workflow for one reviewed host.

use indicatif::ProgressBar;

use super::{ActivationOptions, operations, review::ReviewedHost, rollback};
use crate::{
    error::Error,
    process::ProcessContext,
    ssh::lock::DeploymentLock,
    status::{self, DeploymentPhase},
    verification::PreparedVerifier,
};

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

/// Locks, validates, prepares, and deploys one reviewed host.
pub(super) async fn deploy(
    reviewed: ReviewedHost,
    options: ActivationOptions,
    processes: &ProcessContext<'_>,
    progress: &ProgressBar,
) -> Outcome {
    status::set_phase(progress, reviewed.id(), DeploymentPhase::Locking);
    let _lock = match DeploymentLock::acquire(reviewed.id(), &reviewed.host.ssh_url()).await {
        Ok(lock) => lock,
        Err(error) => return Outcome::Failed(Error::Lock(error)),
    };

    let active = match operations::deployed_store_path(&reviewed.host, processes, progress).await {
        Ok(active) => active,
        Err(error) => return Outcome::Failed(error),
    };
    if active != reviewed.deployed {
        return Outcome::Failed(Error::DeploymentStateChanged {
            host: reviewed.id().to_owned(),
            reviewed: reviewed.deployed.clone(),
            active,
        });
    }

    let verifier = if options.include_verifier {
        status::set_phase(progress, reviewed.id(), DeploymentPhase::BuildingVerifier);
        match PreparedVerifier::prepare(&reviewed.host, processes, progress).await {
            Ok(verifier) => verifier,
            Err(error) => return Outcome::Failed(error),
        }
    } else {
        None
    };

    status::set_phase(progress, reviewed.id(), DeploymentPhase::Testing);
    if let Err(error) = operations::test(&reviewed.host, processes, progress).await {
        return fail(
            &reviewed,
            options,
            rollback::Persistence::Runtime,
            error,
            processes,
            progress,
        )
        .await;
    }

    if let Some(verifier) = &verifier {
        status::set_phase(progress, reviewed.id(), DeploymentPhase::RunningVerifier);
        if let Err(error) = verifier.run(&reviewed.host, processes, progress).await {
            return fail(
                &reviewed,
                options,
                rollback::Persistence::Runtime,
                error,
                processes,
                progress,
            )
            .await;
        }
    }

    status::set_phase(progress, reviewed.id(), DeploymentPhase::Switching);
    if let Err(error) = operations::switch(&reviewed.host, processes, progress).await {
        return fail(
            &reviewed,
            options,
            rollback::Persistence::System,
            error,
            processes,
            progress,
        )
        .await;
    }

    if let Some(keep_generations) = options.keep_generations {
        status::set_phase(progress, reviewed.id(), DeploymentPhase::CollectingGarbage);
        if let Err(error) =
            operations::collect_garbage(&reviewed.host, keep_generations, processes, progress).await
        {
            return Outcome::CleanupFailed(error);
        }
    }

    Outcome::Activated
}
