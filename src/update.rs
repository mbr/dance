//! Flake input updates during deployment review.

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

use futures::future::join_all;
use indicatif::ProgressBar;

use crate::{error::Error, logs::RunLog, process::run_status};

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

    /// Takes the first update error.
    pub(crate) fn take_error(&mut self) -> Option<Error> {
        self.first_error.take()
    }
}

/// Updates one flake and advances every host that depends on it.
async fn flake(
    path: PathBuf,
    host_count: usize,
    logs: &RunLog,
    progress: &ProgressBar,
) -> (PathBuf, Result<(), Error>) {
    let label = format!("update-{}", path.display());
    let result = run_status(
        logs,
        &label,
        "nix",
        &[
            "flake".into(),
            "update".into(),
            "--flake".into(),
            path.as_os_str().to_owned(),
        ],
        progress,
    )
    .await;
    let status = if result.is_ok() { "updated" } else { "failed" };
    progress.set_message(format!("{}: inputs {status}", path.display()));
    progress.inc(u64::try_from(host_count).expect("host count should fit in u64"));
    (path, result)
}

/// Updates distinct flakes concurrently and collects their failures.
pub(crate) async fn flakes(
    flakes: Vec<(PathBuf, usize)>,
    logs: &RunLog,
    progress: &ProgressBar,
) -> UpdateOutcome {
    let results = join_all(
        flakes
            .into_iter()
            .map(|(path, host_count)| flake(path, host_count, logs, progress)),
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
