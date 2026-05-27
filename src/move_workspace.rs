//! `move-workspace` subcommand: move the focused workspace to a named
//! activity.
//!
//! Dispatches
//! `Action::MoveWorkspaceToActivity { workspace, activity: Name(name), focus: false }`
//! over IPC and expects `Response::Handled`.
//!
//! ## Why `workspace` and `focus: false`
//!
//! - **`workspace`** — defaults to `None` (the focused workspace). The
//!   `--workspace <id>` flag overrides this with an explicit workspace id,
//!   which the caller (a launcher) may have captured before a picker stole
//!   focus. When the flag is absent the compositor resolves the workspace
//!   from whatever has focus at dispatch time. Validation of an explicit id is
//!   delegated to the compositor (unknown id → `MalformedResponse(Server)`,
//!   exit 65) — unlike `assign-workspace`, which validates the workspace id
//!   client-side against the fetched snapshot.
//! - **`focus: false`** — the verb is "move the workspace to
//!   activity"; focusing the destination activity is a separate user
//!   gesture (and a separate subcommand: `switch <name>`). The two
//!   actions compose at the user's discretion.
//!
//! ## Error model
//!
//! - `Reply::Ok(Response::Handled)` → `Ok(())`.
//! - `Reply::Ok(other)` →
//!   `CliError::MalformedResponse(WrongVariant { .. })` (exit 65).
//! - `Reply::Err("activity not found")` →
//!   `CliError::ActivityNotFound(name)` (exit 66) — the user-supplied
//!   name is in scope here, so the typed carrier names it.
//! - `Reply::Err("workspace not found")` →
//!   `CliError::MalformedResponse(Server(_))` (exit 65).
//!   - Default path (`workspace: None`): the compositor resolves the focused
//!     workspace; this string is unexpected and signals a wire-contract
//!     violation. (Edge case: no-monitor sessions have no focused workspace
//!     and may legitimately emit this.)
//!   - Explicit path (`--workspace <id>`): the supplied id was not found by
//!     the compositor. This is a normal user-error outcome (stale or
//!     non-existent id). Both paths map to `MalformedResponse(Server)` — the
//!     typed `CliError::ActivityNotFound` carrier is for activity-name
//!     mismatches only, not workspace-id mismatches.
//! - `Reply::Err("workspace not in active activity")` falls through to
//!   `MalformedResponse(Server)`.
//!   - Default path (`workspace: None`): the focused workspace is expected to
//!     be in the active activity, but focus can drift between invocation and
//!     dispatch, so this is a reachable timing race.
//!   - Explicit path (`--workspace <id>`): the supplied id names a real
//!     workspace that is not in the target activity — a normal user-error
//!     outcome (id from the wrong activity). Both paths map to
//!     `MalformedResponse(Server)` and keep the failure diagnosable.
//! - Other `Reply::Err(msg)` →
//!   `CliError::MalformedResponse(Server(msg))`.
//! - Transport / decode errors flow through `IpcError → CliError`
//!   unchanged.

use anyhow::{Context, Result};
use niri_ipc::{Action, ActivityReferenceArg, Request};

use crate::error::{CliError, MalformedResponseSource};
use crate::follow::{self, dispatch_follow_workspace};
use crate::ipc::NiriClient;
use crate::ipc_helpers::{
    names_focused_first, send_expect_activities, send_expect_handled, send_expect_workspaces,
};
use crate::picker::PickerOutcome;

