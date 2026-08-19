//! Git preparation and commits for flake input updates.

use std::path::{Path, PathBuf};

use indexmap::IndexMap;
use indicatif::ProgressBar;

use crate::{
    error::Error,
    process::{Invocation, ProcessContext},
};

/// Describes a clean tracked flake before its inputs are updated.
pub(crate) struct FlakeRepository {
    /// Identifies the repository root.
    repository: PathBuf,
    /// Identifies the repository revision used for deployment.
    head: String,
    /// Identifies the lock file relative to the repository.
    lock: PathBuf,
}

impl FlakeRepository {
    /// Checks that a flake can produce an isolated lock-file commit.
    pub(crate) async fn prepare(
        flake: &Path,
        processes: &ProcessContext<'_>,
        progress: &ProgressBar,
    ) -> Result<Self, Error> {
        let repository = git(processes, progress, flake, "git-repository")
            .arguments(["rev-parse", "--show-toplevel"])
            .output()
            .await?;
        let repository = PathBuf::from(repository.trim());
        let prefix = git(processes, progress, flake, "git-prefix")
            .arguments(["rev-parse", "--show-prefix"])
            .output()
            .await?;
        let prefix = PathBuf::from(prefix.trim());
        let lock = prefix.join("flake.lock");
        let definition = prefix.join("flake.nix");
        let tracked = git(processes, progress, &repository, "git-lock-tracked")
            .arguments(["ls-files", "--stage", "--"])
            .argument(lock.as_os_str())
            .output()
            .await?;
        if tracked.trim().is_empty() {
            return Err(Error::UntrackedFlakeLock {
                path: repository.join(lock),
            });
        }
        let status = git(processes, progress, &repository, "git-flake-status")
            .arguments(["status", "--porcelain=v1", "--untracked-files=all", "--"])
            .argument(definition.as_os_str())
            .argument(lock.as_os_str())
            .output()
            .await?;
        if !status.trim().is_empty() {
            return Err(Error::DirtyFlake {
                path: flake.to_path_buf(),
                status: status.trim().to_owned(),
            });
        }
        let head = git(processes, progress, &repository, "git-head")
            .arguments(["rev-parse", "HEAD"])
            .output()
            .await?;

        Ok(Self {
            repository,
            head: head.trim().to_owned(),
            lock,
        })
    }

    /// Records a lock file changed by a successful update.
    pub(crate) async fn updated(
        self,
        processes: &ProcessContext<'_>,
        progress: &ProgressBar,
    ) -> Result<Option<PendingLock>, Error> {
        let status = lock_status(processes, progress, &self.repository, &self.lock).await?;
        if status.trim().is_empty() {
            return Ok(None);
        }
        let hash = lock_hash(processes, progress, &self.repository, &self.lock).await?;
        Ok(Some(PendingLock {
            repository: self.repository,
            head: self.head,
            lock: self.lock,
            hash,
            status,
        }))
    }
}

/// Describes one updated lock awaiting a successful deployment.
pub(crate) struct PendingLock {
    /// Identifies the repository root.
    repository: PathBuf,
    /// Identifies the repository revision used for deployment.
    head: String,
    /// Identifies the lock file relative to the repository.
    lock: PathBuf,
    /// Identifies the updated lock contents.
    hash: String,
    /// Captures the updated lock's index and worktree state.
    status: String,
}

/// Collects updated lock files awaiting successful deployment.
#[derive(Default)]
pub(crate) struct PendingUpdates {
    /// Contains pending lock files in update order.
    locks: Vec<PendingLock>,
}

impl PendingUpdates {
    /// Adds one updated lock file when an update changed it.
    pub(crate) fn push(&mut self, lock: Option<PendingLock>) {
        self.locks.extend(lock);
    }

    /// Validates and commits all updated locks by repository.
    pub(crate) async fn commit(self, processes: &ProcessContext<'_>) -> Result<(), Error> {
        let progress = ProgressBar::hidden();
        let mut repositories = IndexMap::<PathBuf, RepositoryUpdate>::new();
        for lock in self.locks {
            let update = repositories
                .entry(lock.repository.clone())
                .or_insert_with(|| RepositoryUpdate {
                    head: lock.head.clone(),
                    locks: Vec::new(),
                });
            if update.head != lock.head {
                return Err(Error::RepositoryChanged {
                    path: lock.repository,
                });
            }
            update.locks.push(lock);
        }

        for (repository, update) in &repositories {
            update.validate(repository, processes, &progress).await?;
        }
        for (repository, update) in repositories {
            update.commit(&repository, processes, &progress).await?;
        }
        Ok(())
    }
}

/// Groups lock updates belonging to one repository commit.
struct RepositoryUpdate {
    /// Identifies the repository revision used for deployment.
    head: String,
    /// Contains updated locks within the repository.
    locks: Vec<PendingLock>,
}

