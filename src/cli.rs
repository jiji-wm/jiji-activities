//! Clap-derive subcommand surface and per-subcommand dispatch.
//!
//! Each subcommand routes to a `cmd_<name>` helper. All current
//! subcommands (`switch`, `switch-previous`, `move-window`,
//! `move-window-here`, `move-workspace`, `assign-workspace`, `create`,
//! `remove`, `rename`, `save`, `list`) issue real IPC. The dispatch shape
//! and CLI surface are pinned by `tests/cli.rs`.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};

use crate::assign_workspace;
use crate::completions;
use crate::create;
use crate::ipc;
use crate::list::{self, ListOpts};
use crate::move_window;
use crate::move_workspace;
use crate::picker;
use crate::remove;
use crate::rename;
use crate::save;
use crate::switch;
use crate::switch_previous;

/// Ordering policy for activity lists presented to the user.
///
/// Applies to the `switch` picker and the `list` output. The compositor
/// always returns activities in declaration order; the CLI can reorder
/// client-side using the `last_active_seq` field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum Order {
    /// Declaration order as supplied by the compositor. Current behavior
    /// for `list`; opted-in via `--order=static`.
    Static,
    /// Sort by most-recently-used first, using `last_active_seq`
    /// descending. Activities with `seq == 0` (never activated) fall to
    /// the end in declaration order. Default for `switch`.
    Mru,
}

/// Top-level CLI entry. `--version` and `--help` are handled by clap
/// directly; everything else routes through [`Cmd`] and `dispatch`.
#[derive(Debug, Parser)]
#[command(
    name = "jiji-activities",
    version,
    about = "KDE-style Activities for the jiji Wayland compositor."
)]
pub(crate) struct Cli {
    #[command(subcommand)]
    pub(crate) cmd: Cmd,
}

/// One variant per CLI subcommand.
#[derive(Debug, Subcommand)]
pub(crate) enum Cmd {
    /// Switch to an activity by name. Without `name`, opens a picker.
    Switch {
        name: Option<String>,
        /// Ordering for the picker rows. Default: recency (most-recently
        /// used first, using compositor-tracked activation sequence).
        #[arg(long, value_enum, default_value_t = Order::Mru)]
        order: Order,
    },

    /// Switch to the previously-active activity (toggle behaviour).
    #[command(alias = "toggle")]
    SwitchPrevious {
        /// How many steps back in activity history to go. 1 means the
        /// immediately-previous activity; 2 means one further back, etc.
        #[arg(long, default_value_t = 1)]
        depth: u32,
    },

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
        /// Move this specific window id instead of the focused window.
        /// Replaces the "use the focused window" default.
        #[arg(long)]
        window: Option<u64>,
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
        /// Move this specific window id instead of the focused window.
        /// Replaces the "use the focused window" default.
        #[arg(long)]
        window: Option<u64>,
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
        /// Move this specific workspace id instead of the focused workspace.
        /// Replaces the "use the focused workspace" default.
        #[arg(long)]
        workspace: Option<u64>,
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
        /// Assign this specific workspace id instead of the focused workspace.
        /// Replaces the "use the focused workspace" default.
        #[arg(long)]
        workspace: Option<u64>,
    },

    /// Create a new activity with the given name.
    Create { name: String },

    /// Remove an activity by name.
    Remove { name: String },

    /// Rename an activity to a new name. Without `--activity`, opens a picker to choose the target.
    Rename {
        /// New name for the activity.
        name: String,
        /// Target activity to rename (by name or id). Without this, a picker opens.
        #[arg(long)]
        activity: Option<String>,
    },

    /// Save the current activity layout under the given name.
    Save { name: String },

    /// List activities; default human format, `--json`, or named `--format`.
    List {
        #[arg(long, conflicts_with = "format")]
        json: bool,
        #[arg(long, conflicts_with = "json")]
        format: Option<String>,
        /// Narrow output to a single named activity. Unknown name → exit 66.
        #[arg(long)]
        activity: Option<String>,
        /// Ordering for the output rows. Default: declaration order as
        /// supplied by the compositor.
        #[arg(long, value_enum, default_value_t = Order::Static)]
        order: Order,
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

/// Encodes the mutually-dependent `--follow` / `--overview` flag pair as
/// a three-state enum, making the `overview ⟹ follow` invariant
/// unrepresentable rather than enforced by convention.
///
/// Canonically produced by [`resolve_follow_overview`] — the primary site
/// where the raw `bool, bool` pair from clap collapses into this type.
/// Tests may construct variants directly for ergonomic stub injection.
/// Threaded through the four mutating verbs (`move-window`,
/// `move-window-here`, `move-workspace`, `assign-workspace`) down to their
/// `run*` entry points. Does **not** enter `src/follow.rs`; the
/// `dispatch_follow_*` leaves keep their `overview: bool` param and are fed
/// by [`FollowMode::with_overview`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FollowMode {
    /// Neither `--follow` nor `--overview` was supplied. No follow picker
    /// is spawned; window/workspace id capture is skipped.
    None,
    /// `--follow` was supplied (without `--overview`). A follow picker is
    /// spawned after a successful move; no overview is opened.
    Follow,
    /// `--follow --overview` (or `--overview` alone, which implies
    /// `--follow`). A follow picker is spawned and the compositor's
    /// Overview is revealed on confirmation.
    FollowAndOverview,
}