/// Moves the focused workspace to the activity named `name`, optionally
/// running a follow picker that lets the user opt into focusing the
/// destination workspace (and revealing it in the Overview).
///
/// **Contract:** the default path (`follow: false`) issues exactly one
/// `MoveWorkspaceToActivity` IPC request (focused workspace, move-only,
/// no focus-follow) and expects `Response::Handled`. See module docs for
/// the full error matrix.
///
/// **`--follow` path.** When `follow: true` and no explicit `workspace` id is
/// supplied, an extra `Request::Workspaces` fires BEFORE the move so the
/// focused workspace's id is captured (the move reassigns activity membership;
/// the workspace id is invariant per the compositor's workspace-as-atom
/// contract). When `workspace` is `Some(id)`, the snapshot round-trip is
/// skipped — the explicit id is used directly as the follow target. After a
/// successful dispatch, a single-select picker is spawned with two rows: a
/// follow-confirmation row and a [`STAY_UNICODE`][stay] sentinel. On
/// confirmation [`dispatch_follow_workspace`] is invoked against the captured
/// id. Cancellation (Escape) and the stay-sentinel are both silent exit-0
/// outcomes.
///
/// **Snapshot-vs-dispatch race.** The captured id is the "workspace I was
/// moving" at snapshot time, not at dispatch time. If focus drifts
/// between the snapshot and the compositor processing the move, the
/// follow target may be the workspace that *was* focused (now moved into
/// the new activity), which is precisely the user's intent for
/// `--follow`. Same precedent as the existing named-arg
/// `--follow` flows in `src/move_window.rs`.
///
/// [stay]: crate::follow::STAY_UNICODE
///
/// The `pick` parameter is a closure so unit tests can inject a stub
/// without spawning `fuzzel`; production wiring passes
/// [`crate::picker::pick_one`]. It is invoked at most once, only when
/// `follow: true` AND the move dispatched successfully.
pub(crate) fn run<F>(
    client: &mut dyn NiriClient,
    name: &str,
    pick: F,
    follow: bool,
    overview: bool,
    workspace: Option<u64>,
) -> Result<()>
where
    F: FnOnce(&str, &[String]) -> Result<PickerOutcome, CliError>,
{
    // Determine the follow target workspace id. When an explicit workspace
    // id is supplied, use it directly as the follow target (no snapshot
    // round-trip needed — the id is already known). When no explicit id
    // is given, capture the focused workspace id BEFORE the move dispatches
    // so a subsequent FocusWorkspace can target the moved workspace by id
    // (id is invariant across MoveWorkspaceToActivity per the workspace-
    // as-atom contract). When `follow` is false, no follow target is needed
    // regardless.
    let focused_id = if follow {
        if let Some(explicit_id) = workspace {
            // Explicit workspace id: skip the snapshot round-trip.
            Some(explicit_id)
        } else {
            let workspaces = send_expect_workspaces(client)
                .context("requesting workspaces for --follow capture")?;
            Some(focused_workspace_id(&workspaces)?)
        }
    } else {
        None
    };

    let req = Request::Action(Action::MoveWorkspaceToActivity {
        workspace: workspace.map(niri_ipc::WorkspaceReferenceArg::Id),
        activity: ActivityReferenceArg::Name(name.to_owned()),
        focus: false,
    });
    send_expect_handled(client, req, Some(name)).context("moving workspace to activity")?;

    if let Some(ws_id) = focused_id {
        run_move_workspace_follow_picker(client, pick, name, ws_id, overview)?;
    }
    Ok(())
}

/// Returns the `id` of the workspace whose `is_focused` flag is `true`.
///
/// **Synthetic-string discipline.** The literal `"no focused workspace"`
/// is a **CLI-internal** value — it is **not** emitted on the wire by
/// the niri compositor. A future grep that audits compositor wire-string
/// matches must skip this site. Same pattern as
/// `crate::assign_workspace::focused_workspace`'s `"no focused workspace"`.
fn focused_workspace_id(workspaces: &[niri_ipc::Workspace]) -> Result<u64, CliError> {
    workspaces
        .iter()
        .find(|w| w.is_focused)
        .map(|w| w.id)
        .ok_or_else(|| {
            CliError::MalformedResponse(MalformedResponseSource::Server(
                "no focused workspace".to_owned(),
            ))
        })
}

