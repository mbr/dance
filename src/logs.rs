//! Persistent subprocess diagnostics.

mod manifest;

#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::{
    env,
    ffi::{OsStr, OsString},
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    process::ExitStatus,
    sync::Mutex,
    time::Instant,
};

use clap::ValueEnum;
use uuid::Uuid;

use self::manifest::Manifest;
use crate::error::Error;

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

/// Writes one subprocess output stream to its diagnostic file.
pub(crate) struct OutputLog {
    /// Locates the output file for error reporting.
    path: PathBuf,
    /// Receives subprocess output bytes.
    file: File,
}

impl OutputLog {
    /// Writes output bytes to the diagnostic file.
    pub(crate) fn write(&mut self, bytes: &[u8]) -> Result<(), Error> {
        self.file
            .write_all(bytes)
            .map_err(|source| Error::WriteLog {
                path: self.path.clone(),
                source,
            })
    }
}

/// Guards diagnostic artifacts for one captured subprocess.
pub(crate) struct CommandLog<'a> {
    /// Owns the corresponding run manifest.
    run: &'a RunLog,
    /// Identifies the corresponding manifest record.
    id: Option<usize>,
    /// Measures command duration monotonically.
    started: Instant,
    /// Receives standard output.
    stdout: Option<OutputLog>,
    /// Receives standard error.
    stderr: Option<OutputLog>,
    /// Records whether the command was explicitly finalized.
    finished: bool,
}

/// Owns the diagnostic artifacts for one Dance invocation.
pub(crate) struct RunLog {
    /// Selects the artifact retention policy.
    mode: LogMode,
    /// Locates the run directory when logging is enabled.
    directory: Option<PathBuf>,
    /// Measures run duration monotonically.
    started: Instant,
    /// Protects records shared by concurrent commands.
    manifest: Mutex<Manifest>,
    /// Records whether the run was explicitly finalized.
    finished: bool,
}

impl RunLog {
    /// Starts a run log using an optional base directory override.
    pub(crate) fn start(
        mode: LogMode,
        base: Option<&Path>,
        inventory: &Path,
    ) -> Result<Self, Error> {
        let started = Instant::now();
        let manifest = Manifest::new(
            inventory.to_path_buf(),
            std::env::args_os().map(display_os).collect(),
        );
        if matches!(mode, LogMode::Never) {
            return Ok(Self {
                mode,
                directory: None,
                started,
                manifest: Mutex::new(manifest),
                finished: false,
            });
        }

        let base = match base {
            Some(base) => base.to_path_buf(),
            None => state_directory(env::var_os("XDG_STATE_HOME"), dirs::home_dir())
                .ok_or(Error::StateDirectoryUnavailable)?
                .join("dance/logs"),
        };
        fs::create_dir_all(&base).map_err(|source| Error::CreateLogDirectory {
            path: base.clone(),
            source,
        })?;
        let directory = create_run_directory(&base)?;
        let log = Self {
            mode,
            directory: Some(directory),
            started,
            manifest: Mutex::new(manifest),
            finished: false,
        };
        log.write_manifest()?;
        Ok(log)
    }

