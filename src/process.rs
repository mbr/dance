//! Subprocess execution and output reporting.

use std::{
    ffi::OsString,
    io,
    process::Stdio,
    string::FromUtf8Error,
    sync::{Arc, Mutex},
    time::Duration,
};

use futures::{Stream, StreamExt, stream};
use indicatif::ProgressBar;
use tokio::{
    io::AsyncRead,
    process::Command,
    time::{self, Instant, MissedTickBehavior},
};
use tokio_util::io::{InspectReader, ReaderStream};

use crate::{
    error::Error,
    logs::{CommandLog, LogError, OutputLog, RunLog},
};

/// Limits how often subprocess output changes a progress message.
const OUTPUT_REFRESH_INTERVAL: Duration = Duration::from_millis(100);
/// Limits subprocess diagnostics included in terminal errors.
const ERROR_OUTPUT_LINES: usize = 20;

/// Contains a callback used to retain inspected bytes.
type Inspector = Box<dyn FnMut(&[u8]) + Send>;
/// Contains a chunk stream whose underlying bytes are retained.
type CapturedChunks<R> = ReaderStream<InspectReader<R, Inspector>>;

/// Contains the separately captured subprocess output streams.
struct CapturedOutput {
    /// Contains standard output bytes.
    stdout: Vec<u8>,
    /// Contains standard error bytes.
    stderr: Vec<u8>,
}

/// Contains the sinks fed by one output stream.
struct CaptureState {
    /// Retains output for command results and errors.
    bytes: Vec<u8>,
    /// Writes output to persistent diagnostics when enabled.
    log: Option<OutputLog>,
    /// Retains the first diagnostic write failure.
    error: Option<LogError>,
}

/// Retains and optionally logs bytes inspected from one output stream.
#[derive(Clone)]
struct Capture {
    /// Shares output sinks with the inspection callback.
    state: Arc<Mutex<CaptureState>>,
}

impl Capture {
    /// Creates output sinks for one stream.
    fn new(log: Option<OutputLog>) -> Self {
        Self {
            state: Arc::new(Mutex::new(CaptureState {
                bytes: Vec::new(),
                log,
                error: None,
            })),
        }
    }

    /// Wraps a reader and tees every supplied byte into its sinks.
    fn chunks<R>(&self, reader: R) -> CapturedChunks<R>
    where
        R: AsyncRead + Unpin,
    {
        let capture = self.clone();
        let inspector: Inspector = Box::new(move |bytes| {
            let mut state = capture
                .state
                .lock()
                .expect("output capture lock should not be poisoned");
            state.bytes.extend_from_slice(bytes);
            if state.error.is_none() {
                let result = state.log.as_mut().map(|log| log.write(bytes));
                if let Some(Err(error)) = result {
                    state.error = Some(error);
                }
            }
        });
        ReaderStream::new(InspectReader::new(reader, inspector))
    }

    /// Takes retained bytes or the first diagnostic failure.
    fn take(self) -> Result<Vec<u8>, LogError> {
        let mut state = self
            .state
            .lock()
            .expect("output capture lock should not be poisoned");
        match state.error.take() {
            Some(error) => Err(error),
            None => Ok(std::mem::take(&mut state.bytes)),
        }
    }
}

/// Returns the last nonempty display line in an output chunk.
fn display_message(record: &[u8]) -> Option<String> {
    String::from_utf8_lossy(record)
        .rsplit(['\r', '\n'])
        .find(|line| !line.trim().is_empty())
        .map(str::to_owned)
}

