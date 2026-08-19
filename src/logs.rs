//! Persistent workflow and subprocess diagnostics.

#[cfg(unix)]
use std::os::unix::fs::DirBuilderExt;
#[cfg(test)]
use std::sync::Mutex;
use std::{
    env,
    ffi::{OsStr, OsString},
    fs::{self, File},
    io::Write,
    path::{Path, PathBuf},
    process::ExitStatus,
    sync::atomic::{AtomicUsize, Ordering},
    time::Instant,
};

use clap::ValueEnum;
use thiserror::Error;
use tracing::Span;
use tracing_appender::non_blocking::{NonBlocking, NonBlockingBuilder, WorkerGuard};
use uuid::Uuid;

use crate::error::PathedError;

/// Describes a failure encountered while recording diagnostics.
#[derive(Debug, Error)]
pub(crate) enum LogError {
    /// Indicates that the operating system has no state directory.
    #[error("could not determine the user state directory")]
    StateDirectoryUnavailable,
    /// Indicates that a log directory could not be created.
    #[error("failed to create log directory")]
    CreateDirectory(#[source] PathedError),
    /// Indicates that a log file could not be written.
    #[error("failed to write log")]
    Write(#[source] PathedError),
    /// Indicates that a completed log directory could not be removed.
    #[error("failed to remove log directory")]
    RemoveDirectory(#[source] PathedError),
}

/// Selects when completed run logs are retained.
#[derive(Clone, Copy, Debug, ValueEnum)]
pub(crate) enum LogMode {
    /// Retains logs only when the run fails or is interrupted.
    OnError,
    /// Retains logs for every run.
    Always,
    /// Disables run logs.
    Never,
}

/// Writes one subprocess output stream to its diagnostic file.
pub(crate) struct OutputLog {
    /// Locates the output file for error reporting.
    path: PathBuf,
    /// Receives subprocess output bytes.
    file: File,
}

impl OutputLog {
    /// Writes output bytes to the diagnostic file.
    pub(crate) fn write(&mut self, bytes: &[u8]) -> Result<(), LogError> {
        self.file
            .write_all(bytes)
            .map_err(|source| LogError::Write(PathedError::new(self.path.clone(), source)))
    }
}

/// Guards diagnostic artifacts and tracing state for one subprocess.
pub(crate) struct CommandLog<'a> {
    /// Keeps the run's tracing writer alive for the command span.
    _run: &'a RunLog,
    /// Identifies the command when diagnostics are enabled.
    id: Option<usize>,
    /// Groups command lifecycle events.
    span: Span,
    /// Measures command duration monotonically.
    started: Instant,
    /// Receives standard output.
    stdout: Option<OutputLog>,
    /// Receives standard error.
    stderr: Option<OutputLog>,
    /// Records whether the command was explicitly finalized.
    finished: bool,
}

/// Owns the diagnostic artifacts for one Dance invocation.
pub(crate) struct RunLog {
    /// Selects the artifact retention policy.
    mode: LogMode,
    /// Locates the run directory when logging is enabled.
    directory: Option<PathBuf>,
    /// Measures run duration monotonically.
    started: Instant,
    /// Allocates command identifiers in start order.
    next_command_id: AtomicUsize,
    /// Provides the tracing writer before global initialization.
    writer: Option<NonBlocking>,
    /// Flushes and closes the tracing writer.
    worker: Option<WorkerGuard>,
    /// Records whether the run was explicitly finalized.
    finished: bool,
}

impl RunLog {
    /// Starts a run log using an optional base directory override.
    pub(crate) fn start(mode: LogMode, base: Option<&Path>) -> Result<Self, LogError> {
        let started = Instant::now();
        if matches!(mode, LogMode::Never) {
            return Ok(Self {
                mode,
                directory: None,
                started,
                next_command_id: AtomicUsize::new(1),
                writer: None,
                worker: None,
                finished: false,
            });
        }

        let base = match base {
            Some(base) => base.to_path_buf(),
            None => state_directory(env::var_os("XDG_STATE_HOME"), dirs::home_dir())
                .ok_or(LogError::StateDirectoryUnavailable)?
                .join("dance/logs"),
        };
        fs::create_dir_all(&base)
            .map_err(|source| LogError::CreateDirectory(PathedError::new(base.clone(), source)))?;
        let directory = create_run_directory(&base)?;
        let path = directory.join("run.log");
        let file = open_log(&path)?;
        let (writer, worker) = NonBlockingBuilder::default().lossy(false).finish(file);
        Ok(Self {
            mode,
            directory: Some(directory),
            started,
            next_command_id: AtomicUsize::new(1),
            writer: Some(writer),
            worker: Some(worker),
            finished: false,
        })
    }

    /// Takes the tracing writer for global subscriber initialization.
    pub(crate) fn take_writer(&mut self) -> Option<NonBlocking> {
        self.writer.take()
    }

    /// Records the invocation after tracing has been initialized.
    pub(crate) fn record_start(&self, inventory: &Path) {
        if self.directory.is_none() {
            return;
        }
        let arguments = std::env::args_os().collect::<Vec<_>>();
        tracing::info!(format = 2, inventory = ?inventory, ?arguments, "run started");
    }

    /// Records and starts a subprocess.
    pub(crate) fn command<'a>(
        &'a self,
        label: &str,
        program: impl AsRef<OsStr>,
        arguments: &[OsString],
    ) -> Result<CommandLog<'a>, LogError> {
        let started = Instant::now();
        let Some(directory) = &self.directory else {
            return Ok(CommandLog {
                _run: self,
                id: None,
                span: Span::none(),
                started,
                stdout: None,
                stderr: None,
                finished: false,
            });
        };
        let id = self.next_command_id.fetch_add(1, Ordering::Relaxed);
        let stdout_name = format!("{id:03}.stdout");
        let stderr_name = format!("{id:03}.stderr");
        let stdout = directory.join(&stdout_name);
        let stderr = directory.join(&stderr_name);
        let stdout_log = open_log(&stdout)?;
        let stderr_log = open_log(&stderr)?;
        let span = tracing::info_span!("command", id);
        tracing::info!(
            parent: &span,
            label,
            program = ?program.as_ref(),
            ?arguments,
            stdout = stdout_name,
            stderr = stderr_name,
            "command started"
        );

        Ok(CommandLog {
            _run: self,
            id: Some(id),
            span,
            started,
            stdout: Some(OutputLog {
                path: stdout,
                file: stdout_log,
            }),
            stderr: Some(OutputLog {
                path: stderr,
                file: stderr_log,
            }),
            finished: false,
        })
    }

