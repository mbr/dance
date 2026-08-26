//! Remote deployment leases held through SSH.

use std::{
    io,
    process::{ExitStatus, Stdio},
    time::Duration,
};

use thiserror::Error;
use tokio::{
    io::AsyncReadExt,
    process::{Child, ChildStdin, Command},
    task::JoinHandle,
};

use crate::ssh;

/// Identifies successful lease acquisition on standard output.
const ACQUIRED: &[u8] = b"dance-lock-acquired\n";
/// Limits SSH connection and lease acquisition.
const ACQUISITION_TIMEOUT: Duration = Duration::from_mins(1);
/// Limits graceful lease release.
const RELEASE_TIMEOUT: Duration = Duration::from_secs(5);
/// Runs while holding the remote file lock.
const REMOTE_COMMAND: &str = concat!(
    "exec /run/current-system/sw/bin/flock --exclusive --nonblock ",
    "--conflict-exit-code 75 /run/lock/dance-deployment.lock ",
    "/run/current-system/sw/bin/sh -c '",
    "printf \"dance-lock-acquired\\n\"; ",
    "exec /run/current-system/sw/bin/cat >/dev/null'",
);

/// Describes failures while managing a remote deployment lease.
#[derive(Debug, Error)]
pub(crate) enum LockError {
    /// Indicates that another process holds the host lease.
    #[error("deployment lock on host `{host}` is already held")]
    Unavailable {
        /// Identifies the target host.
        host: String,
    },
    /// Indicates that the SSH lease process could not be started.
    #[error("failed to start deployment lock on host `{host}`")]
    Spawn {
        /// Identifies the target host.
        host: String,
        /// Contains the process creation error.
        #[source]
        source: io::Error,
    },
    /// Indicates that lease acquisition exceeded its deadline.
    #[error("timed out acquiring deployment lock on host `{host}`")]
    AcquisitionTimedOut {
        /// Identifies the target host.
        host: String,
    },
    /// Indicates that the lease process failed before acquisition.
    #[error("failed to acquire deployment lock on host `{host}` ({status}): {stderr}")]
    AcquisitionFailed {
        /// Identifies the target host.
        host: String,
        /// Contains the SSH process status.
        status: ExitStatus,
        /// Contains concise process diagnostics.
        stderr: String,
    },
    /// Indicates that the lease process did not acknowledge acquisition.
    #[error("deployment lock on host `{host}` returned an invalid acknowledgement")]
    InvalidAcknowledgement {
        /// Identifies the target host.
        host: String,
    },
    /// Indicates that lease process output could not be read.
    #[error("failed to read deployment lock process for host `{host}`")]
    Output {
        /// Identifies the target host.
        host: String,
        /// Contains the output error.
        #[source]
        source: io::Error,
    },
    /// Indicates that an acquired lease ended unexpectedly.
    #[error("deployment lock on host `{host}` was lost ({status}): {stderr}")]
    Lost {
        /// Identifies the target host.
        host: String,
        /// Contains the SSH process status.
        status: ExitStatus,
        /// Contains concise process diagnostics.
        stderr: String,
    },
    /// Indicates that a lease could not be released cleanly.
    #[error("failed to release deployment lock on host `{host}`")]
    Release {
        /// Identifies the target host.
        host: String,
        /// Contains the process lifecycle error.
        #[source]
        source: io::Error,
    },
    /// Indicates that lease release exceeded its deadline.
    #[error("timed out releasing deployment lock on host `{host}`")]
    ReleaseTimedOut {
        /// Identifies the target host.
        host: String,
    },
    /// Indicates that the lease process failed during release.
    #[error("deployment lock process for host `{host}` exited with {status}: {stderr}")]
    ReleaseFailed {
        /// Identifies the target host.
        host: String,
        /// Contains the SSH process status.
        status: ExitStatus,
        /// Contains concise process diagnostics.
        stderr: String,
    },
}

/// Holds an exclusive deployment lease on one host.
pub(crate) struct DeploymentLease {
    /// Owns the SSH lease process.
    child: Child,
    /// Keeps the remote lock holder's standard input open.
    stdin: Option<ChildStdin>,
    /// Collects SSH diagnostics without blocking the child.
    stderr: Option<JoinHandle<Result<Vec<u8>, io::Error>>>,
    /// Identifies the target host.
    host: String,
}

impl DeploymentLease {
    /// Acquires a non-blocking deployment lease through SSH.
    pub(crate) async fn acquire(host: &str, ssh_target: &str) -> Result<Self, LockError> {
        let mut command = Command::new("ssh");
        command
            .args(ssh::ARGUMENTS)
            .arg(ssh_target)
            .arg(REMOTE_COMMAND);
        Self::spawn(host, command).await
    }

