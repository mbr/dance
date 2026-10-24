//! Flake input updates during deployment review.

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

use futures::future::join_all;
use indicatif::ProgressBar;

use crate::{
    error::Error,
    git::{FlakeRepository, PendingLock, PendingUpdates},
    process::{Invocation, ProcessContext},
};

/// Contains the failures from a group of flake updates.
#[derive(Default)]
pub(crate) struct UpdateOutcome {
    /// Identifies flakes whose inputs could not be updated.
    failed: HashSet<PathBuf>,
    /// Retains the first update error for reporting.
    first_error: Option<Error>,
}

impl UpdateOutcome {
    /// Returns whether a flake update failed.
    pub(crate) fn failed(&self, flake: &Path) -> bool {
        self.failed.contains(flake)
    }

    /// Returns the first update error for reporting.
    pub(crate) fn into_error(self) -> Option<Error> {
        self.first_error
    }
}

/// Updates one flake and reports the result on each dependent host.
async fn flake(
    path: PathBuf,
    progresses: Vec<ProgressBar>,
    repository: Option<FlakeRepository>,
    processes: &ProcessContext<'_>,
) -> (PathBuf, Result<Option<PendingLock>, Error>) {
    let progress = progresses
        .first()
        .expect("each selected flake should have a host progress bar");
    let result = Invocation::new(
        processes,
        format!("update-{}", path.display()),
        "nix",
        progress,
    )
    .arguments(["flake", "update", "--flake"])
    .argument(path.as_os_str())
    .status()
    .await;
    let result = match (result, repository) {
        (Ok(()), Some(repository)) => repository.updated(processes, progress).await,
        (Ok(()), None) => Ok(None),
        (Err(error), _) => Err(error),
    };
    let status = if result.is_ok() { "updated" } else { "failed" };
    for progress in progresses {
        progress.set_message(format!("inputs {status}"));
    }
    (path, result)
}

/// Updates distinct flakes and optionally commits before configuration evaluation.
pub(crate) async fn flakes(
    flakes: Vec<(PathBuf, Vec<ProgressBar>)>,
    commit: bool,
    processes: &ProcessContext<'_>,
) -> Result<UpdateOutcome, Error> {
    let mut outcome = UpdateOutcome::default();
    let mut pending = PendingUpdates::default();
    let updates = if commit {
        let prepared = join_all(flakes.into_iter().map(|(path, progresses)| async move {
            let progress = progresses
                .first()
                .expect("each selected flake should have a host progress bar");
            let repository = FlakeRepository::prepare(&path, processes, progress).await;
            (path, progresses, repository)
        }))
        .await;
        prepared
            .into_iter()
            .filter_map(|(path, progresses, repository)| match repository {
                Ok(repository) => Some((path, progresses, Some(repository))),
                Err(error) => {
                    for progress in progresses {
                        progress.set_message("update preflight failed");
                    }
                    outcome.failed.insert(path);
                    outcome.first_error.get_or_insert(error);
                    None
                }
            })
            .collect::<Vec<_>>()
    } else {
        flakes
            .into_iter()
            .map(|(path, progresses)| (path, progresses, None))
            .collect()
    };
    let results = join_all(
        updates
            .into_iter()
            .map(|(path, progresses, repository)| flake(path, progresses, repository, processes)),
    )
    .await;

    for (path, result) in results {
        match result {
            Ok(lock) => pending.push(lock),
            Err(error) => {
                outcome.failed.insert(path);
                outcome.first_error.get_or_insert(error);
            }
        }
    }

    if commit {
        if let Some(error) = outcome.first_error {
            return Err(error);
        }
        pending.commit(processes).await?;
    }

    Ok(outcome)
}