/// Spawns the follow picker after a successful `MoveWorkspaceToActivity`
/// dispatch. Two rows: a confirmation row and the `« Stay »` sentinel.
///
/// **Picker contract:**
/// - `PickerOutcome::Cancelled` → `Ok(())` (silent exit 0).
/// - `PickerOutcome::Selected(stay)` → `Ok(())` (silent exit 0). Stay is
///   resolved via [`crate::follow::stay_sentinel`] against the
///   confirmation row to dodge any collision with the activity name.
/// - `PickerOutcome::Selected(confirm)` →
///   `dispatch_follow_workspace(client, ws_id, overview)`.
/// - `PickerOutcome::Selected(other)` →
///   `CliError::MalformedResponse(Server("follow picker returned row not
///   in items: ..."))` (exit 65). Contract-violation routing — NEVER
///   folded into `Cancelled`, which would be a silent-failure anti-pattern.
///
/// **Synthetic-string discipline.** The
/// `"follow picker returned row not in items: …"` literal is a
/// CLI-internal value, not on the wire. Same discipline as
/// [`crate::assign_workspace::focused_workspace`]'s `"no focused
/// workspace"`.
fn run_move_workspace_follow_picker<F>(
    client: &mut dyn NiriClient,
    pick: F,
    activity_name: &str,
    ws_id: u64,
    overview: bool,
) -> Result<()>
where
    F: FnOnce(&str, &[String]) -> Result<PickerOutcome, CliError>,
{
    let confirm_row = format!("Follow workspace to activity '{activity_name}'?");
    let rows = [confirm_row.as_str()];
    let stay = follow::stay_sentinel(&rows);
    let items: Vec<String> = vec![confirm_row.clone(), stay.to_owned()];
    let outcome = pick("Follow?", &items).context("running move-workspace follow picker")?;
    match outcome {
        PickerOutcome::Cancelled => Ok(()),
        PickerOutcome::Selected(row) if row == confirm_row => {
            dispatch_follow_workspace(client, ws_id, overview)
        }
        PickerOutcome::Selected(row) if row == stay => Ok(()),
        PickerOutcome::Selected(row) => Err(CliError::MalformedResponse(
            MalformedResponseSource::Server(format!(
                "follow picker returned row not in items: {row:?}"
            )),
        )
        .into()),
    }
}

/// Opens a single-select picker over the current activity list, then
/// dispatches [`run`] against the chosen name.
///
/// **Contract:**
/// - Issues `Request::Activities` first.
/// - If the activity list is empty, writes a single-line diagnostic to
///   stderr (`jiji-activities: no activities configured; nothing to
///   move workspace to`) and returns `Ok(())` — exit 0. The picker is
///   never spawned because an empty menu is worse UX than a no-op.
/// - Otherwise reorders names with
///   [`names_focused_first`] so the currently-focused activity is the
///   default highlight, calls `pick`, and on `Selected(name)` delegates
///   to [`run`] (which issues a second IPC call:
///   `Request::Action(MoveWorkspaceToActivity)`).
/// - On `Cancelled`, returns `Ok(())` — user dismissal is exit 0.
///
/// The `pick` parameter is `Fn` (not `FnOnce`) because `pick` is invoked
/// twice from this function: once at stage 1 (the activity picker above),
/// then again inside [`run`] via `&pick` for the post-move follow picker.
/// `FnOnce` would be consumed by the first call, leaving nothing to borrow
/// for the inner [`run`] call. `follow` and `overview` are forwarded
/// verbatim to [`run`] so the follow picker fires for the picker-entry path
/// as well. Production wiring passes [`crate::picker::pick_one`].
pub(crate) fn run_picker<F>(
    client: &mut dyn NiriClient,
    pick: F,
    follow: bool,
    overview: bool,
    workspace: Option<u64>,
) -> Result<()>
where
    F: Fn(&str, &[String]) -> Result<PickerOutcome, CliError>,
{
    let activities = send_expect_activities(client).context("requesting activities")?;
    if activities.is_empty() {
        eprintln!("jiji-activities: no activities configured; nothing to move workspace to");
        return Ok(());
    }
    let names = names_focused_first(&activities);
    match pick("Move workspace to activity:", &names)? {
        PickerOutcome::Cancelled => Ok(()),
        PickerOutcome::Selected(name) => {
            // Thread `follow` / `overview` / `workspace` through to `run`
            // so the post-move follow picker fires for the picker entry
            // path too. The `pick` closure is invoked twice from this
            // function: once above for stage 1, then once inside `run`
            // via `&pick` (the post-move follow picker). Hence the
            // `Fn` bound on the outer signature — `FnOnce` would
            // consume `pick` at the stage-1 call site and leave
            // nothing to borrow for the inner `run` call.
            run(client, &name, &pick, follow, overview, workspace)
        }
    }
}