    /// Starts a record for a subprocess.
    pub(crate) fn command<'a>(
        &'a self,
        label: &str,
        program: impl AsRef<OsStr>,
        arguments: &[OsString],
    ) -> Result<CommandLog<'a>, Error> {
        let started = Instant::now();
        let Some(directory) = &self.directory else {
            return Ok(CommandLog {
                run: self,
                id: None,
                started,
                stdout: None,
                stderr: None,
                finished: false,
            });
        };
        let mut manifest = self
            .manifest
            .lock()
            .expect("run log manifest lock should not be poisoned");
        let id = manifest.next_command_id();
        let stem = format!("{id:03}-{}", sanitize(label));
        let stdout_name = format!("{stem}.stdout");
        let stderr_name = format!("{stem}.stderr");
        let stdout = directory.join(&stdout_name);
        let stderr = directory.join(&stderr_name);
        let stdout_log = open_secure(&stdout)?;
        let stderr_log = open_secure(&stderr)?;

        let mut command_arguments = Vec::with_capacity(arguments.len() + 1);
        command_arguments.push(display_os(program));
        command_arguments.extend(arguments.iter().map(display_os));
        manifest.start_command(
            id,
            label.to_owned(),
            command_arguments,
            stdout_name,
            stderr_name,
        );
        self.write_locked_manifest(&manifest)?;
        drop(manifest);
        Ok(CommandLog {
            run: self,
            id: Some(id),
            started,
            stdout: Some(OutputLog {
                path: stdout,
                file: stdout_log,
            }),
            stderr: Some(OutputLog {
                path: stderr,
                file: stderr_log,
            }),
            finished: false,
        })
    }

    /// Finalizes the run and returns the retained log directory.
    pub(crate) fn finish(mut self, error: Option<&str>) -> Result<Option<PathBuf>, Error> {
        let Some(directory) = self.directory.clone() else {
            self.finished = true;
            return Ok(None);
        };
        {
            let mut manifest = self
                .manifest
                .lock()
                .expect("run log manifest lock should not be poisoned");
            manifest.finish(error, elapsed_milliseconds(self.started));
            self.write_locked_manifest(&manifest)?;
            drop(manifest);
        }

        let retained = if error.is_none() && matches!(self.mode, LogMode::OnError) {
            fs::remove_dir_all(&directory).map_err(|source| Error::RemoveLogDirectory {
                path: directory,
                source,
            })?;
            None
        } else {
            Some(directory)
        };
        self.finished = true;
        Ok(retained)
    }

    /// Updates run state and persists the manifest.
    fn update_manifest(&self, update: impl FnOnce(&mut Manifest)) -> Result<(), Error> {
        let mut manifest = self
            .manifest
            .lock()
            .expect("run log manifest lock should not be poisoned");
        update(&mut manifest);
        self.write_locked_manifest(&manifest)
    }

    /// Marks an unfinished command as interrupted.
    fn interrupt_command(&self, id: usize, duration_ms: u64) {
        let Ok(mut manifest) = self.manifest.lock() else {
            return;
        };
        manifest.interrupt_command(id, duration_ms);
        let _result = self.write_locked_manifest(&manifest);
    }

    /// Writes the current manifest atomically.
    fn write_manifest(&self) -> Result<(), Error> {
        let manifest = self
            .manifest
            .lock()
            .expect("run log manifest lock should not be poisoned");
        self.write_locked_manifest(&manifest)
    }

    /// Writes a locked manifest atomically.
    fn write_locked_manifest(&self, manifest: &Manifest) -> Result<(), Error> {
        let Some(directory) = &self.directory else {
            return Ok(());
        };
        let contents = toml::to_string_pretty(manifest)
            .map_err(|source| Error::SerializeLogManifest { source })?;
        let path = directory.join("run.toml");
        let temporary = directory.join("run.toml.tmp");
        write_secure(&temporary, contents.as_bytes())?;
        fs::rename(&temporary, &path).map_err(|source| Error::WriteLog {
            path: path.clone(),
            source,
        })?;
        Ok(())
    }
}

impl CommandLog<'_> {
    /// Takes the output streams used while the command runs.
    pub(crate) fn take_outputs(&mut self) -> (Option<OutputLog>, Option<OutputLog>) {
        (self.stdout.take(), self.stderr.take())
    }

    /// Completes the command after its output has been captured.
    pub(crate) fn complete(mut self, status: ExitStatus) -> Result<(), Error> {
        let Some(id) = self.id else {
            self.finished = true;
            return Ok(());
        };
        let result = self.run.update_manifest(|manifest| {
            manifest.complete_command(id, status, elapsed_milliseconds(self.started));
        });
        self.finished = result.is_ok();
        result
    }

    /// Marks the command as failed outside normal process completion.
    pub(crate) fn fail(mut self, error: &str) -> Result<(), Error> {
        let Some(id) = self.id else {
            self.finished = true;
            return Ok(());
        };
        let result = self.run.update_manifest(|manifest| {
            manifest.fail_command(id, error.to_owned(), elapsed_milliseconds(self.started));
        });
        self.finished = result.is_ok();
        result
    }
}

impl Drop for CommandLog<'_> {
    /// Preserves commands that were not explicitly finalized.
    fn drop(&mut self) {
        if !self.finished
            && let Some(id) = self.id
        {
            self.run
                .interrupt_command(id, elapsed_milliseconds(self.started));
        }
    }
}

impl Drop for RunLog {
    /// Preserves unfinished runs and marks them as interrupted.
    fn drop(&mut self) {
        if self.finished || self.directory.is_none() {
            return;
        }
        let Ok(mut manifest) = self.manifest.lock() else {
            return;
        };
        manifest.interrupt(elapsed_milliseconds(self.started));
        let _result = self.write_locked_manifest(&manifest);
    }
}