    /// Finalizes the run and returns the retained log directory.
    pub(crate) fn finish(mut self, error: Option<&str>) -> Result<Option<PathBuf>, LogError> {
        let Some(directory) = self.directory.clone() else {
            self.finished = true;
            return Ok(None);
        };
        let duration_ms = elapsed_milliseconds(self.started);
        match error {
            Some(error) => tracing::warn!(duration_ms, error, "run failed"),
            None => tracing::info!(duration_ms, "run succeeded"),
        }
        self.close_writer();

        let retained = if error.is_none() && matches!(self.mode, LogMode::OnError) {
            fs::remove_dir_all(&directory)
                .map_err(|source| LogError::RemoveDirectory(PathedError::new(directory, source)))?;
            None
        } else {
            Some(directory)
        };
        self.finished = true;
        Ok(retained)
    }

    /// Flushes pending tracing events and closes the log file.
    fn close_writer(&mut self) {
        drop(self.worker.take());
    }
}

impl CommandLog<'_> {
    /// Takes the output streams used while the command runs.
    pub(crate) fn take_outputs(&mut self) -> (Option<OutputLog>, Option<OutputLog>) {
        (self.stdout.take(), self.stderr.take())
    }

    /// Completes the command after its output has been captured.
    pub(crate) fn complete(mut self, status: ExitStatus) {
        if self.id.is_none() {
            self.finished = true;
            return;
        }
        let duration_ms = elapsed_milliseconds(self.started);
        let exit_code = status.code();
        if status.success() {
            tracing::info!(parent: &self.span, ?exit_code, duration_ms, "command succeeded");
        } else {
            tracing::warn!(parent: &self.span, ?exit_code, duration_ms, "command failed");
        }
        self.finished = true;
    }

    /// Marks the command as failed outside normal process completion.
    pub(crate) fn fail(mut self, error: &str) {
        if self.id.is_none() {
            self.finished = true;
            return;
        }
        let duration_ms = elapsed_milliseconds(self.started);
        tracing::warn!(parent: &self.span, duration_ms, error, "command failed");
        self.finished = true;
    }
}

impl Drop for CommandLog<'_> {
    /// Preserves commands that were not explicitly finalized.
    fn drop(&mut self) {
        if !self.finished && self.id.is_some() {
            let duration_ms = elapsed_milliseconds(self.started);
            tracing::warn!(parent: &self.span, duration_ms, "command interrupted");
        }
    }
}

impl Drop for RunLog {
    /// Flushes interrupted runs and preserves their diagnostic directory.
    fn drop(&mut self) {
        if self.finished || self.directory.is_none() {
            return;
        }
        let duration_ms = elapsed_milliseconds(self.started);
        tracing::warn!(duration_ms, "run interrupted");
        self.close_writer();
    }
}

/// Finds the XDG state directory from environment and home paths.
fn state_directory(xdg_state_home: Option<OsString>, home: Option<PathBuf>) -> Option<PathBuf> {
    xdg_state_home
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| home.map(|path| path.join(".local/state")))
}