#[cfg(test)]
mod tests {
    use niri_ipc::{
        Action, Activity, ActivityReferenceArg, Reply, Request, Response, Workspace,
        WorkspaceReferenceArg,
    };

    use super::*;
    use crate::error::{CliError, MalformedResponseSource};
    use crate::ipc::MockClient;

    fn move_req(name: &str) -> Request {
        Request::Action(Action::MoveWorkspaceToActivity {
            workspace: None,
            activity: ActivityReferenceArg::Name(name.to_owned()),
            focus: false,
        })
    }

    /// Pick stub that panics when invoked — used by `run` tests where
    /// `--follow` is off, so the follow picker must NOT spawn.
    fn no_follow_pick(_: &str, _: &[String]) -> Result<PickerOutcome, CliError> {
        panic!("follow picker must NOT be invoked when --follow is off");
    }

    fn ws(id: u64, focused: bool, activities: Vec<u64>) -> Workspace {
        Workspace {
            id,
            idx: 0,
            name: None,
            output: Some("DP-1".into()),
            is_urgent: false,
            is_active: false,
            is_focused: focused,
            active_window_id: None,
            activities,
            is_sticky: false,
            is_in_active_activity: focused,
        }
    }

    #[test]
    fn move_workspace_dispatches_action_with_name_arg() {
        // Pins three load-bearing fields:
        //  - workspace: None (focused-by-default)
        //  - activity: Name(_) (not Id)
        //  - focus: false (move-only verb)
        // MockClient's queue-equality enforces all three. A regression
        // that flipped focus or pinned a workspace id would fail here.
        let mut client = MockClient::new();
        client.expect(move_req("Work"), Reply::Ok(Response::Handled));
        run(&mut client, "Work", no_follow_pick, false, false, None)
            .expect("move-workspace succeeds on Handled");
        client.assert_consumed_in_order();
    }

    #[test]
    fn move_workspace_unknown_name_maps_to_activity_not_found() {
        let mut client = MockClient::new();
        client.expect(move_req("Work"), Err("activity not found".to_owned()));
        let err = run(&mut client, "Work", no_follow_pick, false, false, None)
            .expect_err("unknown activity must fail");
        let cli_err = err
            .chain()
            .find_map(|e| e.downcast_ref::<CliError>())
            .expect("CliError must be in chain");
        match cli_err {
            CliError::ActivityNotFound(name) => assert_eq!(name, "Work"),
            other => panic!("expected ActivityNotFound, got {other:?}"),
        }
        assert_eq!(cli_err.exit_code(), 66);
        client.assert_consumed_in_order();
    }

    #[test]
    fn move_workspace_workspace_not_found_routes_to_malformed_not_not_found() {
        // Negative-space: the "workspace not found" wire string MUST
        // route to MalformedResponse(Server) (exit 65), NOT
        // ActivityNotFound (exit 66). The string mentions "workspace,"
        // not "activity" — fabricating an ActivityNotFound from a
        // workspace-miss wire string would be misleading.
        let mut client = MockClient::new();
        client.expect(move_req("Work"), Err("workspace not found".to_owned()));
        let err =
            run(&mut client, "Work", no_follow_pick, false, false, None).expect_err("must fail");
        let cli_err = err
            .chain()
            .find_map(|e| e.downcast_ref::<CliError>())
            .expect("CliError must be in chain");
        match cli_err {
            CliError::MalformedResponse(MalformedResponseSource::Server(msg)) => {
                assert_eq!(msg, "workspace not found");
            }
            other => {
                panic!("expected MalformedResponse(Server), not ActivityNotFound; got {other:?}",)
            }
        }
        assert_eq!(cli_err.exit_code(), 65);
        client.assert_consumed_in_order();
    }

