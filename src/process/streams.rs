//! Connects subprocess output streams to command results, diagnostic logs, and progress bars.
//!
//! [`Invocation`](super::Invocation) configures `tokio-process-tools` to expose stdout and stderr
//! as reliable, replayable streams. Immediately after spawning the child it calls
//! [`OutputConsumers::attach`] once, before sealing output replay. The resulting consumers perform
//! three jobs for each stream:
//!
//! - retain requested stdout and a bounded stderr suffix for the eventual result or error;
//! - copy every byte to the command's persistent [`LogFile`], when logging is enabled;
//! - extract recent human-readable text for the command's progress bar.
//!
//! While the child runs, `Invocation` polls [`OutputConsumers::refresh_interval`] alongside process
//! completion and cancellation, calling [`OutputConsumers::refresh`] on each tick. Progress updates
//! are lossy presentation only; they do not alter the exact captured bytes.
//!
//! Once the child has exited or been terminated, `Invocation` consumes the collectors through
//! [`OutputConsumers::finish`]. That method waits for both pipes to reach EOF, reports delayed
//! stream or log failures, performs one final progress refresh, and returns the retained stdout and
//! stderr separately. Draining has a timeout because descendant processes can inherit the pipe handles and
//! keep them open after the direct child exits. Normal application code uses
//! [`Invocation`](super::Invocation) rather than this module directly.

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use indicatif::ProgressBar;
use tokio::time;
use tokio_process_tools::{
    Chunk, Consumable, Consumer, Delivery, Next, Replay, SingleSubscriberOutputStream,
    StreamVisitor,
};

use super::{CommandError, ProcessError};
use crate::{
    error::Error,
    logs::{LogError, LogFile, OutputLogs},
};

/// Limits how often subprocess output changes a progress message.
const OUTPUT_REFRESH_INTERVAL: Duration = Duration::from_millis(100);
/// Limits output draining after the child process exits.
const OUTPUT_EOF_TIMEOUT: Duration = Duration::from_secs(3);
/// Bounds standard error retained for terminal failure diagnostics.
const ERROR_CAPTURE_BYTES: usize = 64 * 1024;

/// Contains output retained after both stream consumers reach EOF.
///
/// Requested stdout is complete, while stderr is a bounded suffix used for failure diagnostics.
/// Ordering between the two independent streams is intentionally not reconstructed.
pub(super) struct CapturedOutput {
    /// Contains standard output bytes.
    pub(super) stdout: Vec<u8>,
    /// Contains standard error bytes.
    pub(super) stderr: Vec<u8>,
}

/// Selects how much subprocess output is retained in memory.
enum CaptureBuffer {
    /// Discards bytes after logging and progress inspection.
    Discard,
    /// Retains complete output for a successful command result.
    All(Vec<u8>),
    /// Retains only the trailing bytes needed for failure diagnostics.
    Tail {
        /// Contains the currently retained suffix.
        bytes: Vec<u8>,
        /// Limits the retained suffix.
        capacity: usize,
    },
}

impl CaptureBuffer {
    /// Appends output according to the configured retention policy.
    fn extend(&mut self, input: &[u8]) {
        match self {
            Self::Discard => {}
            Self::All(bytes) => bytes.extend_from_slice(input),
            Self::Tail { bytes, capacity } => {
                if input.len() >= *capacity {
                    bytes.clear();
                    bytes.extend_from_slice(&input[input.len() - *capacity..]);
                    return;
                }
                let overflow = bytes
                    .len()
                    .saturating_add(input.len())
                    .saturating_sub(*capacity);
                if overflow != 0 {
                    bytes.drain(..overflow);
                }
                bytes.extend_from_slice(input);
            }
        }
    }

    /// Returns the retained bytes.
    fn into_bytes(self) -> Vec<u8> {
        match self {
            Self::Discard => Vec::new(),
            Self::All(bytes) | Self::Tail { bytes, .. } => bytes,
        }
    }
}

