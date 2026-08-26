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

/// Owns one diagnostic log file and its path.
pub(crate) struct LogFile {
    /// Locates the output file for error reporting.
    path: PathBuf,
    /// Receives subprocess output bytes.
    file: File,
}

impl LogFile {
    /// Creates one diagnostic file within a log directory.
    fn create(directory: &Path, name: &str) -> Result<Self, LogError> {
        let path = directory.join(name);
        let file = File::create(&path)
            .map_err(|source| LogError::Write(PathedError::new(path.clone(), source)))?;
        Ok(Self { path, file })
    }

    /// Returns the file without its diagnostic path.
    fn into_file(self) -> File {
        self.file
    }

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
    _session: &'a LogSession,
    /// Identifies the command when diagnostics are enabled.
    id: Option<usize>,
    /// Groups command lifecycle events.
    span: Span,
    /// Measures command duration monotonically.
    started: Instant,
    /// Records whether the command was explicitly finalized.
    finished: bool,
}

/// Owns the paired output logs allocated for one subprocess.
pub(crate) struct OutputLogs {
    /// Receives standard output.
    pub(crate) stdout: LogFile,
    /// Receives standard error.
    pub(crate) stderr: LogFile,
}

/// Owns the lifecycle and optional output logs allocated for one subprocess.
pub(crate) struct CommandLogs<'a> {
    /// Records command completion, failure, or interruption.
    pub(crate) lifecycle: CommandLog<'a>,
    /// Receives subprocess output when diagnostic logging is enabled.
    pub(crate) output: Option<OutputLogs>,
}

/// Owns resources present only while persistent logging is enabled.
enum LogStorage {
    /// Indicates that the invocation produces no diagnostic artifacts.
    Disabled,
    /// Owns the run directory, tracing writer, and flushing guard.
    Enabled {
        /// Locates the run's diagnostic artifacts.
        directory: PathBuf,
        /// Provides the tracing subscriber's writer.
        writer: NonBlocking,
        /// Flushes and closes the tracing writer when storage is dropped.
        _worker: WorkerGuard,
        /// Selects whether successful run artifacts are retained.
        retain_successful: bool,
        /// Measures run duration monotonically.
        started: Instant,
        /// Allocates command identifiers in start order.
        next_command_id: AtomicUsize,
        /// Records whether the run was explicitly finalized.
        finished: bool,
    },
}

/// Owns the diagnostic artifacts for one Dance invocation.
pub(crate) struct LogSession {
    /// Owns optional persistent logging resources and lifecycle state.
    storage: LogStorage,
}

impl LogSession {
    /// Starts a run log using an optional base directory override.
    pub(crate) fn start(mode: LogMode, base: Option<&Path>) -> Result<Self, LogError> {
        if matches!(mode, LogMode::Never) {
            return Ok(Self {
                storage: LogStorage::Disabled,
            });
        }

        let started = Instant::now();
        let base = match base {
            Some(base) => base.to_path_buf(),
            None => state_directory(env::var_os("XDG_STATE_HOME"), dirs::home_dir())
                .ok_or(LogError::StateDirectoryUnavailable)?
                .join("dance/logs"),
        };
        fs::create_dir_all(&base)
            .map_err(|source| LogError::CreateDirectory(PathedError::new(base.clone(), source)))?;
        let directory = create_run_directory(&base)?;
        let file = LogFile::create(&directory, "run.log")?.into_file();
        let (writer, worker) = NonBlockingBuilder::default().lossy(false).finish(file);
        Ok(Self {
            storage: LogStorage::Enabled {
                directory,
                writer,
                _worker: worker,
                retain_successful: matches!(mode, LogMode::Always),
                started,
                next_command_id: AtomicUsize::new(1),
                finished: false,
            },
        })
    }

    /// Returns a tracing writer when persistent logging is enabled.
    pub(crate) fn writer(&self) -> Option<NonBlocking> {
        match &self.storage {
            LogStorage::Disabled => None,
            LogStorage::Enabled { writer, .. } => Some(writer.clone()),
        }
    }

    /// Returns the run directory when persistent logging is enabled.
    #[cfg(test)]
    fn directory(&self) -> Option<&Path> {
        match &self.storage {
            LogStorage::Disabled => None,
            LogStorage::Enabled { directory, .. } => Some(directory),
        }
    }