    #[test]
    fn move_workspace_workspace_not_in_active_activity_routes_to_malformed() {
        // Defensive: focus can shift between user invocation and
        // compositor processing, so this string is reachable. Routes to
        // MalformedResponse(Server) — not ActivityNotFound.
        let mut client = MockClient::new();
        client.expect(
            move_req("Work"),
            Err("workspace not in active activity".to_owned()),
        );
        let err =
            run(&mut client, "Work", no_follow_pick, false, false, None).expect_err("must fail");
        let cli_err = err
            .chain()
            .find_map(|e| e.downcast_ref::<CliError>())
            .expect("CliError must be in chain");
        match cli_err {
            CliError::MalformedResponse(MalformedResponseSource::Server(msg)) => {
                assert_eq!(msg, "workspace not in active activity");
            }
            other => panic!("expected MalformedResponse(Server), got {other:?}"),
        }
        assert_eq!(cli_err.exit_code(), 65);
        client.assert_consumed_in_order();
    }

    #[test]
    fn move_workspace_wrong_response_variant_is_malformed() {
        let mut client = MockClient::new();
        client.expect(move_req("Work"), Reply::Ok(Response::Version("v".into())));
        let err = run(&mut client, "Work", no_follow_pick, false, false, None)
            .expect_err("wrong variant must fail");
        let cli_err = err
            .chain()
            .find_map(|e| e.downcast_ref::<CliError>())
            .expect("CliError must be in chain");
        match cli_err {
            CliError::MalformedResponse(MalformedResponseSource::WrongVariant {
                expected,
                got,
            }) => {
                assert_eq!(*expected, "Response::Handled");
                assert_eq!(got, "Response::Version");
            }
            other => panic!("expected WrongVariant, got {other:?}"),
        }
        assert_eq!(cli_err.exit_code(), 65);
        client.assert_consumed_in_order();
    }

    #[test]
    fn move_workspace_other_server_error_routes_to_malformed_response() {
        let mut client = MockClient::new();
        client.expect(
            move_req("Work"),
            Err("activity switch blocked: gesture".to_owned()),
        );
        let err = run(&mut client, "Work", no_follow_pick, false, false, None)
            .expect_err("server error must surface");
        let cli_err = err
            .chain()
            .find_map(|e| e.downcast_ref::<CliError>())
            .expect("CliError must be in chain");
        match cli_err {
            CliError::MalformedResponse(MalformedResponseSource::Server(msg)) => {
                assert_eq!(msg, "activity switch blocked: gesture");
            }
            other => panic!("expected MalformedResponse(Server), got {other:?}"),
        }
        assert_eq!(cli_err.exit_code(), 65);
        client.assert_consumed_in_order();
    }

    #[test]
    fn move_workspace_preserves_context_in_error_chain() {
        let mut client = MockClient::new();
        client.expect(move_req("Work"), Err("activity not found".to_owned()));
        let err =
            run(&mut client, "Work", no_follow_pick, false, false, None).expect_err("must fail");
        let formatted = format!("{err:#}");
        assert!(
            formatted.contains("moving workspace to activity"),
            "context layer missing from chain: {formatted}",
        );
        assert!(
            formatted.contains("no such activity: Work"),
            "ActivityNotFound Display missing from chain: {formatted}",
        );
        client.assert_consumed_in_order();
    }

    // ---- run_picker -----------------------------------------------------

    fn act(id: u64, name: &str, is_active: bool) -> Activity {
        Activity {
            id,
            name: name.into(),
            is_active,
            is_config_declared: true,
            ..Default::default()
        }
    }

    #[test]
    fn run_picker_selects_and_dispatches_move_workspace() {
        // Two IPC calls in strict order: Activities first (for the
        // menu), then MoveWorkspaceToActivity (after picker returns
        // Selected). MockClient is FIFO — wrong order panics on the
        // first mismatched send(). Also pins focused-first ordering
        // inside the pick closure.
        let mut client = MockClient::new();
        client.expect(
            Request::Activities,
            Reply::Ok(Response::Activities(vec![
                act(1, "Work", true),
                act(2, "Personal", false),
            ])),
        );
        client.expect(move_req("Personal"), Reply::Ok(Response::Handled));

        let pick = |prompt: &str, items: &[String]| -> Result<PickerOutcome, CliError> {
            assert_eq!(prompt, "Move workspace to activity:");
            // Focused-first reordering: Work (focused) precedes Personal.
            assert_eq!(items, &["Work".to_owned(), "Personal".to_owned()]);
            Ok(PickerOutcome::Selected("Personal".to_owned()))
        };

        run_picker(&mut client, pick, false, false, None).expect("happy path succeeds");
        client.assert_consumed_in_order();
    }