    /// Starts a lease command and waits for its acquisition acknowledgement.
    async fn spawn(host: &str, mut command: Command) -> Result<Self, LockError> {
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command.spawn().map_err(|source| LockError::Spawn {
            host: host.to_owned(),
            source,
        })?;
        let stdin = child
            .stdin
            .take()
            .expect("lease command standard input should be piped");
        let mut stdout = child
            .stdout
            .take()
            .expect("lease command standard output should be piped");
        let mut stderr = child
            .stderr
            .take()
            .expect("lease command standard error should be piped");
        let stderr = tokio::spawn(async move {
            let mut output = Vec::new();
            stderr.read_to_end(&mut output).await?;
            Ok::<_, io::Error>(output)
        });
        let mut acknowledgement = vec![0; ACQUIRED.len()];
        let acquisition =
            tokio::time::timeout(ACQUISITION_TIMEOUT, stdout.read_exact(&mut acknowledgement))
                .await;

        match acquisition {
            Ok(Ok(_)) if acknowledgement == ACQUIRED => Ok(Self {
                child,
                stdin: Some(stdin),
                stderr: Some(stderr),
                host: host.to_owned(),
            }),
            Ok(Ok(_)) => {
                drop(stdin);
                terminate(&mut child).await;
                let _ = stderr.await;
                Err(LockError::InvalidAcknowledgement {
                    host: host.to_owned(),
                })
            }
            Ok(Err(source)) => {
                drop(stdin);
                let status = child
                    .wait()
                    .await
                    .map_err(|wait_source| LockError::Output {
                        host: host.to_owned(),
                        source: wait_source,
                    })?;
                let stderr = stderr_text(stderr, host).await?;
                if status.code() == Some(75) {
                    Err(LockError::Unavailable {
                        host: host.to_owned(),
                    })
                } else if source.kind() == io::ErrorKind::UnexpectedEof {
                    Err(LockError::AcquisitionFailed {
                        host: host.to_owned(),
                        status,
                        stderr,
                    })
                } else {
                    Err(LockError::Output {
                        host: host.to_owned(),
                        source,
                    })
                }
            }
            Err(_) => {
                drop(stdin);
                terminate(&mut child).await;
                let _ = stderr.await;
                Err(LockError::AcquisitionTimedOut {
                    host: host.to_owned(),
                })
            }
        }
    }

    /// Waits for an acquired lease to end unexpectedly.
    pub(crate) async fn lost(&mut self) -> LockError {
        let status = match self.child.wait().await {
            Ok(status) => status,
            Err(source) => {
                return LockError::Output {
                    host: self.host.clone(),
                    source,
                };
            }
        };
        let stderr = match self.stderr.take() {
            Some(stderr) => stderr_text(stderr, &self.host)
                .await
                .unwrap_or_else(|error| error.to_string()),
            None => "no standard error output".to_owned(),
        };
        LockError::Lost {
            host: self.host.clone(),
            status,
            stderr,
        }
    }

    /// Releases the lease by closing SSH standard input.
    pub(crate) async fn release(mut self) -> Result<(), LockError> {
        self.stdin.take();
        let status = match tokio::time::timeout(RELEASE_TIMEOUT, self.child.wait()).await {
            Ok(Ok(status)) => status,
            Ok(Err(source)) => {
                return Err(LockError::Release {
                    host: self.host.clone(),
                    source,
                });
            }
            Err(_) => {
                terminate(&mut self.child).await;
                return Err(LockError::ReleaseTimedOut {
                    host: self.host.clone(),
                });
            }
        };
        let stderr = match self.stderr.take() {
            Some(stderr) => stderr_text(stderr, &self.host).await?,
            None => String::new(),
        };
        if status.success() {
            Ok(())
        } else {
            Err(LockError::ReleaseFailed {
                host: self.host.clone(),
                status,
                stderr,
            })
        }
    }
}

/// Converts collected process diagnostics to a concise string.
async fn stderr_text(
    stderr: JoinHandle<Result<Vec<u8>, io::Error>>,
    host: &str,
) -> Result<String, LockError> {
    let output = stderr.await.map_err(|source| LockError::Output {
        host: host.to_owned(),
        source: io::Error::other(source),
    })?;
    let output = output.map_err(|source| LockError::Output {
        host: host.to_owned(),
        source,
    })?;
    let output = String::from_utf8_lossy(&output).trim().to_owned();
    Ok(if output.is_empty() {
        "no standard error output".to_owned()
    } else {
        output
    })
}

/// Best-effort terminates a lease process after a lifecycle failure.
async fn terminate(child: &mut Child) {
    let _ = child.kill().await;
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use tempfile::tempdir;
    use tokio::process::Command;

    use super::{ACQUIRED, DeploymentLease, LockError};

    /// Creates a local lease command using the production acknowledgement protocol.
    fn command(path: &std::path::Path) -> Command {
        let mut command = Command::new("flock");
        command
            .args(["--exclusive", "--nonblock", "--conflict-exit-code", "75"])
            .arg(path)
            .arg("sh")
            .arg("-c")
            .arg(format!(
                "printf '{}'; exec cat >/dev/null",
                String::from_utf8_lossy(ACQUIRED)
            ));
        command
    }

    /// Checks exclusive acquisition, conflict reporting, and release through EOF.
    #[tokio::test]
    async fn leases_are_exclusive_and_release_on_stdin_eof() {
        let directory = tempdir().expect("temporary directory should be created");
        let path = directory.path().join("deployment.lock");
        let first = DeploymentLease::spawn("test", command(&path))
            .await
            .expect("first lease should be acquired");
        let conflict = DeploymentLease::spawn("test", command(&path)).await;
        assert!(matches!(conflict, Err(LockError::Unavailable { .. })));

        first.release().await.expect("lease should be released");
        DeploymentLease::spawn("test", command(&path))
            .await
            .expect("released lease should be acquirable")
            .release()
            .await
            .expect("second lease should be released");
    }
}