impl FollowMode {
    /// Returns `true` when a follow picker should be spawned after the
    /// move — i.e. for [`Self::Follow`] and [`Self::FollowAndOverview`].
    #[inline]
    pub(crate) fn should_follow(self) -> bool {
        matches!(self, FollowMode::Follow | FollowMode::FollowAndOverview)
    }

    /// Returns `true` when the Overview should be revealed after the follow
    /// confirmation — i.e. for [`Self::FollowAndOverview`] **only**.
    #[inline]
    pub(crate) fn with_overview(self) -> bool {
        matches!(self, FollowMode::FollowAndOverview)
    }
}

/// Resolve the `--overview` → `--follow` implication.
///
/// `--overview` alone is shorthand for `--follow --overview`; this
/// helper folds that single resolution rule into a [`FollowMode`] value,
/// making the invariant unrepresentable rather than convention-enforced.
/// Lives at module scope so it is reachable from `src/cli.rs::tests`
/// without spinning up clap or IPC, and is invoked from each
/// `Cmd::Move*` / `Cmd::AssignWorkspace` dispatch arm to keep the rule
/// canonical across verbs.
fn resolve_follow_overview(follow: bool, overview: bool) -> FollowMode {
    if overview {
        FollowMode::FollowAndOverview
    } else if follow {
        FollowMode::Follow
    } else {
        FollowMode::None
    }
}

/// Routes the parsed [`Cli`] to the appropriate stub.
///
/// Returns [`anyhow::Result`] so future IPC layers can `?`-propagate
/// errors freely; stubs surface their failure as `CliError` via
/// [`Into<anyhow::Error>`], which `main()` recovers by downcasting.
pub(crate) fn dispatch(cli: Cli) -> Result<()> {
    match cli.cmd {
        Cmd::Switch { name, order } => cmd_switch(name, order),
        Cmd::SwitchPrevious { depth } => cmd_switch_previous(depth),
        Cmd::MoveWindow {
            name,
            follow,
            overview,
            window,
        } => {
            let follow_mode = resolve_follow_overview(follow, overview);
            cmd_move_window(name, follow_mode, window)
        }
        Cmd::MoveWindowHere {
            follow,
            overview,
            window,
        } => {
            let follow_mode = resolve_follow_overview(follow, overview);
            cmd_move_window_here(follow_mode, window)
        }
        Cmd::MoveWorkspace {
            name,
            follow,
            overview,
            workspace,
        } => {
            let follow_mode = resolve_follow_overview(follow, overview);
            cmd_move_workspace(name, follow_mode, workspace)
        }
        Cmd::AssignWorkspace {
            follow,
            overview,
            workspace,
        } => {
            let follow_mode = resolve_follow_overview(follow, overview);
            cmd_assign_workspace(follow_mode, workspace)
        }
        Cmd::Create { name } => cmd_create(name),
        Cmd::Remove { name } => cmd_remove(name),
        Cmd::Rename { name, activity } => cmd_rename(name, activity),
        Cmd::Save { name } => cmd_save(name),
        Cmd::List {
            json,
            format,
            activity,
            order,
        } => cmd_list(json, format, activity, order),
        Cmd::Completions { shell } => cmd_completions(shell),
    }
}