    #[test]
    fn run_picker_empty_activities_warns_and_exits_zero() {
        let mut client = MockClient::new();
        client.expect(Request::Activities, Reply::Ok(Response::Activities(vec![])));

        let pick = |_prompt: &str, _items: &[String]| -> Result<PickerOutcome, CliError> {
            panic!("pick must not be called when activity list is empty");
        };

        run_picker(&mut client, pick, false, false, None).expect("empty list exits Ok");
        client.assert_consumed_in_order();
    }

    #[test]
    fn run_picker_cancellation_skips_dispatch() {
        // User dismisses the menu → no Move IPC call. Only one
        // queued reply (Activities); if `run_picker` dispatched a
        // Move the MockClient would panic on unexpected request.
        let mut client = MockClient::new();
        client.expect(
            Request::Activities,
            Reply::Ok(Response::Activities(vec![act(1, "Work", true)])),
        );

        let pick = |_prompt: &str, _items: &[String]| -> Result<PickerOutcome, CliError> {
            Ok(PickerOutcome::Cancelled)
        };

        run_picker(&mut client, pick, false, false, None).expect("cancellation is silent Ok");
        client.assert_consumed_in_order();
    }

    #[test]
    fn run_picker_wrong_activities_variant_is_malformed() {
        // If the compositor replies with a non-Activities variant to the
        // first IPC call, `run_picker` must surface WrongVariant (exit
        // 65) before ever reaching the pick closure.
        let mut client = MockClient::new();
        client.expect(
            Request::Activities,
            Reply::Ok(Response::Version("v".into())),
        );

        let pick = |_prompt: &str, _items: &[String]| -> Result<PickerOutcome, CliError> {
            panic!("pick must not be called on a malformed Activities response");
        };

        let err =
            run_picker(&mut client, pick, false, false, None).expect_err("wrong variant must fail");
        let cli_err = err
            .chain()
            .find_map(|e| e.downcast_ref::<CliError>())
            .expect("CliError must be in chain");
        match cli_err {
            CliError::MalformedResponse(MalformedResponseSource::WrongVariant {
                expected,
                got,
            }) => {
                assert_eq!(*expected, "Response::Activities");
                assert_eq!(got, "Response::Version");
            }
            other => panic!("expected WrongVariant, got {other:?}"),
        }
        assert_eq!(cli_err.exit_code(), 65);
        client.assert_consumed_in_order();
    }

    // ---- --follow ---------------------------------------------------------

    fn focus_workspace_req(ws_id: u64) -> Request {
        Request::Action(Action::FocusWorkspace {
            reference: WorkspaceReferenceArg::Id(ws_id),
        })
    }

    /// `--follow` path issues `Request::Workspaces` BEFORE the move so the
    /// focused workspace id is captured. After a successful
    /// `MoveWorkspaceToActivity`, the follow picker spawns; on the
    /// confirmation row a `FocusWorkspace { Id(captured) }` fires.
    /// Pins the strict IPC ordering: Workspaces → Move → FocusWorkspace.
    #[test]
    fn move_workspace_run_follow_captures_workspace_id_via_workspaces_snapshot() {
        let mut client = MockClient::new();
        client.expect(
            Request::Workspaces,
            Reply::Ok(Response::Workspaces(vec![ws(42, true, vec![1])])),
        );
        client.expect(move_req("Personal"), Reply::Ok(Response::Handled));
        client.expect(focus_workspace_req(42), Reply::Ok(Response::Handled));

        let pick = |prompt: &str, items: &[String]| -> Result<PickerOutcome, CliError> {
            assert_eq!(prompt, "Follow?");
            // Items: [confirm_row, « Stay »]. Confirmation row is the
            // first item; sentinel is the second.
            assert_eq!(items.len(), 2);
            assert_eq!(items[1], "« Stay »");
            Ok(PickerOutcome::Selected(items[0].clone()))
        };

        run(&mut client, "Personal", pick, true, false, None).expect("--follow succeeds");
        client.assert_consumed_in_order();
    }

