//! Post-move focus helpers used by `--follow` flags on move verbs.
//!
//! Scaffolding for the `--follow` / `--overview` UX: after a successful
//! move (window or workspace) the caller may want the compositor to focus
//! the destination workspace, optionally the moved window within it, and
//! optionally open the Overview so the user can see the landing tile in
//! its scene context.
//!
//! These helpers are pure dispatch wrappers — no resolution logic, no
//! snapshot reads, no picker contact. Callers feed in the already-resolved
//! `workspace_id` (and `window_id` for the window-aware variant) and pick
//! the `with_overview` flag based on the verb's flag surface.
//!
//! ## IPC ordering (load-bearing)
//!
//! | Helper                                  | `with_overview: false`             | `with_overview: true`                              |
//! | --------------------------------------- | ---------------------------------- | -------------------------------------------------- |
//! | [`dispatch_follow_workspace`]           | `FocusWorkspace`                   | `FocusWorkspace` → `OpenOverview`                  |
//! | [`dispatch_follow_workspace_and_window`]| `FocusWorkspace` → `FocusWindow`   | `FocusWorkspace` → `FocusWindow` → `OpenOverview`  |
//!
//! `FocusWorkspace` always fires first — it switches the activity +
//! workspace context the subsequent `FocusWindow` resolves against.
//! `FocusWindow` must follow before `OpenOverview` so the user sees the
//! destination tile highlighted under the Overview, not a transient
//! mid-move state.
//!
//! ## Wire shape pins
//!
//! - `Action::FocusWorkspace { reference: WorkspaceReferenceArg::Id(_) }`
//!   — always `Id(_)`, never `Index(_)` or `Name(_)`. Same focus-drift
//!   rationale as `MoveWindowToWorkspace`: resolving by index or name
//!   would risk targeting a different workspace if focus or workspace
//!   ordering changed between the snapshot read and the compositor
//!   processing the action.
//! - `Action::FocusWindow { id: u64 }` — single-field action.
//! - `Action::OpenOverview {}` — unit-shape (no fields). Idempotent when
//!   the Overview is already open. `Action::ToggleOverview` and
//!   `Action::CloseOverview` are intentionally NOT used: toggling on an
//!   already-open Overview would close it, the wrong UX for "show me
//!   where the move landed."
//!
//! ## Reply handling
//!
//! Each action is routed through
//! [`send_expect_handled`]`(client, req, None)`. The `None` reflects that
//! these three actions don't take an activity-name argument, so any wire
//! `"activity not found"` would be a compositor contract violation — the
//! helper's `None` branch correctly routes it to
//! `MalformedResponse(Server)` rather than fabricating an
//! `ActivityNotFound` with an empty name. Each call attaches a
//! `.context(...)` layer naming the step (`"focusing target workspace"`,
//! `"focusing moved window"`, `"opening overview after follow"`) so a
//! `{:#}`-printed `anyhow::Error` discloses which step failed.
//!
//! ## Synthetic-string discipline
//!
//! These helpers do not construct any CLI-internal synthetic
//! `MalformedResponse(Server(_))` strings — every server-error carrier
//! that reaches stderr from this module comes from the compositor's own
//! wire payload via [`send_expect_handled`].

use anyhow::{Context, Result};
use niri_ipc::{Action, Request, WorkspaceReferenceArg};

use crate::ipc::NiriClient;
use crate::ipc_helpers::send_expect_handled;

/// Focuses the destination workspace, and optionally opens the Overview.
///
/// **Contract:**
/// - `with_overview: false` → exactly one IPC round-trip:
///   `Action::FocusWorkspace { reference: Id(workspace_id) }`.
/// - `with_overview: true` → two round-trips in strict order:
///   `Action::FocusWorkspace { .. }` then `Action::OpenOverview {}`.
/// - Every dispatched action expects `Response::Handled`; any other
///   variant surfaces as `MalformedResponse(WrongVariant)` (exit 65)
///   via [`send_expect_handled`].
/// - Errors are wrapped with `.context("focusing target workspace")` /
///   `.context("opening overview after follow")` so the
///   `{:#}`-formatted chain discloses which step failed.
///
/// The workspace reference is always
/// [`WorkspaceReferenceArg::Id`]`(workspace_id)` — never `Index(_)` or
/// `Name(_)`. Index/name resolution would re-introduce the snapshot-vs-
/// dispatch race the rest of this module already takes pains to avoid.
//
// `dead_code` allow: scaffolding helper. Callers in move-workspace
// `--follow` / assign-workspace `--follow` light it up in subsequent
// tasks; landing the helper + its ordering tests in isolation pins the
// IPC contract before any caller can drift it.
#[allow(dead_code)]
pub(crate) fn dispatch_follow_workspace(
    client: &mut dyn NiriClient,
    workspace_id: u64,
    with_overview: bool,
) -> Result<()> {
    let focus_req = Request::Action(Action::FocusWorkspace {
        reference: WorkspaceReferenceArg::Id(workspace_id),
    });
    send_expect_handled(client, focus_req, None).context("focusing target workspace")?;

    if with_overview {
        let overview_req = Request::Action(Action::OpenOverview {});
        send_expect_handled(client, overview_req, None).context("opening overview after follow")?;
    }

    Ok(())
}

