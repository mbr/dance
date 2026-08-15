//! Serializable run-log state.

use std::{path::PathBuf, process::ExitStatus};

use serde::Serialize;

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
    /// Indicates that a diagnostic guard was dropped before completion.
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
pub(super) struct Manifest {
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

impl Manifest {
    /// Creates state for a running Dance invocation.
    pub(super) fn new(inventory: PathBuf, arguments: Vec<String>) -> Self {
        Self {
            version: 1,
            status: Status::Running,
            inventory,
            arguments,
            error: None,
            commands: Vec::new(),
        }
    }

    /// Returns the next command identifier.
    pub(super) fn next_command_id(&self) -> usize {
        self.commands.len() + 1
    }

    /// Starts a command record.
    pub(super) fn start_command(
        &mut self,
        id: usize,
        label: String,
        arguments: Vec<String>,
        stdout: String,
        stderr: String,
    ) {
        self.commands.push(CommandRecord {
            id,
            label,
            arguments,
            status: Status::Running,
            exit_code: None,
            stdout,
            stderr,
            error: None,
        });
    }

    /// Completes a command record from its process status.
    pub(super) fn complete_command(&mut self, id: usize, status: ExitStatus) {
        let record = self.command_mut(id);
        record.status = if status.success() {
            Status::Succeeded
        } else {
            Status::Failed
        };
        record.exit_code = status.code();
    }

    /// Marks a command record as failed outside normal process completion.
    pub(super) fn fail_command(&mut self, id: usize, error: String) {
        let record = self.command_mut(id);
        record.status = Status::Failed;
        record.error = Some(error);
    }

    /// Marks a running command as interrupted.
    pub(super) fn interrupt_command(&mut self, id: usize) {
        let record = self.command_mut(id);
        if matches!(record.status, Status::Running) {
            record.status = Status::Interrupted;
        }
    }

    /// Finalizes the run state.
    pub(super) fn finish(&mut self, error: Option<&str>) {
        self.status = if error.is_some() {
            Status::Failed
        } else {
            Status::Succeeded
        };
        self.error = error.map(str::to_owned);
    }

    /// Marks a running invocation as interrupted.
    pub(super) fn interrupt(&mut self) {
        if matches!(self.status, Status::Running) {
            self.status = Status::Interrupted;
        }
    }

    /// Returns one mutable command record.
    fn command_mut(&mut self, id: usize) -> &mut CommandRecord {
        self.commands
            .get_mut(id - 1)
            .expect("command log ID should identify a manifest record")
    }
}