    /// `--follow` with picker returning a row NOT in the items: contract
    /// violation routed to `MalformedResponse(Server)` (exit 65), NOT
    /// folded into `Cancelled`. Mirrors the
    /// `Stage1Resolution::Unknown` / `Stage2Resolution*::Unknown`
    /// discipline.
    #[test]
    fn move_workspace_run_follow_picker_unknown_row_routes_to_malformed_server() {
        let mut client = MockClient::new();
        client.expect(
            Request::Workspaces,
            Reply::Ok(Response::Workspaces(vec![ws(42, true, vec![1])])),
        );
        client.expect(move_req("Personal"), Reply::Ok(Response::Handled));

        let pick = |_: &str, _: &[String]| -> Result<PickerOutcome, CliError> {
            Ok(PickerOutcome::Selected("definitely-not-an-item".to_owned()))
        };
        let err = run(&mut client, "Personal", pick, true, false, None)
            .expect_err("unknown row must route to MalformedResponse");
        let cli_err = err
            .chain()
            .find_map(|e| e.downcast_ref::<CliError>())
            .expect("CliError must be in chain");
        match cli_err {
            CliError::MalformedResponse(MalformedResponseSource::Server(msg)) => {
                assert!(
                    msg.contains("follow picker returned row not in items"),
                    "expected synthetic discipline message, got: {msg}",
                );
            }
            other => panic!("expected MalformedResponse(Server), got {other:?}"),
        }
        assert_eq!(cli_err.exit_code(), 65);
        client.assert_consumed_in_order();
    }

    /// `--follow` with no focused workspace in the snapshot: the
    /// synthetic `"no focused workspace"` error fires BEFORE the move
    /// dispatches. No `MoveWorkspaceToActivity` is sent.
    #[test]
    fn move_workspace_run_follow_no_focused_workspace_exits_65_before_move() {
        let mut client = MockClient::new();
        client.expect(
            Request::Workspaces,
            Reply::Ok(Response::Workspaces(vec![ws(42, false, vec![1])])),
        );
        let err = run(&mut client, "Personal", no_follow_pick, true, false, None)
            .expect_err("no focused workspace must fail with exit 65 before move");
        let cli_err = err
            .chain()
            .find_map(|e| e.downcast_ref::<CliError>())
            .expect("CliError must be in chain");
        match cli_err {
            CliError::MalformedResponse(MalformedResponseSource::Server(msg)) => {
                assert_eq!(msg, "no focused workspace");
            }
            other => panic!("expected MalformedResponse(Server), got {other:?}"),
        }
        assert_eq!(cli_err.exit_code(), 65);
        client.assert_consumed_in_order();
    }

    /// `run_picker → run` delegation threads `follow` / `overview`
    /// through. Pins that the previous hardcoded `false, false` no
    /// longer drops the flags on the floor.
    #[test]
    fn move_workspace_run_picker_threads_follow_flag_to_run() {
        let mut client = MockClient::new();
        client.expect(
            Request::Activities,
            Reply::Ok(Response::Activities(vec![act(1, "Work", true)])),
        );
        // `--follow: true` so `run` issues Workspaces BEFORE Move.
        client.expect(
            Request::Workspaces,
            Reply::Ok(Response::Workspaces(vec![ws(99, true, vec![1])])),
        );
        client.expect(move_req("Work"), Reply::Ok(Response::Handled));
        client.expect(focus_workspace_req(99), Reply::Ok(Response::Handled));

        let pick = |prompt: &str, items: &[String]| -> Result<PickerOutcome, CliError> {
            if prompt == "Move workspace to activity:" {
                Ok(PickerOutcome::Selected("Work".to_owned()))
            } else {
                // Follow picker — confirm.
                assert_eq!(prompt, "Follow?");
                Ok(PickerOutcome::Selected(items[0].clone()))
            }
        };

        run_picker(&mut client, pick, true, false, None).expect("delegation threads --follow");
        client.assert_consumed_in_order();
    }