/// Visits chunks from one subprocess stream without allowing logging failures to stop drainage.
///
/// A write failure is retained and returned at EOF. Continuing to consume chunks prevents the child
/// from blocking indefinitely against a full output pipe while its error is being handled.
struct Capture {
    /// Retains output required for command results or errors.
    buffer: CaptureBuffer,
    /// Writes output to persistent diagnostics when enabled.
    log: Option<LogFile>,
    /// Retains the first diagnostic write failure.
    error: Option<LogError>,
    /// Shares the latest progress record across output streams.
    latest: Arc<Mutex<Option<String>>>,
}

impl StreamVisitor for Capture {
    type Output = Result<Vec<u8>, LogError>;

    /// Processes one raw output chunk.
    fn on_chunk(&mut self, chunk: Chunk) -> Next {
        let bytes = chunk.as_ref();
        self.buffer.extend(bytes);
        if self.error.is_none()
            && let Some(log) = &mut self.log
            && let Err(error) = log.write(bytes)
        {
            self.error = Some(error);
        }
        if let Some(message) = display_message(bytes) {
            *self
                .latest
                .lock()
                .expect("progress output lock should not be poisoned") = Some(message);
        }
        Next::Continue
    }

    /// Returns captured bytes or the diagnostic write failure.
    fn into_output(self) -> Self::Output {
        self.error.map_or(Ok(self.buffer.into_bytes()), Err)
    }
}

/// Owns the sole consumers attached to one subprocess's stdout and stderr streams.
///
/// Construct this immediately after spawning with [`Self::attach`], periodically call
/// [`Self::refresh`] while waiting for the child, and finally consume it with [`Self::finish`] after
/// process completion or termination.
pub(super) struct OutputConsumers {
    /// Captures standard output.
    stdout: Consumer<Result<Vec<u8>, LogError>>,
    /// Captures standard error.
    stderr: Consumer<Result<Vec<u8>, LogError>>,
    /// Retains the latest displayable output record.
    latest: Arc<Mutex<Option<String>>>,
}

impl OutputConsumers {
    /// Attaches the sole consumer to each replayable output stream.
    ///
    /// This must happen before the process output replay is sealed. `logs` contains both output
    /// files when diagnostic logging is enabled and neither otherwise. Calling this after another
    /// consumer has subscribed is a programming error.
    pub(super) fn attach<D, R>(
        stdout: &SingleSubscriberOutputStream<D, R>,
        stderr: &SingleSubscriberOutputStream<D, R>,
        logs: Option<OutputLogs>,
        capture_stdout: bool,
    ) -> Self
    where
        D: Delivery,
        R: Replay,
    {
        let (stdout_log, stderr_log) = match logs {
            Some(OutputLogs { stdout, stderr }) => (Some(stdout), Some(stderr)),
            None => (None, None),
        };
        let latest = Arc::new(Mutex::new(None));
        let stdout_buffer = if capture_stdout {
            CaptureBuffer::All(Vec::new())
        } else {
            CaptureBuffer::Discard
        };
        let stderr_buffer = CaptureBuffer::Tail {
            bytes: Vec::new(),
            capacity: ERROR_CAPTURE_BYTES,
        };
        Self {
            stdout: attach(stdout, stdout_log, Arc::clone(&latest), stdout_buffer),
            stderr: attach(stderr, stderr_log, Arc::clone(&latest), stderr_buffer),
            latest,
        }
    }

    /// Creates the timer the process wait loop polls to coalesce progress updates.
    ///
    /// Output capture and diagnostic writes happen as chunks arrive; only terminal redraws wait for
    /// this timer.
    pub(super) fn refresh_interval(&self) -> time::Interval {
        let mut interval = time::interval(OUTPUT_REFRESH_INTERVAL);
        interval.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
        interval
    }