/// Focuses the destination workspace, focuses the moved window inside it,
/// and optionally opens the Overview.
///
/// **Contract:**
/// - `with_overview: false` → two IPC round-trips in strict order:
///   `Action::FocusWorkspace { reference: Id(workspace_id) }` then
///   `Action::FocusWindow { id: window_id }`.
/// - `with_overview: true` → three round-trips in strict order:
///   `Action::FocusWorkspace { .. }` then `Action::FocusWindow { .. }`
///   then `Action::OpenOverview {}`.
/// - Ordering rationale: `FocusWorkspace` switches the active workspace
///   context the subsequent `FocusWindow` resolves against;
///   `OpenOverview` fires last so the user sees the destination tile
///   highlighted, not a transient mid-move state.
/// - Every dispatched action expects `Response::Handled`; any other
///   variant surfaces as `MalformedResponse(WrongVariant)` (exit 65)
///   via [`send_expect_handled`].
/// - Errors are wrapped with per-step `.context(...)` layers
///   (`"focusing target workspace"`, `"focusing moved window"`,
///   `"opening overview after follow"`) so the `{:#}`-formatted chain
///   discloses which step failed.
///
/// The workspace reference is always
/// [`WorkspaceReferenceArg::Id`]`(workspace_id)` — never `Index(_)` or
/// `Name(_)`. See [`dispatch_follow_workspace`] for the focus-drift
/// rationale.
//
// `dead_code` allow: scaffolding helper. The move-window `--follow`
// caller lights it up once window-id thread-through lands; landing the
// helper + its ordering tests in isolation pins the IPC contract before
// any caller can drift it.
#[allow(dead_code)]
pub(crate) fn dispatch_follow_workspace_and_window(
    client: &mut dyn NiriClient,
    workspace_id: u64,
    window_id: u64,
    with_overview: bool,
) -> Result<()> {
    let focus_ws_req = Request::Action(Action::FocusWorkspace {
        reference: WorkspaceReferenceArg::Id(workspace_id),
    });
    send_expect_handled(client, focus_ws_req, None).context("focusing target workspace")?;

    let focus_win_req = Request::Action(Action::FocusWindow { id: window_id });
    send_expect_handled(client, focus_win_req, None).context("focusing moved window")?;

    if with_overview {
        let overview_req = Request::Action(Action::OpenOverview {});
        send_expect_handled(client, overview_req, None).context("opening overview after follow")?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use niri_ipc::{Action, Reply, Request, Response, WorkspaceReferenceArg};

    use super::*;
    use crate::ipc::MockClient;

    fn focus_workspace_req(ws_id: u64) -> Request {
        Request::Action(Action::FocusWorkspace {
            reference: WorkspaceReferenceArg::Id(ws_id),
        })
    }

    fn focus_window_req(id: u64) -> Request {
        Request::Action(Action::FocusWindow { id })
    }

    fn open_overview_req() -> Request {
        Request::Action(Action::OpenOverview {})
    }

    /// `with_overview: false`: a single `Action::FocusWorkspace`
    /// round-trip and nothing else. Pins the no-overview-no-window shape.
    #[test]
    fn dispatch_follow_workspace_no_overview_emits_focus_workspace_only() {
        let mut client = MockClient::new();
        client.expect(focus_workspace_req(7), Reply::Ok(Response::Handled));
        dispatch_follow_workspace(&mut client, 7, false).expect("happy path");
        client.assert_consumed_in_order();
    }

    /// `with_overview: true`: `FocusWorkspace` then `OpenOverview`, in
    /// that strict order. MockClient's FIFO queue enforces ordering;
    /// swapping the two `expect` calls below would cause a request
    /// mismatch on the first `send`.
    #[test]
    fn dispatch_follow_workspace_with_overview_emits_focus_workspace_then_open_overview() {
        let mut client = MockClient::new();
        client.expect(focus_workspace_req(7), Reply::Ok(Response::Handled));
        client.expect(open_overview_req(), Reply::Ok(Response::Handled));
        dispatch_follow_workspace(&mut client, 7, true).expect("happy path");
        client.assert_consumed_in_order();
    }

    /// `with_overview: false`: `FocusWorkspace` then `FocusWindow`, in
    /// that strict order. Window-id 42 distinct from workspace-id 7 so
    /// an accidental id swap surfaces in the failure output.
    #[test]
    fn dispatch_follow_workspace_and_window_no_overview_emits_focus_workspace_then_focus_window() {
        let mut client = MockClient::new();
        client.expect(focus_workspace_req(7), Reply::Ok(Response::Handled));
        client.expect(focus_window_req(42), Reply::Ok(Response::Handled));
        dispatch_follow_workspace_and_window(&mut client, 7, 42, false).expect("happy path");
        client.assert_consumed_in_order();
    }

    /// `with_overview: true`: all three actions in strict order —
    /// `FocusWorkspace` → `FocusWindow` → `OpenOverview`. This is the
    /// canonical "follow + overview" shape; the test pins the full
    /// ordering contract.
    #[test]
    fn dispatch_follow_workspace_and_window_with_overview_emits_three_actions_in_order() {
        let mut client = MockClient::new();
        client.expect(focus_workspace_req(7), Reply::Ok(Response::Handled));
        client.expect(focus_window_req(42), Reply::Ok(Response::Handled));
        client.expect(open_overview_req(), Reply::Ok(Response::Handled));
        dispatch_follow_workspace_and_window(&mut client, 7, 42, true).expect("happy path");
        client.assert_consumed_in_order();
    }
}
