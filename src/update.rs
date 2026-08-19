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

/// Updates one flake and advances every host that depends on it.
async fn flake(
    path: PathBuf,
    host_count: usize,
    repository: Option<FlakeRepository>,
    processes: &ProcessContext<'_>,
    progress: &ProgressBar,
) -> (PathBuf, Result<Option<PendingLock>, Error>) {
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
    progress.set_message(format!("{}: inputs {status}", path.display()));
    progress.inc(u64::try_from(host_count).expect("host count should fit in u64"));
    (path, result)
}

/// Updates distinct flakes concurrently and collects their failures.
pub(crate) async fn flakes(
    flakes: Vec<(PathBuf, usize)>,
    commit: bool,
    processes: &ProcessContext<'_>,
    progress: &ProgressBar,
) -> UpdateOutcome {
    let repositories = if commit {
        let prepared = join_all(
            flakes
                .iter()
                .map(|(path, _)| FlakeRepository::prepare(path, processes, progress)),
        )
        .await;
        let mut repositories = Vec::with_capacity(prepared.len());
        let mut first_error = None;
        for repository in prepared {
            match repository {
                Ok(repository) => repositories.push(repository),
                Err(error) => {
                    first_error.get_or_insert(error);
                }
            }
        }
        if let Some(error) = first_error {
            let mut outcome = UpdateOutcome {
                first_error: Some(error),
                ..UpdateOutcome::default()
            };
            for (path, host_count) in flakes {
                progress.set_message(format!("{}: update preflight failed", path.display()));
                progress.inc(u64::try_from(host_count).expect("host count should fit in u64"));
                outcome.failed.insert(path);
            }
            return outcome;
        }
        repositories.into_iter().map(Some).collect::<Vec<_>>()
    } else {
        (0..flakes.len()).map(|_| None).collect()
    };
    let results = join_all(flakes.into_iter().zip(repositories).map(
        |((path, host_count), repository)| flake(path, host_count, repository, processes, progress),
    ))
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
