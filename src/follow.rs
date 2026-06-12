//! Post-move focus helpers used by `--follow` flags on move verbs.
//!
//! After a successful move (window or workspace, or an assign-workspace
//! save) the caller may want the compositor to focus the destination
//! workspace, optionally the moved window within it, optionally switch
//! into a destination activity, and optionally open the Overview so the
//! user can see the landing tile in its scene context.
//!
//! These helpers are pure dispatch wrappers — no resolution logic, no
//! snapshot reads, no picker contact. Callers feed in the already-resolved
//! `workspace_id` (and `window_id` / `activity_name` for the richer
//! variants) and pick the `with_overview` flag based on the verb's flag
//! surface.
//!
//! ## IPC ordering (load-bearing)
//!
//! | Helper                                       | `with_overview: false`                              | `with_overview: true`                                                |
//! | -------------------------------------------- | --------------------------------------------------- | -------------------------------------------------------------------- |
//! | [`dispatch_follow_workspace`]                | `FocusWorkspace`                                    | `FocusWorkspace` → `OpenOverview`                                    |
//! | [`dispatch_follow_workspace_and_window`]     | `FocusWorkspace` → `FocusWindow`                    | `FocusWorkspace` → `FocusWindow` → `OpenOverview`                    |
//! | [`dispatch_follow_activity_and_workspace`]   | `SwitchActivity` → `FocusWorkspace`                 | `SwitchActivity` → `FocusWorkspace` → `OpenOverview`                 |
//!
//! `FocusWorkspace` always fires after the activity switch (when present)
//! — it switches the workspace context the subsequent `FocusWindow`
//! resolves against. `FocusWindow` must follow before `OpenOverview` so
//! the user sees the destination tile highlighted under the Overview,
//! not a transient mid-move state.
//!
//! ## Wire shape pins
//!
//! - `Action::FocusWorkspace { reference: WorkspaceReferenceArg::Id(_),
//!   activity: None }` — `reference` is always `Id(_)`, never `Index(_)`
//!   or `Name(_)`. Same focus-drift rationale as `MoveWindowToWorkspace`:
//!   resolving by index or name would risk targeting a different
//!   workspace if focus or workspace ordering changed between the
//!   snapshot read and the compositor processing the action. `activity`
//!   is always `None`: it only narrows index/name lookups to a single
//!   activity, but an `Id(_)` reference is already globally unambiguous,
//!   so activity scoping would be a no-op here. (In
//!   [`dispatch_follow_activity_and_workspace`] the active activity is
//!   changed by a preceding `SwitchActivity`, not by scoping this
//!   lookup — switching the active activity and narrowing a lookup are
//!   distinct operations.)
//! - `Action::FocusWindow { id: u64 }` — single-field action.
//! - `Action::SwitchActivity { activity: ActivityReferenceArg::Name(_) }`
//!   — the activity-name carrier is what the picker returned, so a
//!   `Name(_)` reference is the only one we can construct here. The
//!   compositor's `"activity not found"` wire-error for an unresolvable
//!   reference is treated as a contract violation by this helper (the
//!   user-supplied name just came back from the saved-set picker), see
//!   below for the `None` activity-name routing.
//! - `Action::OpenOverview {}` — unit-shape (no fields). Idempotent when
//!   the Overview is already open. `Action::ToggleOverview` and
//!   `Action::CloseOverview` are intentionally NOT used: toggling on an
//!   already-open Overview would close it, the wrong UX for "show me
//!   where the move landed."
//!
//! ## Reply handling
//!
//! Each action is routed through
//! [`send_expect_handled`]`(client, req, None)`. The `None` reflects two
//! related facts:
//! - `FocusWorkspace`, `FocusWindow`, and `OpenOverview` don't take an
//!   activity-name argument, so any wire `"activity not found"` reply
//!   would be a compositor contract violation.
//! - `SwitchActivity` *does* take an activity-name argument, but the
//!   name we pass came back from a picker that listed only activities
//!   the workspace was just saved into — so `"activity not found"` here
//!   is ALSO a contract violation, not a user-input miss. `None` routes
//!   the error to `MalformedResponse(Server(_))` (exit 65) rather than
//!   `ActivityNotFound` (exit 66).
//!
//! Each call attaches a `.context(...)` layer naming the step
//! (`"switching to selected activity"`, `"focusing target workspace"`,
//! `"focusing moved window"`, `"opening overview after follow"`) so a
//! `{:#}`-printed `anyhow::Error` discloses which step failed.
//!
//! ## Synthetic-string discipline
//!
//! These helpers do not construct any CLI-internal synthetic
//! `MalformedResponse(Server(_))` strings — every server-error carrier
//! that reaches stderr from this module comes from the compositor's own
//! wire payload via [`send_expect_handled`].
//!
//! The `« Stay »` follow-picker sentinel is a separate concern handled
//! by [`stay_sentinel`]: that helper picks between two CLI-internal
//! literals based on a collision check against the picker's row set.
//! The sentinel itself is never sent on the wire — it is only ever
//! consumed by callers comparing against [`PickerOutcome::Selected`].

