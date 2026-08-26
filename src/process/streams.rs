//! Connects subprocess output streams to command results, diagnostic logs, and progress bars.
//!
//! [`Invocation`](super::Invocation) configures `tokio-process-tools` to expose stdout and stderr
//! as reliable, replayable streams. Immediately after spawning the child it calls
//! [`OutputConsumers::attach`] once, before sealing output replay. The resulting consumers perform
//! three jobs for each stream:
//!
//! - retain the exact bytes for the eventual command result or error;
//! - copy the bytes to the command's persistent [`LogFile`], when logging is enabled;
//! - extract recent human-readable text for the command's progress bar.
//!
//! While the child runs, `Invocation` polls [`OutputConsumers::refresh_interval`] alongside process
//! completion and cancellation, calling [`OutputConsumers::refresh`] on each tick. Progress updates
//! are lossy presentation only; they do not alter the exact captured bytes.
//!
//! Once the child has exited or been terminated, `Invocation` consumes the collectors through
//! [`OutputConsumers::finish`]. That method waits for both pipes to reach EOF, reports delayed
//! stream or log failures, performs one final progress refresh, and returns stdout and stderr
//! separately. Draining has a timeout because descendant processes can inherit the pipe handles and
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

use super::ProcessError;
use crate::{
    error::Error,
    logs::{LogError, LogFile},
};

/// Limits how often subprocess output changes a progress message.
const OUTPUT_REFRESH_INTERVAL: Duration = Duration::from_millis(100);
/// Limits output draining after the child process exits.
const OUTPUT_EOF_TIMEOUT: Duration = Duration::from_secs(3);

/// Contains the exact output bytes returned after both stream consumers reach EOF.
///
/// Stream ordering is preserved within stdout and stderr, but ordering between the two independent
/// streams is intentionally not reconstructed.
pub(super) struct CapturedOutput {
    /// Contains standard output bytes.
    pub(super) stdout: Vec<u8>,
    /// Contains standard error bytes.
    pub(super) stderr: Vec<u8>,
}

/// Visits chunks from one subprocess stream without allowing logging failures to stop drainage.
///
/// A write failure is retained and returned at EOF. Continuing to consume chunks prevents the child
/// from blocking indefinitely against a full output pipe while its error is being handled.
struct Capture {
    /// Retains output for command results and errors.
    bytes: Vec<u8>,
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
        self.bytes.extend_from_slice(bytes);
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
        self.error.map_or(Ok(self.bytes), Err)
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
    /// This must happen before the process output replay is sealed. `stdout_log` and `stderr_log`
    /// are the optional files obtained from the corresponding command log. Calling this after
    /// another consumer has subscribed is a programming error.
    pub(super) fn attach<D, R>(
        stdout: &SingleSubscriberOutputStream<D, R>,
        stderr: &SingleSubscriberOutputStream<D, R>,
        stdout_log: Option<LogFile>,
        stderr_log: Option<LogFile>,
    ) -> Self
    where
        D: Delivery,
        R: Replay,
    {
        let latest = Arc::new(Mutex::new(None));
        Self {
            stdout: attach(stdout, stdout_log, Arc::clone(&latest)),
            stderr: attach(stderr, stderr_log, Arc::clone(&latest)),
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

    /// Drains both streams after child completion and returns their exact captured bytes.
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
) -> Consumer<Result<Vec<u8>, LogError>>
where
    D: Delivery,
    R: Replay,
{
    stream
        .consume(Capture {
            bytes: Vec::new(),
            log,
            error: None,
            latest,
        })
        .expect("the output stream should not have another consumer")
}

/// Wraps an output consumer failure with command context.
fn output_read_error(command: &str, source: tokio_process_tools::ConsumerError) -> Error {
    Error::Process(ProcessError::OutputRead {
        command: command.to_owned(),
        source,
    })
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
    use super::display_message;

    /// Checks carriage-return and newline progress extraction.
    #[test]
    fn display_message_uses_the_last_nonempty_record() {
        assert_eq!(
            display_message(b"building\rchecking\n\n"),
            Some("checking".to_owned())
        );
        assert_eq!(display_message(b"\r\n"), None);
    }
}
