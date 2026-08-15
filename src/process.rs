//! Subprocess execution and output reporting.

use std::{
    ffi::OsString,
    io,
    pin::Pin,
    process::Stdio,
    task::{Context, Poll},
    time::Duration,
};

use futures::{Stream, StreamExt, stream};
use indicatif::ProgressBar;
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, BufReader, ReadBuf},
    process::Command,
    time::{self, Instant, MissedTickBehavior},
};
use tokio_stream::wrappers::LinesStream;

use crate::{error::Error, logs::RunLog};

/// Limits how often subprocess output changes a progress message.
const OUTPUT_REFRESH_INTERVAL: Duration = Duration::from_millis(100);
/// Limits subprocess diagnostics included in terminal errors.
const ERROR_OUTPUT_LINES: usize = 20;

/// Contains a line stream whose underlying bytes are retained.
type CapturedLines<R> = LinesStream<BufReader<CapturingReader<R>>>;

/// Contains the separately captured subprocess output streams.
struct CapturedOutput {
    /// Contains standard output bytes.
    stdout: Vec<u8>,
    /// Contains standard error bytes.
    stderr: Vec<u8>,
}

/// Copies bytes read from an asynchronous reader into a capture buffer.
struct CapturingReader<R> {
    /// Supplies bytes to the consumer.
    reader: R,
    /// Retains every byte supplied by the reader.
    captured: Vec<u8>,
}

impl<R> CapturingReader<R> {
    /// Creates a reader with an empty capture buffer.
    fn new(reader: R) -> Self {
        Self {
            reader,
            captured: Vec::new(),
        }
    }

    /// Returns all bytes supplied by the reader.
    fn into_captured(self) -> Vec<u8> {
        self.captured
    }
}

impl<R> AsyncRead for CapturingReader<R>
where
    R: AsyncRead + Unpin,
{
    fn poll_read(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let previous_length = buffer.filled().len();
        match Pin::new(&mut this.reader).poll_read(context, buffer) {
            Poll::Ready(Ok(())) => {
                this.captured
                    .extend_from_slice(&buffer.filled()[previous_length..]);
                Poll::Ready(Ok(()))
            }
            result => result,
        }
    }
}

/// Converts an asynchronous byte reader into captured text lines.
fn captured_lines<R>(reader: R) -> CapturedLines<R>
where
    R: AsyncRead + Unpin,
{
    LinesStream::new(BufReader::new(CapturingReader::new(reader)).lines())
}

/// Recovers the bytes retained beneath a completed line stream.
fn captured_bytes<R>(lines: CapturedLines<R>) -> Vec<u8>
where
    R: AsyncRead + Unpin,
{
    lines.into_inner().into_inner().into_inner().into_captured()
}

/// Returns the last nonempty display line in a text record.
fn display_message(record: &str) -> Option<String> {
    let line = record.rsplit(['\r', '\n']).next()?;
    if line.trim().is_empty() {
        None
    } else {
        Some(line.to_owned())
    }
}

/// Displays one coalesced output line.
fn display_line(progress: &ProgressBar, message: String) {
    progress.set_message(message);
}

/// Consumes merged output while periodically displaying its latest line.
async fn display_output(
    output: &mut (impl Stream<Item = io::Result<String>> + Unpin),
    progress: &ProgressBar,
) -> io::Result<()> {
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
                        if let Some(message) = display_message(&record) {
                            latest = Some(message);
                        }
                    }
                    Some(Err(source)) => return Err(source),
                    None => break,
                }
            }
            _ = refresh.tick() => {
                if let Some(message) = latest.take() {
                    display_line(progress, message);
                }
            }
        }
    }

    if let Some(message) = latest {
        display_line(progress, message);
    }
    Ok(())
}

/// Captures two output streams while displaying their merged lines.
async fn capture_output(
    stdout: impl AsyncRead + Unpin,
    stderr: impl AsyncRead + Unpin,
    progress: &ProgressBar,
) -> io::Result<CapturedOutput> {
    let stdout = captured_lines(stdout);
    let stderr = captured_lines(stderr);
    let mut output = stream::select(stdout, stderr);
    display_output(&mut output, progress).await?;
    let (stdout, stderr) = output.into_inner();
    Ok(CapturedOutput {
        stdout: captured_bytes(stdout),
        stderr: captured_bytes(stderr),
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

/// Runs a subprocess and captures its standard output.
pub(crate) async fn run(
    logs: &RunLog,
    label: &str,
    program: &str,
    arguments: &[OsString],
    progress: &ProgressBar,
) -> Result<String, Error> {
    let command_log = logs.command(label, program, arguments)?;
    let mut process = Command::new(program);
    process
        .args(arguments)
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
            logs.fail_command(&command_log, &error.to_string())?;
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

    let (status, output) = tokio::join!(process.wait(), capture_output(stdout, stderr, progress));
    let status = match status {
        Ok(status) => status,
        Err(source) => {
            let error = Error::CommandWait {
                command: command.clone(),
                source,
            };
            logs.fail_command(&command_log, &error.to_string())?;
            return Err(error);
        }
    };
    let output = match output {
        Ok(output) => output,
        Err(source) => {
            let error = Error::CommandOutputRead {
                command: command.clone(),
                source,
            };
            logs.fail_command(&command_log, &error.to_string())?;
            return Err(error);
        }
    };
    logs.complete_command(&command_log, &output.stdout, &output.stderr, status)?;
    if !status.success() {
        return Err(Error::CommandFailed {
            command,
            status,
            stderr: error_output(&output.stderr),
        });
    }

    String::from_utf8(output.stdout).map_err(|source| Error::CommandOutput { command, source })
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use indicatif::ProgressBar;
    use tokio::{io::AsyncWriteExt, time};

    use super::{OUTPUT_REFRESH_INTERVAL, capture_output};

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
        let capture = capture_output(stdout, stderr, &progress);

        let ((), output) = tokio::join!(writing, capture);
        let output = output.expect("output should be captured");
        assert_eq!(output.stdout, b"result");
        assert_eq!(output.stderr, b"building\n");
        assert_eq!(progress.message(), "result");
    }
}
