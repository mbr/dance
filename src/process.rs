//! Subprocess execution and output reporting.

use std::{
    ffi::OsString,
    io,
    process::Stdio,
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
    logs::{OutputLog, RunLog},
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
    error: Option<Error>,
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
    fn take(self) -> Result<Vec<u8>, Error> {
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
        stdout: stdout_capture.take()?,
        stderr: stderr_capture.take()?,
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

/// Runs a subprocess without returning its standard output.
pub(crate) async fn run_status(
    logs: &RunLog,
    label: &str,
    program: &str,
    arguments: &[OsString],
    progress: &ProgressBar,
) -> Result<(), Error> {
    run_command(logs, label, program, arguments, &[], None, progress)
        .await
        .map(drop)
}

/// Runs a bounded subprocess without returning its standard output.
pub(crate) async fn run_status_bounded(
    logs: &RunLog,
    label: &str,
    program: &str,
    arguments: &[OsString],
    environment: &[(&str, OsString)],
    timeout: Duration,
    progress: &ProgressBar,
) -> Result<(), Error> {
    run_command(
        logs,
        label,
        program,
        arguments,
        environment,
        Some(timeout),
        progress,
    )
    .await
    .map(drop)
}

/// Runs a bounded subprocess and captures its standard output.
pub(crate) async fn run_bounded(
    logs: &RunLog,
    label: &str,
    program: &str,
    arguments: &[OsString],
    timeout: Duration,
    progress: &ProgressBar,
) -> Result<String, Error> {
    run_command(
        logs,
        label,
        program,
        arguments,
        &[],
        Some(timeout),
        progress,
    )
    .await
}

/// Runs a subprocess and captures its standard output.
pub(crate) async fn run(
    logs: &RunLog,
    label: &str,
    program: &str,
    arguments: &[OsString],
    progress: &ProgressBar,
) -> Result<String, Error> {
    run_command(logs, label, program, arguments, &[], None, progress).await
}

/// Runs one configured subprocess and captures its standard output.
async fn run_command(
    logs: &RunLog,
    label: &str,
    program: &str,
    arguments: &[OsString],
    environment: &[(&str, OsString)],
    timeout: Option<Duration>,
    progress: &ProgressBar,
) -> Result<String, Error> {
    let mut command_log = logs.command(label, program, arguments)?;
    let (stdout_log, stderr_log) = command_log.take_outputs();
    let mut process = Command::new(program);
    process
        .args(arguments)
        .envs(environment.iter().cloned())
        .kill_on_drop(true)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let command = format!("{:?}", process.as_std());
    let mut process = match process.spawn() {
        Ok(process) => process,
        Err(source) => {
            let error = Error::CommandSpawn {
                command: command.clone(),
                source,
            };
            command_log.fail(&error.to_string())?;
            return Err(error);
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
            capture_output(stdout, stderr, stdout_log, stderr_log, &command, progress,)
        )
    };
    let (status, output) = match timeout {
        Some(limit) => match time::timeout(limit, completion).await {
            Ok(completion) => completion,
            Err(_elapsed) => {
                let error = Error::CommandTimedOut {
                    command: command.clone(),
                    seconds: limit.as_secs(),
                };
                command_log.fail(&error.to_string())?;
                return Err(error);
            }
        },
        None => completion.await,
    };
    let status = match status {
        Ok(status) => status,
        Err(source) => {
            let error = Error::CommandWait {
                command: command.clone(),
                source,
            };
            command_log.fail(&error.to_string())?;
            return Err(error);
        }
    };
    let output = match output {
        Ok(output) => output,
        Err(error) => {
            command_log.fail(&error.to_string())?;
            return Err(error);
        }
    };
    if !status.success() {
        command_log.complete(status)?;
        return Err(Error::CommandFailed {
            command,
            status,
            stderr: error_output(&output.stderr),
        });
    }

    match String::from_utf8(output.stdout) {
        Ok(stdout) => {
            command_log.complete(status)?;
            Ok(stdout)
        }
        Err(source) => {
            let error = Error::CommandOutput { command, source };
            command_log.fail(&error.to_string())?;
            Err(error)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{ffi::OsString, fs, path::Path, time::Duration};

    use indicatif::ProgressBar;
    use tempfile::tempdir;
    use tokio::{io::AsyncWriteExt, time};

    use super::{OUTPUT_REFRESH_INTERVAL, capture_output, run, run_status_bounded};
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
        let result = run_status_bounded(
            &logs,
            "timeout",
            "sh",
            &[OsString::from("-c"), OsString::from("sleep 10")],
            &[],
            Duration::from_millis(20),
            &progress,
        )
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
        let result = run(
            &logs,
            "invalid-output",
            "sh",
            &[OsString::from("-c"), OsString::from("printf '\\377'")],
            &progress,
        )
        .await;
        let error = result.expect_err("non-UTF-8 output should fail");
        assert!(matches!(error, Error::CommandOutput { .. }));
        let directory = logs
            .finish(Some("invalid output"))
            .expect("run log should finish")
            .expect("always logs should be retained");
        let manifest =
            fs::read_to_string(directory.join("run.toml")).expect("manifest should be readable");
        let command = manifest
            .split_once("[[commands]]")
            .expect("manifest should contain a command")
            .1;
        assert!(command.contains("status = \"failed\""));
        assert!(command.contains("returned non-UTF-8 output"));
    }
}