/// Finds the XDG state directory from environment and home paths.
fn state_directory(xdg_state_home: Option<OsString>, home: Option<PathBuf>) -> Option<PathBuf> {
    xdg_state_home
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| home.map(|path| path.join(".local/state")))
}

/// Returns a saturated millisecond duration for manifest serialization.
fn elapsed_milliseconds(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// Creates a uniquely named run directory.
fn create_run_directory(base: &Path) -> Result<PathBuf, Error> {
    let path = base.join(Uuid::now_v7().to_string());
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    builder.mode(0o700);
    builder
        .create(&path)
        .map_err(|source| Error::CreateLogDirectory {
            path: path.clone(),
            source,
        })?;
    Ok(path)
}

/// Converts an operating-system string for diagnostics.
fn display_os(value: impl AsRef<OsStr>) -> String {
    value.as_ref().to_string_lossy().into_owned()
}

/// Converts a command label into a portable filename component.
fn sanitize(label: &str) -> String {
    let value = label
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '-'
            }
        })
        .collect::<String>();
    value.trim_matches('-').to_owned()
}

/// Opens a truncated file with private permissions.
fn open_secure(path: &Path) -> Result<File, Error> {
    let mut options = OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    options.mode(0o600);
    options.open(path).map_err(|source| Error::WriteLog {
        path: path.to_path_buf(),
        source,
    })
}

/// Writes a file with private permissions.
fn write_secure(path: &Path, contents: &[u8]) -> Result<(), Error> {
    let mut file = open_secure(path)?;
    file.write_all(contents).map_err(|source| Error::WriteLog {
        path: path.to_path_buf(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path, process::Command};

    use tempfile::tempdir;

    use super::{LogMode, RunLog, state_directory};

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

    /// Checks that retained runs contain command metadata and exact output.
    #[test]
    fn retained_run_contains_command_artifacts() {
        let base = tempdir().expect("temporary directory should be created");
        let logs = RunLog::start(
            LogMode::Always,
            Some(base.path()),
            Path::new("inventory.toml"),
        )
        .expect("run log should start");
        let mut command = logs
            .command("review party", "ssh", &["party".into()])
            .expect("command log should start");
        let (mut stdout, mut stderr) = command.take_outputs();
        stdout
            .as_mut()
            .expect("standard output log should exist")
            .write(b"standard output\n")
            .expect("standard output should be logged");
        stderr
            .as_mut()
            .expect("standard error log should exist")
            .write(b"standard error\n")
            .expect("standard error should be logged");
        let status = Command::new("true")
            .status()
            .expect("successful status should be produced");
        command
            .complete(status)
            .expect("command log should complete");
        let directory = logs
            .finish(None)
            .expect("run log should finish")
            .expect("always logs should be retained");

        assert_eq!(
            fs::read(directory.join("001-review-party.stdout"))
                .expect("standard output log should be readable"),
            b"standard output\n"
        );
        assert_eq!(
            fs::read(directory.join("001-review-party.stderr"))
                .expect("standard error log should be readable"),
            b"standard error\n"
        );
        let manifest =
            fs::read_to_string(directory.join("run.toml")).expect("manifest should be readable");
        assert!(manifest.contains("version = 2"));
        assert!(manifest.contains("status = \"succeeded\""));
        assert_eq!(manifest.matches("duration_ms = ").count(), 2);
        assert!(manifest.contains("arguments = [\n    \"ssh\",\n    \"party\",\n]"));
    }

    /// Checks fail-safe retention and successful cleanup for automatic logs.
    #[test]
    fn automatic_logs_preserve_interruptions_and_remove_successes() {
        let base = tempdir().expect("temporary directory should be created");
        let interrupted = RunLog::start(
            LogMode::OnError,
            Some(base.path()),
            Path::new("inventory.toml"),
        )
        .expect("run log should start");
        let interrupted_path = interrupted
            .directory
            .clone()
            .expect("enabled logs should have a directory");
        drop(interrupted);
        let manifest = fs::read_to_string(interrupted_path.join("run.toml"))
            .expect("interrupted manifest should be readable");
        assert!(manifest.contains("status = \"interrupted\""));
        assert!(manifest.contains("duration_ms = "));

        let successful = RunLog::start(
            LogMode::OnError,
            Some(base.path()),
            Path::new("inventory.toml"),
        )
        .expect("run log should start");
        let successful_path = successful
            .directory
            .clone()
            .expect("enabled logs should have a directory");
        assert_eq!(
            successful
                .finish(None)
                .expect("successful run should finish"),
            None
        );
        assert!(!successful_path.exists());
    }
}