impl RepositoryUpdate {
    /// Verifies that the repository and updated locks have not changed.
    async fn validate(
        &self,
        repository: &Path,
        processes: &ProcessContext<'_>,
        progress: &ProgressBar,
    ) -> Result<(), Error> {
        let head = git(processes, progress, repository, "git-commit-head")
            .arguments(["rev-parse", "HEAD"])
            .output()
            .await?;
        if head.trim() != self.head {
            return Err(Error::RepositoryChanged {
                path: repository.to_path_buf(),
            });
        }
        for lock in &self.locks {
            let status = lock_status(processes, progress, repository, &lock.lock).await?;
            let hash = lock_hash(processes, progress, repository, &lock.lock).await?;
            if status != lock.status || hash != lock.hash {
                return Err(Error::UpdatedFlakeLockChanged {
                    path: repository.join(&lock.lock),
                });
            }
        }
        Ok(())
    }

    /// Commits only the updated lock files in one repository.
    async fn commit(
        self,
        repository: &Path,
        processes: &ProcessContext<'_>,
        progress: &ProgressBar,
    ) -> Result<(), Error> {
        let mut invocation = git(processes, progress, repository, "git-commit-flake-updates")
            .arguments(["commit", "--only", "-m", "Update flake inputs", "--"]);
        for lock in self.locks {
            invocation = invocation.argument(lock.lock.into_os_string());
        }
        invocation.status().await
    }
}

/// Reads one lock file's index and worktree state.
async fn lock_status(
    processes: &ProcessContext<'_>,
    progress: &ProgressBar,
    repository: &Path,
    lock: &Path,
) -> Result<String, Error> {
    git(processes, progress, repository, "git-updated-lock-status")
        .arguments(["status", "--porcelain=v1", "--untracked-files=all", "--"])
        .argument(lock.as_os_str())
        .output()
        .await
}

/// Calculates the Git object identity of a worktree lock file.
async fn lock_hash(
    processes: &ProcessContext<'_>,
    progress: &ProgressBar,
    repository: &Path,
    lock: &Path,
) -> Result<String, Error> {
    let hash = git(processes, progress, repository, "git-updated-lock-hash")
        .arguments(["hash-object", "--"])
        .argument(lock.as_os_str())
        .output()
        .await?;
    Ok(hash.trim().to_owned())
}

/// Creates a Git invocation rooted at the selected path.
fn git<'a>(
    processes: &'a ProcessContext<'a>,
    progress: &'a ProgressBar,
    directory: &Path,
    label: &str,
) -> Invocation<'a> {
    Invocation::new(processes, label, "git", progress)
        .argument("-C")
        .argument(directory.as_os_str())
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path, process::Command};

    use indicatif::ProgressBar;
    use tempfile::tempdir;
    use tokio_util::sync::CancellationToken;

    use super::{FlakeRepository, PendingUpdates};
    use crate::{
        logs::{LogMode, RunLog},
        process::ProcessContext,
    };

    /// Runs a successful Git command in a test repository.
    fn run(repository: &Path, arguments: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(repository)
            .args(arguments)
            .output()
            .expect("git should run");
        assert!(output.status.success());
        String::from_utf8(output.stdout).expect("git output should be UTF-8")
    }

    /// Checks isolated lock commits and preservation of unrelated staged changes.
    #[tokio::test]
    async fn commits_only_the_updated_lock_file() {
        let repository = tempdir().expect("temporary repository should be created");
        run(repository.path(), &["init", "--quiet"]);
        run(repository.path(), &["config", "user.name", "Dance Test"]);
        run(
            repository.path(),
            &["config", "user.email", "dance@example.invalid"],
        );
        fs::write(repository.path().join("flake.nix"), "{}\n")
            .expect("flake definition should be written");
        fs::write(repository.path().join("flake.lock"), "old\n")
            .expect("flake lock should be written");
        fs::write(repository.path().join("other"), "old\n")
            .expect("unrelated file should be written");
        run(
            repository.path(),
            &["add", "flake.nix", "flake.lock", "other"],
        );
        run(repository.path(), &["commit", "--quiet", "-m", "Initial"]);

        let logs = RunLog::start(LogMode::Never, None, Path::new("inventory.toml"))
            .expect("disabled logs should start");
        let processes = ProcessContext::new(&logs, CancellationToken::new());
        let progress = ProgressBar::hidden();
        let prepared = FlakeRepository::prepare(repository.path(), &processes, &progress)
            .await
            .expect("clean tracked flake should be prepared");
        fs::write(repository.path().join("flake.lock"), "new\n")
            .expect("flake lock should be updated");
        let lock = prepared
            .updated(&processes, &progress)
            .await
            .expect("updated lock should be recorded");
        fs::write(repository.path().join("other"), "staged\n")
            .expect("unrelated file should be changed");
        run(repository.path(), &["add", "other"]);
        let mut updates = PendingUpdates::default();
        updates.push(lock);
        updates
            .commit(&processes)
            .await
            .expect("updated lock should be committed");

        assert_eq!(
            run(repository.path(), &["show", "HEAD:flake.lock"]),
            "new\n"
        );
        assert_eq!(run(repository.path(), &["show", "HEAD:other"]), "old\n");
        assert_eq!(run(repository.path(), &["show", ":other"]), "staged\n");
        drop(processes);
        logs.finish(None).expect("disabled logs should finish");
    }
}