/// Consumes merged output while periodically displaying its latest line.
async fn display_output<B>(
    output: &mut (impl Stream<Item = io::Result<B>> + Unpin),
    progress: &ProgressBar,
) -> io::Result<()>
where
    B: AsRef<[u8]>,
{
    let mut latest = None;
    let mut refresh = time::interval_at(
        Instant::now() + OUTPUT_REFRESH_INTERVAL,
        OUTPUT_REFRESH_INTERVAL,
    );
    refresh.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            record = output.next() => {
                match record {
                    Some(Ok(record)) => {
                        if let Some(message) = display_message(record.as_ref()) {
                            latest = Some(message);
                        }
                    }
                    Some(Err(source)) => return Err(source),
                    None => break,
                }
            }
            _ = refresh.tick() => {
                if let Some(message) = latest.take() {
                    progress.set_message(message);
                }
            }
        }
    }

    if let Some(message) = latest {
        progress.set_message(message);
    }
    Ok(())
}

/// Captures two output streams while displaying their merged lines.
async fn capture_output(
    stdout: impl AsyncRead + Unpin,
    stderr: impl AsyncRead + Unpin,
    stdout_log: Option<OutputLog>,
    stderr_log: Option<OutputLog>,
    command: &str,
    progress: &ProgressBar,
) -> Result<CapturedOutput, Error> {
    let stdout_capture = Capture::new(stdout_log);
    let stderr_capture = Capture::new(stderr_log);
    let stdout = stdout_capture.chunks(stdout);
    let stderr = stderr_capture.chunks(stderr);
    let mut output = stream::select(stdout, stderr);
    display_output(&mut output, progress)
        .await
        .map_err(|source| Error::CommandOutputRead {
            command: command.to_owned(),
            source,
        })?;
    drop(output);
    Ok(CapturedOutput {
        stdout: stdout_capture.take().map_err(Error::Log)?,
        stderr: stderr_capture.take().map_err(Error::Log)?,
    })
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
                return fail_command(command_log, Error::CommandSpawn { command, source });
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
                        Error::CommandTimedOut {
                            command: command.clone(),
                            seconds: limit.as_secs(),
                        },
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
                    Error::CommandWait {
                        command: command.clone(),
                        source,
                    },
                );
            }
        };
        let output = match output {
            Ok(output) => output,
            Err(error) => return fail_command(command_log, error),
        };
        if !status.success() {
            command_log.complete(status).map_err(Error::Log)?;
            return Err(Error::CommandFailed {
                command,
                status,
                stderr: error_output(&output.stderr),
            });
        }

        match consume(output.stdout) {
            Ok(output) => {
                command_log.complete(status).map_err(Error::Log)?;
                Ok(output)
            }
            Err(source) => fail_command(command_log, Error::CommandOutput { command, source }),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path, time::Duration};

    use indicatif::ProgressBar;
    use tempfile::tempdir;
    use tokio::{io::AsyncWriteExt, time};

    use super::{Invocation, OUTPUT_REFRESH_INTERVAL, capture_output};
    use crate::{
        error::Error,
        logs::{LogMode, RunLog},
    };

    /// Checks that merged lines are coalesced without mixing captured streams.
    #[tokio::test]
    async fn merged_output_is_captured_and_coalesced() {
        let (mut stdout_writer, stdout) = tokio::io::duplex(64);
        let (mut stderr_writer, stderr) = tokio::io::duplex(64);
        let writing = async move {
            stderr_writer
                .write_all(b"building\n")
                .await
                .expect("standard error should be written");
            time::sleep(Duration::from_millis(20)).await;
            stdout_writer
                .write_all(b"result")
                .await
                .expect("standard output should be written");
            drop(stdout_writer);
            time::sleep(OUTPUT_REFRESH_INTERVAL + Duration::from_millis(20)).await;
        };
        let progress = ProgressBar::hidden();
        let capture = capture_output(stdout, stderr, None, None, "test", &progress);

        let ((), output) = tokio::join!(writing, capture);
        let output = output.expect("output should be captured");
        assert_eq!(output.stdout, b"result");
        assert_eq!(output.stderr, b"building\n");
        assert_eq!(progress.message(), "result");
    }

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

        assert!(matches!(result, Err(Error::CommandTimedOut { .. })));
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
        assert!(matches!(error, Error::CommandOutput { .. }));
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