    /// `run_picker → run` delegation threads `overview: true` through.
    /// Pins that `OpenOverview` fires after `FocusWorkspace` when the
    /// `--overview` flag is set alongside `--follow`.
    #[test]
    fn move_workspace_run_picker_threads_overview_flag_to_run() {
        let mut client = MockClient::new();
        client.expect(
            Request::Activities,
            Reply::Ok(Response::Activities(vec![act(1, "Work", true)])),
        );
        // `--follow: true` so `run` issues Workspaces BEFORE Move.
        client.expect(
            Request::Workspaces,
            Reply::Ok(Response::Workspaces(vec![ws(99, true, vec![1])])),
        );
        client.expect(move_req("Work"), Reply::Ok(Response::Handled));
        client.expect(focus_workspace_req(99), Reply::Ok(Response::Handled));
        client.expect(
            Request::Action(Action::OpenOverview {}),
            Reply::Ok(Response::Handled),
        );

        let pick = |prompt: &str, items: &[String]| -> Result<PickerOutcome, CliError> {
            if prompt == "Move workspace to activity:" {
                Ok(PickerOutcome::Selected("Work".to_owned()))
            } else {
                // Follow picker — confirm.
                assert_eq!(prompt, "Follow?");
                Ok(PickerOutcome::Selected(items[0].clone()))
            }
        };

        run_picker(&mut client, pick, true, true, None).expect("delegation threads --overview");
        client.assert_consumed_in_order();
    }

    // ---- --workspace explicit-target tests ----------------------------------

    /// `--workspace 42` passed to `move-workspace` named-arg form: the
    /// `MoveWorkspaceToActivity` request must carry
    /// `workspace: Some(WorkspaceReferenceArg::Id(42))` — not `None`.
    #[test]
    fn move_workspace_explicit_workspace_threads_into_request_payload() {
        let mut client = MockClient::new();
        // Explicit workspace: no Workspaces snapshot capture needed before
        // the move (follow is false here).
        client.expect(
            Request::Action(Action::MoveWorkspaceToActivity {
                workspace: Some(WorkspaceReferenceArg::Id(42)),
                activity: ActivityReferenceArg::Name("Personal".to_owned()),
                focus: false,
            }),
            Reply::Ok(Response::Handled),
        );
        run(
            &mut client,
            "Personal",
            no_follow_pick,
            false,
            false,
            Some(42),
        )
        .expect("explicit --workspace succeeds");
        client.assert_consumed_in_order();
    }

    /// `--workspace 42` with `--follow true`: the `Request::Workspaces`
    /// capture round-trip is skipped (the explicit id is used directly
    /// as the follow target). The MockClient queue has no `Request::Workspaces`
    /// before the move; only the move and follow-confirm dispatch appear.
    #[test]
    fn move_workspace_explicit_workspace_with_follow_skips_capture_roundtrip() {
        let mut client = MockClient::new();
        // No Request::Workspaces queued — the test asserts that absence by
        // virtue of assert_consumed_in_order succeeding without it.
        client.expect(
            Request::Action(Action::MoveWorkspaceToActivity {
                workspace: Some(WorkspaceReferenceArg::Id(42)),
                activity: ActivityReferenceArg::Name("Personal".to_owned()),
                focus: false,
            }),
            Reply::Ok(Response::Handled),
        );
        // Follow picker confirms → FocusWorkspace fires against the explicit id.
        client.expect(focus_workspace_req(42), Reply::Ok(Response::Handled));

        let pick = |prompt: &str, items: &[String]| -> Result<PickerOutcome, CliError> {
            assert_eq!(prompt, "Follow?");
            Ok(PickerOutcome::Selected(items[0].clone()))
        };
        run(&mut client, "Personal", pick, true, false, Some(42))
            .expect("--workspace with --follow skips snapshot round-trip");
        client.assert_consumed_in_order();
    }
}
