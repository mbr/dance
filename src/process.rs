//! Subprocess execution and output reporting.

mod streams;

use std::{
    ffi::OsString, future::IntoFuture, process::ExitStatus, string::FromUtf8Error, time::Duration,
};

use indicatif::ProgressBar;
use thiserror::Error;
use tokio::process::Command;
use tokio_process_tools::{
    ConsumerError, DEFAULT_MAX_BUFFERED_CHUNKS, DEFAULT_READ_CHUNK_SIZE, GracefulShutdown, Process,
    SpawnError, TerminationError, WaitForCompletionOrTerminateResult, WaitOrTerminateError,
};
use tokio_util::sync::CancellationToken;

use self::streams::OutputConsumers;
use crate::{
    error::Error,
    logs::{CommandDiagnostics, CommandLog, LogSession},
};

/// Describes a failure encountered while running a subprocess.
#[derive(Debug, Error)]
pub(crate) enum ProcessError {
    /// Indicates that a command could not be started.
    #[error("failed to run `{command}`")]
    Spawn {
        /// Shows the attempted command.
        command: String,
        /// Contains the process creation error.
        #[source]
        source: SpawnError,
    },
    /// Indicates that a command could not be awaited or cleaned up.
    #[error("failed to wait for `{command}`")]
    Wait {
        /// Shows the attempted command.
        command: String,
        /// Contains the process lifecycle error.
        #[source]
        source: WaitOrTerminateError,
    },
    /// Indicates that a command could not be terminated after interruption.
    #[error("failed to terminate `{command}`")]
    Terminate {
        /// Shows the attempted command.
        command: String,
        /// Contains the process termination error.
        #[source]
        source: TerminationError,
    },
    /// Indicates that command output could not be read.
    #[error("failed to read output from `{command}`")]
    OutputRead {
        /// Shows the attempted command.
        command: String,
        /// Contains the output consumer error.
        #[source]
        source: ConsumerError,
    },
    /// Indicates that command output did not close after process exit.
    #[error("output from `{command}` did not close after {seconds} seconds")]
    OutputDrainTimedOut {
        /// Shows the attempted command.
        command: String,
        /// Contains the output drain deadline in seconds.
        seconds: u64,
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
    /// Indicates that command execution was interrupted.
    #[error("`{command}` was interrupted")]
    Interrupted {
        /// Shows the attempted command.
        command: String,
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
/// Bounds commands without a routine execution deadline.
const MAX_COMMAND_RUNTIME: Duration = Duration::from_secs(365 * 24 * 60 * 60);
/// Allows subprocesses to handle graceful termination.
const TERMINATION_GRACE: Duration = Duration::from_secs(2);

/// Shares subprocess services across one application run.
pub(crate) struct ProcessContext<'a> {
    /// Provides persistent command diagnostics.
    logs: &'a LogSession,
    /// Signals that active commands should terminate.
    cancellation: CancellationToken,
}

impl<'a> ProcessContext<'a> {
    /// Creates subprocess services for one application run.
    pub(crate) fn new(logs: &'a LogSession, cancellation: CancellationToken) -> Self {
        Self { logs, cancellation }
    }

    /// Creates subprocess services with child-scoped cancellation.
    pub(crate) fn child(&self) -> Self {
        Self {
            logs: self.logs,
            cancellation: self.cancellation.child_token(),
        }
    }

    /// Cancels commands using this context without cancelling its parent.
    pub(crate) fn cancel(&self) {
        self.cancellation.cancel();
    }
}

/// Describes how a process wait completed.
enum Completion {
    /// Contains a normal child exit status.
    Exited(ExitStatus),
    /// Indicates that the execution deadline initiated termination.
    TimedOut,
    /// Indicates that application interruption initiated termination.
    Interrupted,
}

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
    log.fail(&error.to_string());
    Err(error)
}

/// Configures and executes one logged subprocess.
pub(crate) struct Invocation<'a> {
    /// Shares run-scoped process services.
    context: &'a ProcessContext<'a>,
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
    /// Creates a command without arguments or environment additions.
    pub(crate) fn new(
        context: &'a ProcessContext<'a>,
        label: impl Into<String>,
        program: impl Into<OsString>,
        progress: &'a ProgressBar,
    ) -> Self {
        Self {
            context,
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
        let CommandDiagnostics {
            log: command_log,
            stdout: stdout_log,
            stderr: stderr_log,
        } = self
            .context
            .logs
            .command(&self.label, &self.program, &self.arguments)
            .map_err(Error::Log)?;
        let mut command = Command::new(&self.program);
        command
            .args(&self.arguments)
            .envs(self.environment.iter().map(|(name, value)| (name, value)));
        let command_display = format!("{:?}", command.as_std());
        if self.context.cancellation.is_cancelled() {
            return fail_command(
                command_log,
                Error::Process(ProcessError::Interrupted {
                    command: command_display,
                }),
            );
        }
        let mut process = match Process::new(command)
            .name(self.label.clone())
            .stdout_and_stderr(|stream| {
                stream
                    .single_subscriber()
                    .reliable_with_backpressure()
                    .replay_all()
                    .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                    .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
            })
            .spawn()
        {
            Ok(process) => process,
            Err(source) => {
                return fail_command(
                    command_log,
                    Error::Process(ProcessError::Spawn {
                        command: command_display,
                        source,
                    }),
                );
            }
        };
        let consumers =
            OutputConsumers::attach(process.stdout(), process.stderr(), stdout_log, stderr_log);
        process.seal_output_replay();
        let mut refresh = consumers.refresh_interval();
        let limit = self.timeout.unwrap_or(MAX_COMMAND_RUNTIME);
        let wait_result = {
            let wait = process
                .wait_for_completion(limit)
                .or_terminate(
                    GracefulShutdown::builder()
                        .unix_sigterm(TERMINATION_GRACE)
                        .windows_ctrl_break(TERMINATION_GRACE)
                        .build(),
                )
                .into_future();
            tokio::pin!(wait);
            loop {
                tokio::select! {
                    result = &mut wait => break Some(result),
                    () = self.context.cancellation.cancelled() => break None,
                    _ = refresh.tick() => consumers.refresh(self.progress),
                }
            }
        };
        let completion = match wait_result {
            Some(Ok(WaitForCompletionOrTerminateResult::Completed(status))) => {
                Completion::Exited(status)
            }
            Some(Ok(WaitForCompletionOrTerminateResult::TerminatedAfterTimeout { .. })) => {
                Completion::TimedOut
            }
            Some(Err(source)) => {
                process.must_not_be_terminated();
                return fail_command(
                    command_log,
                    Error::Process(ProcessError::Wait {
                        command: command_display,
                        source,
                    }),
                );
            }
            None => match process
                .terminate(
                    GracefulShutdown::builder()
                        .unix_sigint(TERMINATION_GRACE)
                        .windows_ctrl_break(TERMINATION_GRACE)
                        .build(),
                )
                .await
            {
                Ok(_status) => Completion::Interrupted,
                Err(source) => {
                    process.must_not_be_terminated();
                    return fail_command(
                        command_log,
                        Error::Process(ProcessError::Terminate {
                            command: command_display,
                            source,
                        }),
                    );
                }
            },
        };
        let output = match consumers.finish(&command_display, self.progress).await {
            Ok(output) => output,
            Err(error) => return fail_command(command_log, error),
        };
        let status = match completion {
            Completion::TimedOut => {
                return fail_command(
                    command_log,
                    Error::Process(ProcessError::TimedOut {
                        command: command_display,
                        seconds: limit.as_secs(),
                    }),
                );
            }
            Completion::Interrupted => {
                return fail_command(
                    command_log,
                    Error::Process(ProcessError::Interrupted {
                        command: command_display,
                    }),
                );
            }
            Completion::Exited(status) => status,
        };
        if !status.success() {
            command_log.complete(status);
            return Err(Error::Process(ProcessError::Failed {
                command: command_display,
                status,
                stderr: error_output(&output.stderr),
            }));
        }

        match consume(output.stdout) {
            Ok(output) => {
                command_log.complete(status);
                Ok(output)
            }
            Err(source) => fail_command(
                command_log,
                Error::Process(ProcessError::Output {
                    command: command_display,
                    source,
                }),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, time::Duration};

    use indicatif::ProgressBar;
    use tempfile::tempdir;
    use tokio_util::sync::CancellationToken;

    use super::{Invocation, ProcessContext, ProcessError};
    use crate::{
        error::Error,
        logs::{LogMode, LogSession, SUBSCRIBER_LOCK, test_subscriber},
    };

    /// Checks bounded termination and explicit cancellation reporting.
    #[tokio::test]
    async fn commands_report_timeouts_and_interruption() {
        let logs = LogSession::start(LogMode::Never, None).expect("disabled logs should start");
        let progress = ProgressBar::hidden();
        let timeout_cancellation = CancellationToken::new();
        let timeout_context = ProcessContext::new(&logs, timeout_cancellation);
        let timeout = Invocation::new(&timeout_context, "timeout", "sh", &progress)
            .arguments(["-c", "sleep 10"])
            .timeout(Duration::from_millis(20))
            .status()
            .await;
        assert!(matches!(
            timeout,
            Err(Error::Process(ProcessError::TimedOut { .. }))
        ));

        let cancellation = CancellationToken::new();
        let interrupted_context = ProcessContext::new(&logs, cancellation.clone());
        let interrupt = async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            cancellation.cancel();
        };
        let command = Invocation::new(&interrupted_context, "interrupted", "sh", &progress)
            .arguments(["-c", "sleep 10"])
            .status();
        let ((), interrupted) = tokio::join!(interrupt, command);
        assert!(matches!(
            interrupted,
            Err(Error::Process(ProcessError::Interrupted { .. }))
        ));
        drop((timeout_context, interrupted_context));
        logs.finish(Some("timed out or interrupted"))
            .expect("disabled logs should finish");
    }

    /// Checks that output decoding failures are reported.
    #[test]
    fn invalid_command_output_is_reported() {
        let _subscriber_lock = SUBSCRIBER_LOCK
            .lock()
            .expect("subscriber test lock should not be poisoned");
        let base = tempdir().expect("temporary directory should be created");
        let mut logs = LogSession::start(LogMode::Always, Some(base.path()))
            .expect("enabled logs should start");
        let subscriber = test_subscriber(&mut logs);
        let _subscriber = tracing::subscriber::set_default(subscriber);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("Tokio runtime should start");
        runtime.block_on(async {
            let progress = ProgressBar::hidden();
            let context = ProcessContext::new(&logs, CancellationToken::new());
            Invocation::new(&context, "ignored-output", "sh", &progress)
                .arguments(["-c", "printf '\\377'"])
                .status()
                .await
                .expect("discarded output should not require UTF-8");
            let result = Invocation::new(&context, "invalid-output", "sh", &progress)
                .arguments(["-c", "printf '\\377'"])
                .output()
                .await;
            let error = result.expect_err("non-UTF-8 output should fail");
            assert!(matches!(error, Error::Process(ProcessError::Output { .. })));
        });
        let directory = logs
            .finish(Some("invalid output"))
            .expect("enabled logs should finish")
            .expect("always logs should be retained");
        assert_eq!(
            fs::read(directory.join("001.stdout")).expect("ignored output log should be readable"),
            [0xff]
        );
        assert_eq!(
            fs::read(directory.join("002.stdout")).expect("invalid output log should be readable"),
            [0xff]
        );
    }
}
