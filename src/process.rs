//! Subprocess execution and output reporting.

mod streams;

use std::{
    ffi::OsString,
    io,
    process::{ExitStatus, Stdio},
    string::FromUtf8Error,
    time::Duration,
};

use indicatif::ProgressBar;
use thiserror::Error;
use tokio::{
    process::Command,
    time,
};

use self::streams::capture_output;
use crate::{
    error::Error,
    logs::{CommandLog, RunLog},
};

/// Describes a failure encountered while running a subprocess.
#[derive(Debug, Error)]
pub(crate) enum ProcessError {
    /// Indicates that a command could not be started.
    #[error("failed to run `{command}`")]
    Spawn {
        /// Shows the attempted command.
        command: String,
        /// Contains the operating-system error.
        #[source]
        source: io::Error,
    },
    /// Indicates that a command could not be awaited.
    #[error("failed to wait for `{command}`")]
    Wait {
        /// Shows the attempted command.
        command: String,
        /// Contains the operating-system error.
        #[source]
        source: io::Error,
    },
    /// Indicates that command output could not be read.
    #[error("failed to read output from `{command}`")]
    OutputRead {
        /// Shows the attempted command.
        command: String,
        /// Contains the input/output error.
        #[source]
        source: io::Error,
    },
    /// Indicates that a command returned a failure status.
    #[error("`{command}` exited with {status}: {stderr}")]
    Failed {
        /// Shows the attempted command.
        command: String,
        /// Contains the command's exit status.
        status: ExitStatus,
        /// Contains standard error from the command.
        stderr: String,
    },
    /// Indicates that a command exceeded its execution deadline.
    #[error("`{command}` timed out after {seconds} seconds")]
    TimedOut {
        /// Shows the attempted command.
        command: String,
        /// Contains the configured deadline in seconds.
        seconds: u64,
    },
    /// Indicates that command output was not UTF-8.
    #[error("`{command}` returned non-UTF-8 output")]
    Output {
        /// Shows the attempted command.
        command: String,
        /// Contains the decoding error.
        #[source]
        source: FromUtf8Error,
    },
}

/// Limits subprocess diagnostics included in terminal errors.
const ERROR_OUTPUT_LINES: usize = 20;

/// Returns the trailing standard-error lines for concise diagnostics.
fn error_output(stderr: &[u8]) -> String {
    let value = String::from_utf8_lossy(stderr);
    let mut lines = value
        .lines()
        .rev()
        .take(ERROR_OUTPUT_LINES)
        .collect::<Vec<_>>();
    lines.reverse();
    let summary = lines.join("\n");
    if summary.is_empty() {
        "no standard error output".to_owned()
    } else {
        summary
    }
}

/// Records an execution failure before returning it.
fn fail_command<T>(log: CommandLog<'_>, error: Error) -> Result<T, Error> {
    log.fail(&error.to_string()).map_err(Error::Log)?;
    Err(error)
}

/// Configures and executes one logged subprocess.
pub(crate) struct Invocation<'a> {
    /// Provides the command's diagnostic record.
    logs: &'a RunLog,
    /// Describes the command's role in the workflow.
    label: String,
    /// Identifies the executable.
    program: OsString,
    /// Contains the executable arguments.
    arguments: Vec<OsString>,
    /// Contains environment additions for the child process.
    environment: Vec<(OsString, OsString)>,
    /// Limits command execution when configured.
    timeout: Option<Duration>,
    /// Receives live command output.
    progress: &'a ProgressBar,
}

impl<'a> Invocation<'a> {
    /// Creates an unbounded command without arguments or environment additions.
    pub(crate) fn new(
        logs: &'a RunLog,
        label: impl Into<String>,
        program: impl Into<OsString>,
        progress: &'a ProgressBar,
    ) -> Self {
        Self {
            logs,
            label: label.into(),
            program: program.into(),
            arguments: Vec::new(),
            environment: Vec::new(),
            timeout: None,
            progress,
        }
    }

    /// Adds one argument to the child process.
    #[must_use]
    pub(crate) fn argument(mut self, argument: impl Into<OsString>) -> Self {
        self.arguments.push(argument.into());
        self
    }

    /// Adds arguments to the child process.
    #[must_use]
    pub(crate) fn arguments(
        mut self,
        arguments: impl IntoIterator<Item = impl Into<OsString>>,
    ) -> Self {
        self.arguments.extend(arguments.into_iter().map(Into::into));
        self
    }

    /// Adds an environment variable to the child process.
    #[must_use]
    pub(crate) fn environment(
        mut self,
        name: impl Into<OsString>,
        value: impl Into<OsString>,
    ) -> Self {
        self.environment.push((name.into(), value.into()));
        self
    }

    /// Limits command execution time.
    #[must_use]
    pub(crate) fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// Runs the command and discards its successful standard output.
    pub(crate) async fn status(self) -> Result<(), Error> {
        self.execute(|_output| Ok(())).await
    }