fn cmd_switch(name: Option<String>, order: Order) -> Result<()> {
    match name {
        Some(n) => {
            let mut client = ipc::make_client();
            switch::run(client.as_mut(), &n)
        }
        None => {
            // Verify `fuzzel` is on $PATH BEFORE any IPC round-trip so a
            // missing-dep failure surfaces with a fuzzel-naming stderr
            // message ("fuzzel: not on $PATH (...)") rather than
            // the generic "jiji socket unavailable" the IPC layer would
            // produce on a disconnected socket. `run_picker` would also
            // hit this via `pick_one`'s internal re-check, but only
            // after the Activities IPC call — which is the wrong order
            // for diagnostics.
            picker::ensure_available().context("verifying switch picker availability")?;
            let mut client = ipc::make_client();
            switch::run_picker(client.as_mut(), order, picker::pick_one)
                .context("running switch picker")
        }
    }
}

fn cmd_switch_previous(depth: u32) -> Result<()> {
    let mut client = ipc::make_client();
    switch_previous::run(client.as_mut(), depth)
}

fn cmd_move_window(
    name: Option<String>,
    follow_mode: FollowMode,
    window: Option<u64>,
) -> Result<()> {
    match name {
        Some(n) => {
            // When `--follow` is set the named-arg form also spawns a
            // fuzzel-backed follow picker after the move, so the
            // picker-availability check must fire pre-IPC even on the
            // non-interactive path. Skipping when follow is None
            // preserves the "no fuzzel required for plain named-arg"
            // ergonomic.
            if follow_mode.should_follow() {
                picker::ensure_available()
                    .context("verifying move-window follow picker availability")?;
            }
            let mut client = ipc::make_client();
            move_window::run(client.as_mut(), &n, picker::pick_one, follow_mode, window)
        }
        None => {
            // Verify `fuzzel` is on $PATH BEFORE any IPC round-trip so a
            // missing-dep failure surfaces with a fuzzel-naming stderr
            // message ("fuzzel: not on $PATH (...)") rather than the
            // generic "jiji socket unavailable" the IPC layer would
            // produce on a disconnected socket.
            picker::ensure_available().context("verifying move-window picker availability")?;
            let mut client = ipc::make_client();
            move_window::run_picker(
                client.as_mut(),
                picker::pick_one,
                picker::prompt_name,
                follow_mode,
                window,
            )
            .context("running move-window picker")
        }
    }
}

fn cmd_move_window_here(follow_mode: FollowMode, window: Option<u64>) -> Result<()> {
    // Verify `fuzzel` is on $PATH BEFORE any IPC round-trip — same
    // rationale as cmd_move_window's no-arg branch. `move-window-here`
    // has no named-arg form; it is always picker-driven.
    picker::ensure_available().context("verifying move-window-here picker availability")?;
    let mut client = ipc::make_client();
    move_window::run_here_picker(client.as_mut(), picker::pick_one, follow_mode, window)
        .context("running move-window-here picker")
}

fn cmd_move_workspace(
    name: Option<String>,
    follow_mode: FollowMode,
    workspace: Option<u64>,
) -> Result<()> {
    match name {
        Some(n) => {
            // Same rationale as `cmd_move_window`'s named-arg branch:
            // when `--follow` is set we will spawn a fuzzel-backed follow
            // picker after the move, so the picker-availability check
            // must fire pre-IPC. The plain named-arg form (no `--follow`)
            // remains fuzzel-free.
            if follow_mode.should_follow() {
                picker::ensure_available()
                    .context("verifying move-workspace follow picker availability")?;
            }
            let mut client = ipc::make_client();
            move_workspace::run(
                client.as_mut(),
                &n,
                picker::pick_one,
                follow_mode,
                workspace,
            )
        }
        None => {
            // Verify `fuzzel` is on $PATH BEFORE any IPC round-trip so a
            // missing-dep failure surfaces with a fuzzel-naming stderr
            // message ("fuzzel: not on $PATH (...)") rather than the
            // generic "jiji socket unavailable" the IPC layer would
            // produce on a disconnected socket.
            picker::ensure_available().context("verifying move-workspace picker availability")?;
            let mut client = ipc::make_client();
            move_workspace::run_picker(client.as_mut(), picker::pick_one, follow_mode, workspace)
                .context("running move-workspace picker")
        }
    }
}