/// Returns a saturated millisecond duration for diagnostic events.
fn elapsed_milliseconds(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// Creates a uniquely named run directory.
fn create_run_directory(base: &Path) -> Result<PathBuf, LogError> {
    let path = base.join(Uuid::now_v7().to_string());
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    builder.mode(0o700);
    builder
        .create(&path)
        .map_err(|source| LogError::CreateDirectory(PathedError::new(path.clone(), source)))?;
    Ok(path)
}

/// Creates a truncated diagnostic file.
fn open_log(path: &Path) -> Result<File, LogError> {
    File::create(path)
        .map_err(|source| LogError::Write(PathedError::new(path.to_path_buf(), source)))
}

/// Serializes tests that replace tracing's thread-local subscriber.
#[cfg(test)]
pub(crate) static SUBSCRIBER_LOCK: Mutex<()> = Mutex::new(());

/// Creates an isolated subscriber for a log test.
#[cfg(test)]
pub(crate) fn test_subscriber(logs: &mut RunLog) -> impl tracing::Subscriber {
    let writer = logs
        .take_writer()
        .expect("enabled logs should provide a tracing writer");
    tracing_subscriber::fmt()
        .compact()
        .with_ansi(false)
        .with_target(false)
        .with_writer(writer)
        .finish()
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path, process::Command};

    use tempfile::tempdir;

    use super::{LogMode, RunLog, SUBSCRIBER_LOCK, state_directory, test_subscriber};

    /// Checks XDG state overrides and the home-directory fallback.
    #[test]
    fn state_directory_is_available_on_all_platforms() {
        let home = tempdir().expect("temporary home directory should be created");
        let state = tempdir().expect("temporary state directory should be created");

        assert_eq!(
            state_directory(
                Some(state.path().as_os_str().to_owned()),
                Some(home.path().to_path_buf()),
            ),
            Some(state.path().to_path_buf())
        );
        assert_eq!(
            state_directory(None, Some(home.path().to_path_buf())),
            Some(home.path().join(".local/state"))
        );
        assert_eq!(
            state_directory(Some("relative".into()), Some(home.path().to_path_buf())),
            Some(home.path().join(".local/state"))
        );
    }

    /// Checks that retained runs contain tracing and output artifacts.
    #[test]
    fn retained_run_contains_command_artifacts() {
        let _subscriber_lock = SUBSCRIBER_LOCK
            .lock()
            .expect("subscriber test lock should not be poisoned");
        let base = tempdir().expect("temporary directory should be created");
        let mut logs =
            RunLog::start(LogMode::Always, Some(base.path())).expect("run log should start");
        let subscriber = test_subscriber(&mut logs);
        let directory = tracing::subscriber::with_default(subscriber, || {
            logs.record_start(Path::new("inventory.toml"));
            let mut command = logs
                .command("review party", "ssh", &["party".into()])
                .expect("command log should start");
            let (mut stdout, mut stderr) = command.take_outputs();
            stdout
                .as_mut()
                .expect("standard output log should exist")
                .write(b"standard output\n")
                .expect("standard output should be logged");
            stderr
                .as_mut()
                .expect("standard error log should exist")
                .write(b"standard error\n")
                .expect("standard error should be logged");
            let status = Command::new("true")
                .status()
                .expect("successful status should be produced");
            command.complete(status);
            logs.finish(None)
                .expect("run log should finish")
                .expect("always logs should be retained")
        });

        assert_eq!(
            fs::read(directory.join("001.stdout")).expect("standard output log should be readable"),
            b"standard output\n"
        );
        assert_eq!(
            fs::read(directory.join("001.stderr")).expect("standard error log should be readable"),
            b"standard error\n"
        );
        let trace =
            fs::read_to_string(directory.join("run.log")).expect("trace log should be readable");
        assert!(trace.contains("run started"));
        assert!(trace.contains("command: command started"), "{trace}");
        assert!(trace.contains("id=1"));
        assert!(trace.contains("label=\"review party\""));
        assert!(trace.contains("stdout=\"001.stdout\""));
        assert!(trace.contains("command: command succeeded"));
        assert!(trace.contains("run succeeded"));
    }

    /// Checks fail-safe retention and successful cleanup for automatic logs.
    #[test]
    fn automatic_logs_preserve_interruptions_and_remove_successes() {
        let _subscriber_lock = SUBSCRIBER_LOCK
            .lock()
            .expect("subscriber test lock should not be poisoned");
        let base = tempdir().expect("temporary directory should be created");
        let mut interrupted =
            RunLog::start(LogMode::OnError, Some(base.path())).expect("run log should start");
        let interrupted_path = interrupted
            .directory
            .clone()
            .expect("enabled logs should have a directory");
        let subscriber = test_subscriber(&mut interrupted);
        tracing::subscriber::with_default(subscriber, || {
            interrupted.record_start(Path::new("inventory.toml"));
            drop(interrupted);
        });
        let trace = fs::read_to_string(interrupted_path.join("run.log"))
            .expect("interrupted trace log should be readable");
        assert!(trace.contains("run interrupted"));

        let mut successful =
            RunLog::start(LogMode::OnError, Some(base.path())).expect("run log should start");
        let successful_path = successful
            .directory
            .clone()
            .expect("enabled logs should have a directory");
        let subscriber = test_subscriber(&mut successful);
        tracing::subscriber::with_default(subscriber, || {
            successful.record_start(Path::new("inventory.toml"));
            assert_eq!(
                successful
                    .finish(None)
                    .expect("successful run should finish"),
                None
            );
        });
        assert!(!successful_path.exists());
    }
}
