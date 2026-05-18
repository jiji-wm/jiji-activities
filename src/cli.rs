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
    MoveWindow {
        name: Option<String>,
        /// Follow the window to its new workspace after the move.
        #[arg(long)]
        follow: bool,
        /// Like `--follow`, but also reveal the destination in overview.
        /// Implies `--follow`.
        // The `--overview` → `--follow` implication is resolved canonically
        // in `dispatch` via `resolve_follow_overview`.
        #[arg(long)]
        overview: bool,
    },

    /// Move the focused window to a workspace within the current activity (picker).
    MoveWindowHere {
        /// Follow the window to its new workspace after the move.
        #[arg(long)]
        follow: bool,
        /// Like `--follow`, but also reveal the destination in overview.
        /// Implies `--follow`.
        // The `--overview` → `--follow` implication is resolved canonically
        // in `dispatch` via `resolve_follow_overview`.
        #[arg(long)]
        overview: bool,
    },

    /// Move the focused workspace to an activity (picker if no name).
    MoveWorkspace {
        name: Option<String>,
        /// Follow the workspace to its new activity after the move.
        #[arg(long)]
        follow: bool,
        /// Like `--follow`, but also reveal the destination in overview.
        /// Implies `--follow`.
        // The `--overview` → `--follow` implication is resolved canonically
        // in `dispatch` via `resolve_follow_overview`.
        #[arg(long)]
        overview: bool,
    },

    /// Assign the focused workspace to one or more activities via picker.
    AssignWorkspace {
        /// Follow the focused workspace after the assignment completes.
        #[arg(long)]
        follow: bool,
        /// Like `--follow`, but also reveal the destination in overview.
        /// Implies `--follow`.
        // The `--overview` → `--follow` implication is resolved canonically
        // in `dispatch` via `resolve_follow_overview`.
        #[arg(long)]
        overview: bool,
    },

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

