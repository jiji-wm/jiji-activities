//! Clap-derive subcommand surface and per-subcommand stub dispatch.
//!
//! Each subcommand currently dispatches to a `cmd_<name>` stub that
//! returns [`CliError::NotImplemented`] (exit code 70). Each stub body
//! will be replaced with a real IPC call against `niri-ipc` as the
//! subcommands are wired up; the dispatch shape and CLI surface are
//! pinned by the integration tests in `tests/cli.rs` so accidental
//! subcommand drops surface as test failures rather than silent regressions.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use crate::error::CliError;
use crate::ipc;
use crate::list::{self, ListOpts};
use crate::picker;
use crate::switch;

/// Top-level CLI entry. `--version` and `--help` are handled by clap
/// directly; everything else routes through [`Cmd`] and `dispatch`.
#[derive(Debug, Parser)]
#[command(
    name = "niri-activities",
    version,
    about = "KDE-style Activities for the niri Wayland compositor."
)]
pub(crate) struct Cli {
    #[command(subcommand)]
    pub(crate) cmd: Cmd,
}

/// One variant per CLI subcommand.
#[derive(Debug, Subcommand)]
pub(crate) enum Cmd {
    /// Switch to an activity by name. Without `name`, opens a picker.
    Switch { name: Option<String> },

    /// Switch to the previously-active activity (toggle behaviour).
    #[command(alias = "toggle")]
    SwitchPrevious,

    /// Move the focused window to an activity (picker if no name).
    MoveWindow { name: Option<String> },

    /// Move the focused workspace to an activity (picker if no name).
    MoveWorkspace { name: Option<String> },

    /// Assign the focused workspace to one or more activities via picker.
    AssignWorkspace,

    /// Create a new activity with the given name.
    Create { name: String },

    /// Remove an activity by name.
    Remove { name: String },

    /// Save the current activity layout under the given name.
    Save { name: String },

    /// List activities; default human format, `--json`, or named `--format`.
    List {
        #[arg(long, conflicts_with = "format")]
        json: bool,
        #[arg(long, conflicts_with = "json")]
        format: Option<String>,
    },
}

/// Routes the parsed [`Cli`] to the appropriate stub.
///
/// Returns [`anyhow::Result`] so future IPC layers can `?`-propagate
/// errors freely; stubs surface their failure as `CliError` via
/// [`Into<anyhow::Error>`], which `main()` recovers by downcasting.
pub(crate) fn dispatch(cli: Cli) -> Result<()> {
    match cli.cmd {
        Cmd::Switch { name } => cmd_switch(name),
        Cmd::SwitchPrevious => cmd_switch_previous(),
        Cmd::MoveWindow { name } => cmd_move_window(name),
        Cmd::MoveWorkspace { name } => cmd_move_workspace(name),
        Cmd::AssignWorkspace => cmd_assign_workspace(),
        Cmd::Create { name } => cmd_create(name),
        Cmd::Remove { name } => cmd_remove(name),
        Cmd::Save { name } => cmd_save(name),
        Cmd::List { json, format } => cmd_list(json, format),
    }
}

fn cmd_switch(name: Option<String>) -> Result<()> {
    match name {
        Some(n) => {
            let mut client = ipc::make_client();
            switch::run(client.as_mut(), &n)
        }
        None => {
            // Verify `fuzzel` is on $PATH BEFORE any IPC round-trip so a
            // missing-dep failure surfaces with a fuzzel-naming stderr
            // message ("fuzzel: not on $PATH (...)") rather than
            // the generic "niri socket unavailable" the IPC layer would
            // produce on a disconnected socket. `run_picker` would also
            // hit this via `pick_one`'s internal re-check, but only
            // after the Activities IPC call — which is the wrong order
            // for diagnostics.
            picker::ensure_available().context("running switch picker")?;
            let mut client = ipc::make_client();
            switch::run_picker(client.as_mut(), picker::pick_one).context("running switch picker")
        }
    }
}

fn cmd_switch_previous() -> Result<()> {
    Err(CliError::NotImplemented("switch-previous").into())
}

fn cmd_move_window(_name: Option<String>) -> Result<()> {
    Err(CliError::NotImplemented("move-window").into())
}

fn cmd_move_workspace(_name: Option<String>) -> Result<()> {
    Err(CliError::NotImplemented("move-workspace").into())
}

fn cmd_assign_workspace() -> Result<()> {
    Err(CliError::NotImplemented("assign-workspace").into())
}

fn cmd_create(_name: String) -> Result<()> {
    Err(CliError::NotImplemented("create").into())
}

fn cmd_remove(_name: String) -> Result<()> {
    Err(CliError::NotImplemented("remove").into())
}

fn cmd_save(_name: String) -> Result<()> {
    Err(CliError::NotImplemented("save").into())
}

fn cmd_list(json: bool, format: Option<String>) -> Result<()> {
    let mut client = ipc::make_client();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    list::run(
        client.as_mut(),
        ListOpts {
            json,
            format: format.as_deref(),
        },
        &mut out,
    )
}
