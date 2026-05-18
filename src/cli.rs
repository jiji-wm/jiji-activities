//! Clap-derive subcommand surface and per-subcommand dispatch.
//!
//! Each subcommand routes to a `cmd_<name>` helper. All current
//! subcommands (`switch`, `switch-previous`, `move-window`,
//! `move-window-here`, `move-workspace`, `assign-workspace`, `create`,
//! `remove`, `save`, `list`) issue real IPC. The dispatch shape and CLI
//! surface are pinned by `tests/cli.rs`.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use crate::assign_workspace;
use crate::completions;
use crate::create;
use crate::ipc;
use crate::list::{self, ListOpts};
use crate::move_window;
use crate::move_workspace;
use crate::picker;
use crate::remove;
use crate::save;
use crate::switch;
use crate::switch_previous;

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

    /// Move the focused window to a workspace in an activity (picker if no name).
    MoveWindow { name: Option<String> },

    /// Move the focused window to a workspace within the current activity (picker).
    MoveWindowHere,

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

    /// Emit a shell completion script for the given shell to stdout.
    ///
    /// Fish output is augmented with dynamic activity-name completion;
    /// other shells emit the `clap_complete` base only. See
    /// [`crate::completions`].
    Completions {
        #[arg(value_enum)]
        shell: clap_complete::Shell,
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
        Cmd::MoveWindowHere => cmd_move_window_here(),
        Cmd::MoveWorkspace { name } => cmd_move_workspace(name),
        Cmd::AssignWorkspace => cmd_assign_workspace(),
        Cmd::Create { name } => cmd_create(name),
        Cmd::Remove { name } => cmd_remove(name),
        Cmd::Save { name } => cmd_save(name),
        Cmd::List { json, format } => cmd_list(json, format),
        Cmd::Completions { shell } => cmd_completions(shell),
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
            picker::ensure_available().context("verifying switch picker availability")?;
            let mut client = ipc::make_client();
            switch::run_picker(client.as_mut(), picker::pick_one).context("running switch picker")
        }
    }
}

fn cmd_switch_previous() -> Result<()> {
    let mut client = ipc::make_client();
    switch_previous::run(client.as_mut())
}

fn cmd_move_window(name: Option<String>) -> Result<()> {
    match name {
        Some(n) => {
            let mut client = ipc::make_client();
            move_window::run(client.as_mut(), &n)
        }
        None => {
            // Verify `fuzzel` is on $PATH BEFORE any IPC round-trip so a
            // missing-dep failure surfaces with a fuzzel-naming stderr
            // message ("fuzzel: not on $PATH (...)") rather than the
            // generic "niri socket unavailable" the IPC layer would
            // produce on a disconnected socket.
            picker::ensure_available().context("verifying move-window picker availability")?;
            let mut client = ipc::make_client();
            move_window::run_picker(client.as_mut(), picker::pick_one)
                .context("running move-window picker")
        }
    }
}

fn cmd_move_window_here() -> Result<()> {
    // Verify `fuzzel` is on $PATH BEFORE any IPC round-trip — same
    // rationale as cmd_move_window's no-arg branch. `move-window-here`
    // has no named-arg form; it is always picker-driven.
    picker::ensure_available().context("verifying move-window-here picker availability")?;
    let mut client = ipc::make_client();
    move_window::run_here_picker(client.as_mut(), picker::pick_one)
        .context("running move-window-here picker")
}

fn cmd_move_workspace(name: Option<String>) -> Result<()> {
    match name {
        Some(n) => {
            let mut client = ipc::make_client();
            move_workspace::run(client.as_mut(), &n)
        }
        None => {
            // Verify `fuzzel` is on $PATH BEFORE any IPC round-trip so a
            // missing-dep failure surfaces with a fuzzel-naming stderr
            // message ("fuzzel: not on $PATH (...)") rather than the
            // generic "niri socket unavailable" the IPC layer would
            // produce on a disconnected socket.
            picker::ensure_available().context("verifying move-workspace picker availability")?;
            let mut client = ipc::make_client();
            move_workspace::run_picker(client.as_mut(), picker::pick_one)
                .context("running move-workspace picker")
        }
    }
}

fn cmd_assign_workspace() -> Result<()> {
    // Verify `rofi` is on $PATH BEFORE any IPC round-trip so a
    // missing-dep failure surfaces with a rofi-naming stderr message
    // rather than the generic "niri socket unavailable" the IPC layer
    // would produce on a disconnected socket.
    picker::multi_select::ensure_available()
        .context("verifying assign-workspace picker availability")?;
    let mut client = ipc::make_client();
    assign_workspace::run(client.as_mut()).context("running assign-workspace picker")
}

fn cmd_create(name: String) -> Result<()> {
    let mut client = ipc::make_client();
    create::run(client.as_mut(), &name)
}

fn cmd_remove(name: String) -> Result<()> {
    let mut client = ipc::make_client();
    remove::run(client.as_mut(), &name)
}

fn cmd_save(name: String) -> Result<()> {
    let mut client = ipc::make_client();
    save::run(client.as_mut(), &name, &save::RealConfigPaths).context("saving activity to config")
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

fn cmd_completions(shell: clap_complete::Shell) -> Result<()> {
    completions::run(shell)
}
