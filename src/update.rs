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
    /// Contains changed locks awaiting successful deployment.
    pending: PendingUpdates,
}

impl UpdateOutcome {
    /// Returns whether a flake update failed.
    pub(crate) fn failed(&self, flake: &Path) -> bool {
        self.failed.contains(flake)
    }

    /// Separates the first update error from pending lock commits.
    pub(crate) fn into_parts(self) -> (Option<Error>, PendingUpdates) {
        (self.first_error, self.pending)
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

/// Updates distinct flakes concurrently and collects their failures.
pub(crate) async fn flakes(
    flakes: Vec<(PathBuf, Vec<ProgressBar>)>,
    commit: bool,
    processes: &ProcessContext<'_>,
) -> UpdateOutcome {
    let updates = if commit {
        let prepared = join_all(flakes.into_iter().map(|(path, progresses)| async move {
            let progress = progresses
                .first()
                .expect("each selected flake should have a host progress bar");
            let repository = FlakeRepository::prepare(&path, processes, progress).await;
            (path, progresses, repository)
        }))
        .await;
        if prepared
            .iter()
            .any(|(_, _, repository)| repository.is_err())
        {
            let mut outcome = UpdateOutcome::default();
            for (path, progresses, repository) in prepared {
                if let Err(error) = repository {
                    outcome.first_error.get_or_insert(error);
                }
                for progress in progresses {
                    progress.set_message("update preflight failed");
                }
                outcome.failed.insert(path);
            }
            return outcome;
        }
        prepared
            .into_iter()
            .filter_map(|(path, progresses, repository)| {
                repository
                    .ok()
                    .map(|repository| (path, progresses, Some(repository)))
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
    let mut outcome = UpdateOutcome::default();

    for (path, result) in results {
        match result {
            Ok(lock) => outcome.pending.push(lock),
            Err(error) => {
                outcome.failed.insert(path);
                outcome.first_error.get_or_insert(error);
            }
        }
    }

    outcome
}
