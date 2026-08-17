//! Subprocess output capture and progress reporting.

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
    logs::{LogError, OutputLog},
};

/// Limits how often subprocess output changes a progress message.
const OUTPUT_REFRESH_INTERVAL: Duration = Duration::from_millis(100);
/// Limits output draining after the child process exits.
const OUTPUT_EOF_TIMEOUT: Duration = Duration::from_secs(3);

/// Contains the separately captured subprocess output streams.
pub(super) struct CapturedOutput {
    /// Contains standard output bytes.
    pub(super) stdout: Vec<u8>,
    /// Contains standard error bytes.
    pub(super) stderr: Vec<u8>,
}

/// Captures, logs, and reports one subprocess output stream.
struct Capture {
    /// Retains output for command results and errors.
    bytes: Vec<u8>,
    /// Writes output to persistent diagnostics when enabled.
    log: Option<OutputLog>,
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

/// Owns consumers attached to a subprocess's output streams.
pub(super) struct OutputConsumers {
    /// Captures standard output.
    stdout: Consumer<Result<Vec<u8>, LogError>>,
    /// Captures standard error.
    stderr: Consumer<Result<Vec<u8>, LogError>>,
    /// Retains the latest displayable output record.
    latest: Arc<Mutex<Option<String>>>,
}

impl OutputConsumers {
    /// Attaches consumers to both output streams.
    pub(super) fn attach<D, R>(
        stdout: &SingleSubscriberOutputStream<D, R>,
        stderr: &SingleSubscriberOutputStream<D, R>,
        stdout_log: Option<OutputLog>,
        stderr_log: Option<OutputLog>,
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

    /// Creates the timer used to coalesce progress updates.
    pub(super) fn refresh_interval(&self) -> time::Interval {
        let mut interval = time::interval(OUTPUT_REFRESH_INTERVAL);
        interval.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
        interval
    }

    /// Displays the latest observed subprocess output.
    pub(super) fn refresh(&self, progress: &ProgressBar) {
        refresh_progress(&self.latest, progress);
    }

    /// Drains both output streams and returns their captured bytes.
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
    log: Option<OutputLog>,
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