use anyhow::{Context, Result};
use niri_ipc::{Action, ActivityReferenceArg, Request, WorkspaceReferenceArg};

use crate::ipc::NiriClient;
use crate::ipc_helpers::send_expect_handled;

/// Preferred unicode form of the follow-picker "do not follow" sentinel
/// row. Used unless [`stay_sentinel`] detects a collision against the
/// picker's row set.
pub(crate) const STAY_UNICODE: &str = "« Stay »";

/// Underscore-fallback form of the "do not follow" sentinel, substituted
/// by [`stay_sentinel`] iff any picker row would otherwise collide with
/// [`STAY_UNICODE`].
pub(crate) const STAY_FALLBACK: &str = "__jiji_activities_stay__";

/// Picks the appropriate stay-sentinel literal for a given set of picker
/// rows. Returns [`STAY_UNICODE`] unless one of the rows literally equals
/// the unicode form, in which case [`STAY_FALLBACK`] is returned instead.
///
/// **Synthetic-string discipline.** Both [`STAY_UNICODE`] and
/// [`STAY_FALLBACK`] are CLI-internal literals — neither is emitted on
/// the wire by the niri compositor. The sentinel is resolved purely
/// against the picker's stdout in the caller's `PickerOutcome::Selected`
/// match arm; a future grep auditing compositor wire-string matches must
/// skip both literals. Same pattern as the stage-1 / stage-2 sentinels in
/// [`crate::move_window`].
pub(crate) fn stay_sentinel(rows: &[&str]) -> &'static str {
    if rows.contains(&STAY_UNICODE) {
        STAY_FALLBACK
    } else {
        STAY_UNICODE
    }
}

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
pub(crate) fn dispatch_follow_workspace(
    client: &mut dyn NiriClient,
    workspace_id: u64,
    with_overview: bool,
) -> Result<()> {
    let focus_req = Request::Action(Action::FocusWorkspace {
        reference: WorkspaceReferenceArg::Id(workspace_id),
        activity: None,
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
pub(crate) fn dispatch_follow_workspace_and_window(
    client: &mut dyn NiriClient,
    workspace_id: u64,
    window_id: u64,
    with_overview: bool,
) -> Result<()> {
    let focus_ws_req = Request::Action(Action::FocusWorkspace {
        reference: WorkspaceReferenceArg::Id(workspace_id),
        activity: None,
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

/// Switches to the named activity, focuses the destination workspace,
/// and optionally opens the Overview.
///
/// **Contract:**
/// - `with_overview: false` → two IPC round-trips in strict order:
///   `Action::SwitchActivity { activity: Name(activity_name) }` then
///   `Action::FocusWorkspace { reference: Id(workspace_id) }`.
/// - `with_overview: true` → three round-trips in strict order:
///   `SwitchActivity` then `FocusWorkspace` then `OpenOverview {}`.
/// - Ordering rationale: when a workspace belongs to multiple activities
///   the compositor's deterministic disambiguation may not match the
///   activity the user picked at the follow stage; switching activities
///   first scopes the subsequent `FocusWorkspace` to the user's chosen
///   activity context.
/// - Every dispatched action expects `Response::Handled`; any other
///   variant surfaces as `MalformedResponse(WrongVariant)` (exit 65)
///   via [`send_expect_handled`].
/// - The `SwitchActivity` step passes `None` for the helper's
///   `activity_name` parameter so an `"activity not found"` wire reply
///   routes to `MalformedResponse(Server)` (exit 65), NOT
///   `ActivityNotFound` (exit 66). The user just saved the workspace
///   into the activity set picked from; a "not found" reply here is a
///   compositor contract violation, not a user-input miss. See module
///   docs for the broader `None` routing rationale.
/// - Errors are wrapped with per-step `.context(...)` layers
///   (`"switching to selected activity"`, `"focusing target workspace"`,
///   `"opening overview after follow"`) so the `{:#}`-formatted chain
///   discloses which step failed.
///
/// **Synthetic-string discipline.** No CLI-internal `Server(_)` carrier
/// is constructed here: every server-error string surfaced from this
/// helper comes verbatim from the compositor wire payload via the `None`
/// routing in [`send_expect_handled`].
pub(crate) fn dispatch_follow_activity_and_workspace(
    client: &mut dyn NiriClient,
    activity_name: &str,
    workspace_id: u64,
    with_overview: bool,
) -> Result<()> {
    let switch_req = Request::Action(Action::SwitchActivity {
        activity: ActivityReferenceArg::Name(activity_name.to_owned()),
    });
    send_expect_handled(client, switch_req, None).context("switching to selected activity")?;

    let focus_ws_req = Request::Action(Action::FocusWorkspace {
        reference: WorkspaceReferenceArg::Id(workspace_id),
        activity: None,
    });
    send_expect_handled(client, focus_ws_req, None).context("focusing target workspace")?;

    if with_overview {
        let overview_req = Request::Action(Action::OpenOverview {});
        send_expect_handled(client, overview_req, None).context("opening overview after follow")?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use niri_ipc::{Action, ActivityReferenceArg, Reply, Request, Response, WorkspaceReferenceArg};

    use super::*;
    use crate::error::{CliError, MalformedResponseSource};
    use crate::ipc::MockClient;

    fn focus_workspace_req(ws_id: u64) -> Request {
        Request::Action(Action::FocusWorkspace {
            reference: WorkspaceReferenceArg::Id(ws_id),
            activity: None,
        })
    }

    fn focus_window_req(id: u64) -> Request {
        Request::Action(Action::FocusWindow { id })
    }

    fn open_overview_req() -> Request {
        Request::Action(Action::OpenOverview {})
    }

    fn switch_activity_req(name: &str) -> Request {
        Request::Action(Action::SwitchActivity {
            activity: ActivityReferenceArg::Name(name.to_owned()),
        })
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

    /// `with_overview: false`: `SwitchActivity { Name(_) }` then
    /// `FocusWorkspace { Id(_) }`, in that strict order. Pins the
    /// activity-then-workspace ordering rationale for the multi-activity
    /// workspace disambiguation contract.
    #[test]
    fn dispatch_follow_activity_and_workspace_no_overview_emits_switch_then_focus() {
        let mut client = MockClient::new();
        client.expect(switch_activity_req("Work"), Reply::Ok(Response::Handled));
        client.expect(focus_workspace_req(7), Reply::Ok(Response::Handled));
        dispatch_follow_activity_and_workspace(&mut client, "Work", 7, false).expect("happy path");
        client.assert_consumed_in_order();
    }

    /// `with_overview: true`: all three actions in strict order —
    /// `SwitchActivity` → `FocusWorkspace` → `OpenOverview`. Canonical
    /// "switch + follow + overview" shape for assign-workspace --follow.
    #[test]
    fn dispatch_follow_activity_and_workspace_with_overview_emits_all_three_in_order() {
        let mut client = MockClient::new();
        client.expect(switch_activity_req("Work"), Reply::Ok(Response::Handled));
        client.expect(focus_workspace_req(7), Reply::Ok(Response::Handled));
        client.expect(open_overview_req(), Reply::Ok(Response::Handled));
        dispatch_follow_activity_and_workspace(&mut client, "Work", 7, true).expect("happy path");
        client.assert_consumed_in_order();
    }

    /// When `SwitchActivity` returns the wire error `"activity not found"`,
    /// the helper must surface it as `MalformedResponse(Server(_))` (exit
    /// 65), NOT as `CliError::ActivityNotFound` (exit 66). The user just
    /// picked this activity from a list of activities the workspace was
    /// saved into; an unresolvable reference here is a contract violation,
    /// not user input. Pins the `None` activity-name argument discipline
    /// at the `send_expect_handled` call site.
    #[test]
    fn dispatch_follow_activity_and_workspace_switch_activity_wire_error_routes_to_server() {
        let mut client = MockClient::new();
        client.expect(
            switch_activity_req("Work"),
            Err("activity not found".to_owned()),
        );
        let err = dispatch_follow_activity_and_workspace(&mut client, "Work", 7, false)
            .expect_err("activity-not-found must surface as error");
        let cli_err = err
            .chain()
            .find_map(|e| e.downcast_ref::<CliError>())
            .expect("CliError must be in chain");
        match cli_err {
            CliError::MalformedResponse(MalformedResponseSource::Server(msg)) => {
                assert_eq!(msg, "activity not found");
            }
            other => {
                panic!("expected MalformedResponse(Server), NOT ActivityNotFound; got {other:?}",)
            }
        }
        assert_eq!(cli_err.exit_code(), 65);
        client.assert_consumed_in_order();
    }

    /// `stay_sentinel` falls back to `STAY_FALLBACK` when one of the rows
    /// literally equals the unicode form. Mirrors the `sentinel_names`
    /// collision discipline in `src/move_window.rs`: the helper picks
    /// between unicode and underscore-fallback purely against the live
    /// row set, never against a static "what the user might have named
    /// an activity" guess. Sub-cases pin both the colliding and
    /// non-colliding branches.
    #[test]
    fn follow_picker_stay_sentinel_falls_back_when_activity_named_stay_exists() {
        // Default happy path: no collision → unicode form.
        assert_eq!(stay_sentinel(&["Work", "Personal"]), STAY_UNICODE);
        // Collision against the unicode form → fallback.
        assert_eq!(stay_sentinel(&["Work", STAY_UNICODE]), STAY_FALLBACK);
        // Defensive: the underscore-fallback form's presence does NOT
        // drive another collision substitution. Only the unicode form
        // can flip the decision.
        assert_eq!(stay_sentinel(&["Work", STAY_FALLBACK]), STAY_UNICODE);
    }
}