    /// Moves the latest displayable output record into the progress bar, if one is pending.
    ///
    /// Stdout and stderr share one slot, so whichever stream most recently produced displayable
    /// text determines the next message.
    pub(super) fn refresh(&self, progress: &ProgressBar) {
        refresh_progress(&self.latest, progress);
    }

    /// Drains both streams after child completion and returns their retained bytes.
    ///
    /// This consumes the collectors, waits for both pipes concurrently, propagates consumer and
    /// diagnostic-write errors, and performs a final progress refresh. It fails after
    /// [`OUTPUT_EOF_TIMEOUT`] rather than hanging on pipe handles inherited by descendants.
    pub(super) async fn finish(
        self,
        command: &str,
        progress: &ProgressBar,
    ) -> Result<CapturedOutput, Error> {
        let latest = Arc::clone(&self.latest);
        let completion = async {
            let (stdout, stderr) = tokio::join!(self.stdout.wait(), self.stderr.wait());
            let stdout = stdout
                .map_err(|source| output_read_error(command, source))?
                .map_err(Error::Log)?;
            let stderr = stderr
                .map_err(|source| output_read_error(command, source))?
                .map_err(Error::Log)?;
            Ok(CapturedOutput { stdout, stderr })
        };
        let output = time::timeout(OUTPUT_EOF_TIMEOUT, completion)
            .await
            .map_err(|_elapsed| {
                Error::Process(ProcessError::OutputDrainTimedOut {
                    command: command.to_owned(),
                    seconds: OUTPUT_EOF_TIMEOUT.as_secs(),
                })
            })??;
        refresh_progress(&latest, progress);
        Ok(output)
    }
}

/// Attaches one output consumer.
fn attach<D, R>(
    stream: &SingleSubscriberOutputStream<D, R>,
    log: Option<LogFile>,
    latest: Arc<Mutex<Option<String>>>,
    buffer: CaptureBuffer,
) -> Consumer<Result<Vec<u8>, LogError>>
where
    D: Delivery,
    R: Replay,
{
    stream
        .consume(Capture {
            buffer,
            log,
            error: None,
            latest,
        })
        .expect("the output stream should not have another consumer")
}

/// Wraps an output consumer failure with command context.
fn output_read_error(command: &str, source: tokio_process_tools::ConsumerError) -> Error {
    Error::Process(ProcessError::OutputRead(CommandError::new(
        command.to_owned(),
        source,
    )))
}

/// Displays the latest output retained for progress reporting.
fn refresh_progress(latest: &Mutex<Option<String>>, progress: &ProgressBar) {
    if let Some(message) = latest
        .lock()
        .expect("progress output lock should not be poisoned")
        .take()
    {
        progress.set_message(message);
    }
}

/// Returns the last nonempty display line in an output chunk.
fn display_message(record: &[u8]) -> Option<String> {
    String::from_utf8_lossy(record)
        .rsplit(['\r', '\n'])
        .find(|line| !line.trim().is_empty())
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::{CaptureBuffer, display_message};

    /// Checks carriage-return and newline progress extraction.
    #[test]
    fn display_message_uses_the_last_nonempty_record() {
        assert_eq!(
            display_message(b"building\rchecking\n\n"),
            Some("checking".to_owned())
        );
        assert_eq!(display_message(b"\r\n"), None);
    }

    /// Checks that diagnostic output retains only its configured suffix.
    #[test]
    fn tail_capture_is_bounded() {
        let mut buffer = CaptureBuffer::Tail {
            bytes: Vec::new(),
            capacity: 5,
        };
        buffer.extend(b"abc");
        buffer.extend(b"defg");
        assert_eq!(buffer.into_bytes(), b"cdefg");

        let mut buffer = CaptureBuffer::Tail {
            bytes: Vec::new(),
            capacity: 3,
        };
        buffer.extend(b"discard-prefix");
        assert_eq!(buffer.into_bytes(), b"fix");
    }
}