fn cmd_assign_workspace(follow_mode: FollowMode, workspace: Option<u64>) -> Result<()> {
    // Verify `rofi` is on $PATH BEFORE any IPC round-trip so a
    // missing-dep failure surfaces with a rofi-naming stderr message
    // rather than the generic "jiji socket unavailable" the IPC layer
    // would produce on a disconnected socket.
    picker::multi_select::ensure_available()
        .context("verifying assign-workspace picker availability")?;
    // When `--follow` is set the post-save follow picker uses fuzzel
    // (single-select); rofi handles the assignment picker but the
    // follow stage is always single-select. Pre-verify so a missing
    // fuzzel surfaces with the picker-naming stderr message rather
    // than as a transport-layer failure after the save has already
    // landed. Skipped when follow is None to preserve the
    // "no fuzzel required" ergonomic for the plain assign path.
    if follow_mode.should_follow() {
        picker::ensure_available()
            .context("verifying assign-workspace follow picker availability")?;
    }
    let mut client = ipc::make_client();
    assign_workspace::run(client.as_mut(), picker::pick_one, follow_mode, workspace)
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

fn cmd_rename(name: String, activity: Option<String>) -> Result<()> {
    match activity {
        Some(ref target) => {
            // Named-target path: no picker needed.
            let mut client = ipc::make_client();
            rename::run(
                client.as_mut(),
                &niri_ipc::ActivityReferenceArg::Name(target.clone()),
                &name,
                Some(target.as_str()),
            )
        }
        None => {
            // Picker path: verify fuzzel is available BEFORE any IPC
            // round-trip so a missing-dep failure surfaces with a
            // fuzzel-naming stderr message rather than "jiji socket
            // unavailable" (the IPC layer's error on a dead socket).
            picker::ensure_available().context("verifying rename picker availability")?;
            let mut client = ipc::make_client();
            rename::run_picker(client.as_mut(), &name, picker::pick_one)
                .context("running rename picker")
        }
    }
}

fn cmd_save(name: String) -> Result<()> {
    let mut client = ipc::make_client();
    save::run(client.as_mut(), &name, &save::RealConfigPaths).context("saving activity to config")
}

fn cmd_list(
    json: bool,
    format: Option<String>,
    activity: Option<String>,
    order: Order,
) -> Result<()> {
    let mut client = ipc::make_client();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    list::run(
        client.as_mut(),
        ListOpts {
            json,
            format: format.as_deref(),
            activity: activity.as_deref(),
            order,
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
        // `--overview` alone collapses to FollowAndOverview (implies --follow).
        assert_eq!(
            resolve_follow_overview(false, true),
            FollowMode::FollowAndOverview
        );
        // `--follow` alone → Follow (no overview).
        assert_eq!(resolve_follow_overview(true, false), FollowMode::Follow);
        // Both flags off → None (the default no-op path).
        assert_eq!(resolve_follow_overview(false, false), FollowMode::None);
        // Explicit `--follow --overview` → FollowAndOverview, idempotent.
        assert_eq!(
            resolve_follow_overview(true, true),
            FollowMode::FollowAndOverview
        );
    }

    /// Accessor correctness: pins all five (variant × accessor) cells so
    /// a future inversion of `should_follow` / `with_overview` is caught
    /// immediately rather than silently misrouting follow pickers.
    #[test]
    fn follow_mode_accessors_are_correct() {
        // None: neither follow nor overview.
        assert!(!FollowMode::None.should_follow());
        assert!(!FollowMode::None.with_overview());
        // Follow: follow yes, overview no.
        assert!(FollowMode::Follow.should_follow());
        assert!(!FollowMode::Follow.with_overview());
        // FollowAndOverview: both true.
        assert!(FollowMode::FollowAndOverview.should_follow());
        assert!(FollowMode::FollowAndOverview.with_overview());
    }
}