    /// Runs the command and returns its successful standard output.
    pub(crate) async fn output(self) -> Result<String, Error> {
        self.execute(String::from_utf8).await
    }

    /// Executes the configured command and consumes successful standard output.
    async fn execute<T>(
        self,
        consume: impl FnOnce(Vec<u8>) -> Result<T, FromUtf8Error>,
    ) -> Result<T, Error> {
        let mut command_log = self
            .logs
            .command(&self.label, &self.program, &self.arguments)
            .map_err(Error::Log)?;
        let (stdout_log, stderr_log) = command_log.take_outputs();
        let mut process = Command::new(&self.program);
        process
            .args(&self.arguments)
            .envs(self.environment.iter().map(|(name, value)| (name, value)))
            .kill_on_drop(true)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let command = format!("{:?}", process.as_std());
        let mut process = match process.spawn() {
            Ok(process) => process,
            Err(source) => {
                return fail_command(
                    command_log,
                    Error::Process(ProcessError::Spawn { command, source }),
                );
            }
        };
        let stdout = process
            .stdout
            .take()
            .expect("piped standard output should be available");
        let stderr = process
            .stderr
            .take()
            .expect("piped standard error should be available");

        let completion = async {
            tokio::join!(
                process.wait(),
                capture_output(
                    stdout,
                    stderr,
                    stdout_log,
                    stderr_log,
                    &command,
                    self.progress,
                )
            )
        };
        let (status, output) = match self.timeout {
            Some(limit) => match time::timeout(limit, completion).await {
                Ok(completion) => completion,
                Err(_elapsed) => {
                    return fail_command(
                        command_log,
                        Error::Process(ProcessError::TimedOut {
                            command: command.clone(),
                            seconds: limit.as_secs(),
                        }),
                    );
                }
            },
            None => completion.await,
        };
        let status = match status {
            Ok(status) => status,
            Err(source) => {
                return fail_command(
                    command_log,
                    Error::Process(ProcessError::Wait {
                        command: command.clone(),
                        source,
                    }),
                );
            }
        };
        let output = match output {
            Ok(output) => output,
            Err(error) => return fail_command(command_log, error),
        };
        if !status.success() {
            command_log.complete(status).map_err(Error::Log)?;
            return Err(Error::Process(ProcessError::Failed {
                command,
                status,
                stderr: error_output(&output.stderr),
            }));
        }

        match consume(output.stdout) {
            Ok(output) => {
                command_log.complete(status).map_err(Error::Log)?;
                Ok(output)
            }
            Err(source) => fail_command(
                command_log,
                Error::Process(ProcessError::Output { command, source }),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path, time::Duration};

    use indicatif::ProgressBar;
    use tempfile::tempdir;

    use super::{Invocation, ProcessError};
    use crate::{
        error::Error,
        logs::{LogMode, RunLog},
    };

    /// Checks that bounded subprocesses are terminated and reported distinctly.
    #[tokio::test]
    async fn bounded_commands_report_timeouts() {
        let logs = RunLog::start(
            LogMode::Never,
            None,
            Path::new("timeout-test-inventory.toml"),
        )
        .expect("disabled logs should start");
        let progress = ProgressBar::hidden();
        let result = Invocation::new(&logs, "timeout", "sh", &progress)
            .arguments(["-c", "sleep 10"])
            .timeout(Duration::from_millis(20))
            .status()
            .await;

        assert!(matches!(
            result,
            Err(Error::Process(ProcessError::TimedOut { .. }))
        ));
        logs.finish(Some("timed out"))
            .expect("disabled logs should finish");
    }

    /// Checks that output decoding failures are reflected in command metadata.
    #[tokio::test]
    async fn invalid_command_output_is_recorded_as_failed() {
        let base = tempdir().expect("temporary directory should be created");
        let logs = RunLog::start(
            LogMode::Always,
            Some(base.path()),
            Path::new("inventory.toml"),
        )
        .expect("run log should start");
        let progress = ProgressBar::hidden();
        Invocation::new(&logs, "ignored-output", "sh", &progress)
            .arguments(["-c", "printf '\\377'"])
            .status()
            .await
            .expect("discarded output should not require UTF-8");
        let result = Invocation::new(&logs, "invalid-output", "sh", &progress)
            .arguments(["-c", "printf '\\377'"])
            .output()
            .await;
        let error = result.expect_err("non-UTF-8 output should fail");
        assert!(matches!(error, Error::Process(ProcessError::Output { .. })));
        let directory = logs
            .finish(Some("invalid output"))
            .expect("run log should finish")
            .expect("always logs should be retained");
        let run = fs::read_to_string(directory.join("run.log"))
            .expect("lifecycle log should be readable");
        assert!(run.contains("command 002 failed duration_ms="));
        assert!(run.contains("returned non-UTF-8 output"));
    }
}