    /// Records the invocation after tracing has been initialized.
    pub(crate) fn record_start(&self, inventory: &Path) {
        if matches!(self.storage, LogStorage::Disabled) {
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
    ) -> Result<CommandLogs<'a>, LogError> {
        let started = Instant::now();
        let LogStorage::Enabled {
            directory,
            next_command_id,
            ..
        } = &self.storage
        else {
            return Ok(CommandLogs {
                lifecycle: CommandLog {
                    _session: self,
                    id: None,
                    span: Span::none(),
                    started,
                    finished: false,
                },
                output: None,
            });
        };
        let id = next_command_id.fetch_add(1, Ordering::Relaxed);
        let stdout_name = format!("{id:03}.stdout");
        let stderr_name = format!("{id:03}.stderr");
        let stdout = LogFile::create(directory, &stdout_name)?;
        let stderr = LogFile::create(directory, &stderr_name)?;
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

        Ok(CommandLogs {
            lifecycle: CommandLog {
                _session: self,
                id: Some(id),
                span,
                started,
                finished: false,
            },
            output: Some(OutputLogs { stdout, stderr }),
        })
    }

    /// Finalizes the run and returns the retained log directory.
    pub(crate) fn finish(mut self, error: Option<&str>) -> Result<Option<PathBuf>, LogError> {
        let LogStorage::Enabled {
            directory,
            retain_successful,
            started,
            finished,
            ..
        } = &mut self.storage
        else {
            return Ok(None);
        };
        let directory = directory.clone();
        let duration_ms = elapsed_milliseconds(*started);
        match error {
            Some(error) => tracing::warn!(duration_ms, error, "run failed"),
            None => tracing::info!(duration_ms, "run succeeded"),
        }
        let remove = error.is_none() && !*retain_successful;
        *finished = true;
        drop(self);

        if remove {
            fs::remove_dir_all(&directory).map_err(|source| {
                LogError::RemoveDirectory(PathedError::new(directory.clone(), source))
            })?;
            Ok(None)
        } else {
            Ok(Some(directory))
        }
    }
}

impl CommandLog<'_> {
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

impl Drop for LogSession {
    /// Flushes interrupted runs and preserves their diagnostic directory.
    fn drop(&mut self) {
        let LogStorage::Enabled {
            started, finished, ..
        } = &self.storage
        else {
            return;
        };
        if !finished {
            let duration_ms = elapsed_milliseconds(*started);
            tracing::warn!(duration_ms, "run interrupted");
        }
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

/// Runs a test with an isolated persistent log session and tracing subscriber.
#[cfg(test)]
pub(crate) fn with_test_log_session<T>(
    mode: LogMode,
    base: &Path,
    operation: impl FnOnce(LogSession) -> T,
) -> T {
    static SUBSCRIBER_LOCK: Mutex<()> = Mutex::new(());

    let _subscriber_lock = SUBSCRIBER_LOCK
        .lock()
        .expect("subscriber test lock should not be poisoned");
    let logs = LogSession::start(mode, Some(base)).expect("test log session should start");
    let writer = logs
        .writer()
        .expect("test log session should provide a tracing writer");
    let subscriber = tracing_subscriber::fmt()
        .compact()
        .with_ansi(false)
        .with_target(false)
        .with_writer(writer)
        .finish();
    tracing::subscriber::with_default(subscriber, || operation(logs))
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path, process::Command};

    use tempfile::tempdir;

    use super::{CommandLogs, LogMode, OutputLogs, state_directory, with_test_log_session};

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
        let base = tempdir().expect("temporary directory should be created");
        let directory = with_test_log_session(LogMode::Always, base.path(), |logs| {
            logs.record_start(Path::new("inventory.toml"));
            let CommandLogs {
                lifecycle: command,
                output,
            } = logs
                .command("review party", "ssh", &["party".into()])
                .expect("command log should start");
            let OutputLogs {
                mut stdout,
                mut stderr,
            } = output.expect("command output logs should exist");
            stdout
                .write(b"standard output\n")
                .expect("standard output should be logged");
            stderr
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
        let base = tempdir().expect("temporary directory should be created");
        let interrupted_path =
            with_test_log_session(LogMode::OnError, base.path(), |interrupted| {
                let path = interrupted
                    .directory()
                    .expect("enabled logs should have a directory")
                    .to_path_buf();
                interrupted.record_start(Path::new("inventory.toml"));
                drop(interrupted);
                path
            });
        let trace = fs::read_to_string(interrupted_path.join("run.log"))
            .expect("interrupted trace log should be readable");
        assert!(trace.contains("run interrupted"));

        let successful_path = with_test_log_session(LogMode::OnError, base.path(), |successful| {
            let path = successful
                .directory()
                .expect("enabled logs should have a directory")
                .to_path_buf();
            successful.record_start(Path::new("inventory.toml"));
            assert_eq!(
                successful
                    .finish(None)
                    .expect("successful run should finish"),
                None
            );
            path
        });
        assert!(!successful_path.exists());
    }
}
