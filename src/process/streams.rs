//! Subprocess stream capture and progress reporting.

use std::{
    io,
    sync::{Arc, Mutex},
    time::Duration,
};

use futures::{Stream, StreamExt, stream};
use indicatif::ProgressBar;
use tokio::{
    io::AsyncRead,
    time::{self, Instant, MissedTickBehavior},
};
use tokio_util::io::{InspectReader, ReaderStream};

use super::ProcessError;
use crate::{
    error::Error,
    logs::{LogError, OutputLog},
};

/// Limits how often subprocess output changes a progress message.
const OUTPUT_REFRESH_INTERVAL: Duration = Duration::from_millis(100);

/// Contains a callback used to retain inspected bytes.
type Inspector = Box<dyn FnMut(&[u8]) + Send>;
/// Contains a chunk stream whose underlying bytes are retained.
type CapturedChunks<R> = ReaderStream<InspectReader<R, Inspector>>;

/// Contains the separately captured subprocess output streams.
pub(super) struct CapturedOutput {
    /// Contains standard output bytes.
    pub(super) stdout: Vec<u8>,
    /// Contains standard error bytes.
    pub(super) stderr: Vec<u8>,
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
pub(super) async fn capture_output(
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
        .map_err(|source| {
            Error::Process(ProcessError::OutputRead {
                command: command.to_owned(),
                source,
            })
        })?;
    drop(output);
    Ok(CapturedOutput {
        stdout: stdout_capture.take().map_err(Error::Log)?,
        stderr: stderr_capture.take().map_err(Error::Log)?,
    })
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
        let capture = capture_output(stdout, stderr, None, None, "test", &progress);

        let ((), output) = tokio::join!(writing, capture);
        let output = output.expect("output should be captured");
        assert_eq!(output.stdout, b"result");
        assert_eq!(output.stderr, b"building\n");
        assert_eq!(progress.message(), "result");
    }
}
