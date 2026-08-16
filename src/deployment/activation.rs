//! Per-host testing, verification, activation, rollback, and cleanup.

use std::num::NonZeroU32;

use indicatif::ProgressBar;

use super::{operations, review::ReviewedHost, rollback};
use crate::{
    error::Error,
    logs::RunLog,
    status::{self, DeploymentPhase},
    verification,
};

/// Configures verifier execution, rollback, and cleanup for one host.
#[derive(Clone, Copy)]
pub(super) struct Options {
    /// Selects whether a configured verifier is included.
    pub(super) include_verifier: bool,
    /// Selects whether failed tests or verification trigger live restoration.
    pub(super) rollback: bool,
    /// Selects cleanup and generation retention after activation.
    pub(super) keep_generations: Option<NonZeroU32>,
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

/// Handles one failed deployment step according to the rollback policy.
async fn fail(
    reviewed: &ReviewedHost,
    options: Options,
    persistence: rollback::Persistence,
    trigger: Error,
    logs: &RunLog,
    progress: &ProgressBar,
) -> Outcome {
    if !options.rollback {
        return Outcome::Failed(trigger);
    }

    status::set_phase(progress, reviewed.host.id(), DeploymentPhase::RollingBack);
    match rollback::restore(
        &reviewed.host,
        &reviewed.deployed,
        persistence,
        logs,
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

/// Temporarily activates, verifies, and persists one host configuration.
pub(super) async fn activate(
    reviewed: ReviewedHost,
    options: Options,
    logs: &RunLog,
    progress: &ProgressBar,
) -> Outcome {
    status::set_phase(progress, reviewed.host.id(), DeploymentPhase::Testing);
    if let Err(error) = operations::test(&reviewed.host, logs, progress).await {
        return fail(
            &reviewed,
            options,
            rollback::Persistence::Runtime,
            error,
            logs,
            progress,
        )
        .await;
    }

    if options.include_verifier
        && let Err(error) = verification::run_configured(&reviewed.host, logs, progress).await
    {
        return fail(
            &reviewed,
            options,
            rollback::Persistence::Runtime,
            error,
            logs,
            progress,
        )
        .await;
    }

    status::set_phase(progress, reviewed.host.id(), DeploymentPhase::Switching);
    if let Err(error) = operations::switch(&reviewed.host, logs, progress).await {
        return fail(
            &reviewed,
            options,
            rollback::Persistence::System,
            error,
            logs,
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
            operations::collect_garbage(&reviewed.host, keep_generations, logs, progress).await
        {
            return Outcome::CleanupFailed(error);
        }
    }

    status::finish(progress, reviewed.host.id());
    Outcome::Activated
}
