//! Flake input updates during deployment review.

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

use futures::future::join_all;
use indicatif::ProgressBar;

use crate::{
    error::Error,
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

    /// Returns the first update error.
    pub(crate) fn into_error(self) -> Option<Error> {
        self.first_error
    }
}

/// Updates one flake and advances every host that depends on it.
async fn flake(
    path: PathBuf,
    host_count: usize,
    processes: &ProcessContext<'_>,
    progress: &ProgressBar,
) -> (PathBuf, Result<(), Error>) {
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
    let status = if result.is_ok() { "updated" } else { "failed" };
    progress.set_message(format!("{}: inputs {status}", path.display()));
    progress.inc(u64::try_from(host_count).expect("host count should fit in u64"));
    (path, result)
}

/// Updates distinct flakes concurrently and collects their failures.
pub(crate) async fn flakes(
    flakes: Vec<(PathBuf, usize)>,
    processes: &ProcessContext<'_>,
    progress: &ProgressBar,
) -> UpdateOutcome {
    let results = join_all(
        flakes
            .into_iter()
            .map(|(path, host_count)| flake(path, host_count, processes, progress)),
    )
    .await;
    let mut outcome = UpdateOutcome::default();

    for (path, result) in results {
        if let Err(error) = result {
            outcome.failed.insert(path);
            outcome.first_error.get_or_insert(error);
        }
    }

    outcome
}