/// Resolve the `--overview` → `--follow` implication.
///
/// `--overview` alone is shorthand for `--follow --overview`; this
/// helper folds that single resolution rule. Lives at module scope so
/// it is reachable from `src/cli.rs::tests` without spinning up clap or
/// IPC, and is invoked from each `Cmd::Move*` / `Cmd::AssignWorkspace`
/// dispatch arm to keep the rule canonical across verbs.
fn resolve_follow_overview(follow: bool, overview: bool) -> (bool, bool) {
    (follow || overview, overview)
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
        Cmd::MoveWindow {
            name,
            follow,
            overview,
        } => {
            let (follow, overview) = resolve_follow_overview(follow, overview);
            cmd_move_window(name, follow, overview)
        }
        Cmd::MoveWindowHere { follow, overview } => {
            let (follow, overview) = resolve_follow_overview(follow, overview);
            cmd_move_window_here(follow, overview)
        }
        Cmd::MoveWorkspace {
            name,
            follow,
            overview,
        } => {
            let (follow, overview) = resolve_follow_overview(follow, overview);
            cmd_move_workspace(name, follow, overview)
        }
        Cmd::AssignWorkspace { follow, overview } => {
            let (follow, overview) = resolve_follow_overview(follow, overview);
            cmd_assign_workspace(follow, overview)
        }
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

fn cmd_move_window(name: Option<String>, follow: bool, overview: bool) -> Result<()> {
    match name {
        Some(n) => {
            // When `--follow` is set the named-arg form also spawns a
            // fuzzel-backed follow picker after the move, so the
            // picker-availability check must fire pre-IPC even on the
            // non-interactive path. Skipping when `follow` is false
            // preserves the "no fuzzel required for plain named-arg"
            // ergonomic.
            if follow {
                picker::ensure_available()
                    .context("verifying move-window follow picker availability")?;
            }
            let mut client = ipc::make_client();
            move_window::run(client.as_mut(), &n, picker::pick_one, follow, overview)
        }
        None => {
            // Verify `fuzzel` is on $PATH BEFORE any IPC round-trip so a
            // missing-dep failure surfaces with a fuzzel-naming stderr
            // message ("fuzzel: not on $PATH (...)") rather than the
            // generic "niri socket unavailable" the IPC layer would
            // produce on a disconnected socket.
            picker::ensure_available().context("verifying move-window picker availability")?;
            let mut client = ipc::make_client();
            move_window::run_picker(
                client.as_mut(),
                picker::pick_one,
                picker::prompt_name,
                follow,
                overview,
            )
            .context("running move-window picker")
        }
    }
}

fn cmd_move_window_here(follow: bool, overview: bool) -> Result<()> {
    // Verify `fuzzel` is on $PATH BEFORE any IPC round-trip — same
    // rationale as cmd_move_window's no-arg branch. `move-window-here`
    // has no named-arg form; it is always picker-driven.
    picker::ensure_available().context("verifying move-window-here picker availability")?;
    let mut client = ipc::make_client();
    move_window::run_here_picker(client.as_mut(), picker::pick_one, follow, overview)
        .context("running move-window-here picker")
}

fn cmd_move_workspace(name: Option<String>, follow: bool, overview: bool) -> Result<()> {
    match name {
        Some(n) => {
            // Same rationale as `cmd_move_window`'s named-arg branch:
            // when `--follow` is set we will spawn a fuzzel-backed follow
            // picker after the move, so the picker-availability check
            // must fire pre-IPC. The plain named-arg form (no `--follow`)
            // remains fuzzel-free.
            if follow {
                picker::ensure_available()
                    .context("verifying move-workspace follow picker availability")?;
            }
            let mut client = ipc::make_client();
            move_workspace::run(client.as_mut(), &n, picker::pick_one, follow, overview)
        }
        None => {
            // Verify `fuzzel` is on $PATH BEFORE any IPC round-trip so a
            // missing-dep failure surfaces with a fuzzel-naming stderr
            // message ("fuzzel: not on $PATH (...)") rather than the
            // generic "niri socket unavailable" the IPC layer would
            // produce on a disconnected socket.
            picker::ensure_available().context("verifying move-workspace picker availability")?;
            let mut client = ipc::make_client();
            move_workspace::run_picker(client.as_mut(), picker::pick_one, follow, overview)
                .context("running move-workspace picker")
        }
    }
}

fn cmd_assign_workspace(follow: bool, overview: bool) -> Result<()> {
    // Verify `rofi` is on $PATH BEFORE any IPC round-trip so a
    // missing-dep failure surfaces with a rofi-naming stderr message
    // rather than the generic "niri socket unavailable" the IPC layer
    // would produce on a disconnected socket.
    picker::multi_select::ensure_available()
        .context("verifying assign-workspace picker availability")?;
    // When `--follow` is set the post-save follow picker uses fuzzel
    // (single-select); rofi handles the assignment picker but the
    // follow stage is always single-select. Pre-verify so a missing
    // fuzzel surfaces with the picker-naming stderr message rather
    // than as a transport-layer failure after the save has already
    // landed. Skipped when `follow` is false to preserve the
    // "no fuzzel required" ergonomic for the plain assign path.
    if follow {
        picker::ensure_available()
            .context("verifying assign-workspace follow picker availability")?;
    }
    let mut client = ipc::make_client();
    assign_workspace::run(client.as_mut(), picker::pick_one, follow, overview)
        .context("running assign-workspace picker")
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Pins the canonical `--overview` → `--follow` implication that the
    /// four mutating-verb dispatch arms (`MoveWindow`, `MoveWindowHere`,
    /// `MoveWorkspace`, `AssignWorkspace`) delegate to. The helper has no
    /// IPC dependency, so the unit test exercises the rule directly
    /// rather than spinning up clap or a `MockClient`.
    #[test]
    fn dispatch_overview_implies_follow() {
        // `--overview` alone is shorthand for `--follow --overview`.
        assert_eq!(resolve_follow_overview(false, true), (true, true));
        // `--follow` alone leaves `overview` false.
        assert_eq!(resolve_follow_overview(true, false), (true, false));
        // Both flags off → both stay off (the default no-op path).
        assert_eq!(resolve_follow_overview(false, false), (false, false));
        // Explicit `--follow --overview` is the canonical form, idempotent.
        assert_eq!(resolve_follow_overview(true, true), (true, true));
    }
}
