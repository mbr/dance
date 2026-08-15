//! Persistent subprocess diagnostics.

use std::{
    ffi::{OsStr, OsString},
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    process::{self, ExitStatus},
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

use clap::ValueEnum;
use serde::Serialize;

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

/// Describes the state of a run or command.
#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "kebab-case")]
enum Status {
    /// Indicates that work has not completed.
    Running,
    /// Indicates successful completion.
    Succeeded,
    /// Indicates failed completion.
    Failed,
    /// Indicates that the run guard was dropped before completion.
    Interrupted,
}

/// Describes one captured subprocess.
#[derive(Serialize)]
struct CommandRecord {
    /// Uniquely identifies the command within the run.
    id: usize,
    /// Describes the command's role in the workflow.
    label: String,
    /// Contains the program and its arguments.
    arguments: Vec<String>,
    /// Describes how the command completed.
    status: Status,
    /// Contains the process exit code when available.
    #[serde(skip_serializing_if = "Option::is_none")]
    exit_code: Option<i32>,
    /// Locates captured standard output relative to the run directory.
    stdout: String,
    /// Locates captured standard error relative to the run directory.
    stderr: String,
    /// Describes failures outside a process exit status.
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// Describes a complete Dance invocation.
#[derive(Serialize)]
struct Manifest {
    /// Identifies the manifest format.
    version: u8,
    /// Describes how the invocation completed.
    status: Status,
    /// Locates the selected inventory.
    inventory: PathBuf,
    /// Contains the Dance process arguments.
    arguments: Vec<String>,
    /// Describes the final workflow failure.
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    /// Contains subprocesses in start order.
    commands: Vec<CommandRecord>,
}

/// Identifies files belonging to one captured subprocess.
pub(crate) struct CommandLog {
    /// Identifies the corresponding manifest record.
    id: Option<usize>,
    /// Locates the standard output file.
    stdout: Option<PathBuf>,
    /// Locates the standard error file.
    stderr: Option<PathBuf>,
}

/// Owns the diagnostic artifacts for one Dance invocation.
pub(crate) struct RunLog {
    /// Selects the artifact retention policy.
    mode: LogMode,
    /// Locates the run directory when logging is enabled.
    directory: Option<PathBuf>,
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
        let manifest = Manifest {
            version: 1,
            status: Status::Running,
            inventory: inventory.to_path_buf(),
            arguments: std::env::args_os().map(display_os).collect(),
            error: None,
            commands: Vec::new(),
        };
        if matches!(mode, LogMode::Never) {
            return Ok(Self {
                mode,
                directory: None,
                manifest: Mutex::new(manifest),
                finished: false,
            });
        }

        let base = match base {
            Some(base) => base.to_path_buf(),
            None => dirs::state_dir()
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
            manifest: Mutex::new(manifest),
            finished: false,
        };
        log.write_manifest()?;
        Ok(log)
    }

    /// Starts a record for a subprocess.
    pub(crate) fn command(
        &self,
        label: &str,
        program: &str,
        arguments: &[OsString],
    ) -> Result<CommandLog, Error> {
        let Some(directory) = &self.directory else {
            return Ok(CommandLog {
                id: None,
                stdout: None,
                stderr: None,
            });
        };
        let mut manifest = self
            .manifest
            .lock()
            .expect("run log manifest lock should not be poisoned");
        let id = manifest.commands.len() + 1;
        let stem = format!("{id:03}-{}", sanitize(label));
        let stdout_name = format!("{stem}.stdout");
        let stderr_name = format!("{stem}.stderr");
        let stdout = directory.join(&stdout_name);
        let stderr = directory.join(&stderr_name);
        write_secure(&stdout, &[])?;
        write_secure(&stderr, &[])?;

        let mut command_arguments = Vec::with_capacity(arguments.len() + 1);
        command_arguments.push(program.to_owned());
        command_arguments.extend(arguments.iter().map(display_os));
        manifest.commands.push(CommandRecord {
            id,
            label: label.to_owned(),
            arguments: command_arguments,
            status: Status::Running,
            exit_code: None,
            stdout: stdout_name,
            stderr: stderr_name,
            error: None,
        });
        self.write_locked_manifest(&manifest)?;
        Ok(CommandLog {
            id: Some(id),
            stdout: Some(stdout),
            stderr: Some(stderr),
        })
    }

    /// Completes a subprocess record and writes its captured output.
    pub(crate) fn complete_command(
        &self,
        command: &CommandLog,
        stdout: &[u8],
        stderr: &[u8],
        status: ExitStatus,
    ) -> Result<(), Error> {
        let Some(id) = command.id else {
            return Ok(());
        };
        write_secure(
            command
                .stdout
                .as_deref()
                .expect("enabled command log should have a stdout path"),
            stdout,
        )?;
        write_secure(
            command
                .stderr
                .as_deref()
                .expect("enabled command log should have a stderr path"),
            stderr,
        )?;
        let mut manifest = self
            .manifest
            .lock()
            .expect("run log manifest lock should not be poisoned");
        let record = manifest
            .commands
            .get_mut(id - 1)
            .expect("command log ID should identify a manifest record");
        record.status = if status.success() {
            Status::Succeeded
        } else {
            Status::Failed
        };
        record.exit_code = status.code();
        self.write_locked_manifest(&manifest)
    }

    /// Marks a subprocess record as failed outside normal process completion.
    pub(crate) fn fail_command(&self, command: &CommandLog, error: &str) -> Result<(), Error> {
        let Some(id) = command.id else {
            return Ok(());
        };
        let mut manifest = self
            .manifest
            .lock()
            .expect("run log manifest lock should not be poisoned");
        let record = manifest
            .commands
            .get_mut(id - 1)
            .expect("command log ID should identify a manifest record");
        record.status = Status::Failed;
        record.error = Some(error.to_owned());
        self.write_locked_manifest(&manifest)
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
            manifest.status = if error.is_some() {
                Status::Failed
            } else {
                Status::Succeeded
            };
            manifest.error = error.map(str::to_owned);
            self.write_locked_manifest(&manifest)?;
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

impl Drop for RunLog {
    /// Preserves unfinished runs and marks them as interrupted.
    fn drop(&mut self) {
        if self.finished || self.directory.is_none() {
            return;
        }
        let mut manifest = self
            .manifest
            .lock()
            .expect("run log manifest lock should not be poisoned");
        if matches!(manifest.status, Status::Running) {
            manifest.status = Status::Interrupted;
            let _result = self.write_locked_manifest(&manifest);
        }
    }
}

/// Creates a uniquely named run directory.
fn create_run_directory(base: &Path) -> Result<PathBuf, Error> {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis());
    let stem = format!("{timestamp}-{}", process::id());
    for suffix in 0_u32.. {
        let name = if suffix == 0 {
            stem.clone()
        } else {
            format!("{stem}-{suffix}")
        };
        let path = base.join(name);
        match fs::create_dir(&path) {
            Ok(()) => {
                secure_directory(&path)?;
                return Ok(path);
            }
            Err(source) if source.kind() == io::ErrorKind::AlreadyExists => {}
            Err(source) => return Err(Error::CreateLogDirectory { path, source }),
        }
    }
    unreachable!("run log suffix space should not be exhausted")
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

/// Writes a file with private permissions.
fn write_secure(path: &Path, contents: &[u8]) -> Result<(), Error> {
    let mut options = OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;

        options.mode(0o600);
    }
    let mut file = options.open(path).map_err(|source| Error::WriteLog {
        path: path.to_path_buf(),
        source,
    })?;
    file.write_all(contents).map_err(|source| Error::WriteLog {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(())
}

/// Restricts access to a run directory.
fn secure_directory(path: &Path) -> Result<(), Error> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(|source| {
            Error::CreateLogDirectory {
                path: path.to_path_buf(),
                source,
            }
        })?;
    }
    Ok(())
}
