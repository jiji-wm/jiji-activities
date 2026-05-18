//! `move-workspace` subcommand: move the focused workspace to a named
//! activity.
//!
//! Dispatches
//! `Action::MoveWorkspaceToActivity { workspace: None, activity: Name(name), focus: false }`
//! over IPC and expects `Response::Handled`.
//!
//! ## Why `workspace: None` and `focus: false`
//!
//! - **`workspace: None`** — defaults to the focused workspace. The
//!   user invoked `move-workspace` without naming a workspace; the
//!   only sensible referent is whatever has focus right now. There is
//!   intentionally no flag to pick a specific workspace from the CLI
//!   in v1.
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
//!   `CliError::MalformedResponse(Server(_))` (exit 65). Normally
//!   `workspace: None` defaults to the focused workspace, so this
//!   string is unexpected — if it surfaces, the right diagnostic is
//!   "compositor wire contract violated", not "no such input."
//!   (Edge case: no-monitor sessions have no focused workspace and
//!   may legitimately emit this.)
//! - `Reply::Err("workspace not in active activity")` falls through
//!   to `MalformedResponse(Server)`. The `workspace: None` payload
//!   usually resolves to the focused workspace in the active activity,
//!   but focus can change between the user invoking the command and
//!   the compositor processing it, so this arm is reachable in
//!   practice. Mapping it uniformly keeps the failure diagnosable.
//! - Other `Reply::Err(msg)` →
//!   `CliError::MalformedResponse(Server(msg))`.
//! - Transport / decode errors flow through `IpcError → CliError`
//!   unchanged.

use anyhow::{Context, Result};
use niri_ipc::{Action, ActivityReferenceArg, Request};

use crate::error::CliError;
use crate::ipc::NiriClient;
use crate::ipc_helpers::{names_focused_first, send_expect_activities, send_expect_handled};
use crate::picker::PickerOutcome;

/// Moves the focused workspace to the activity named `name`.
///
/// **Contract:** issues exactly one `MoveWorkspaceToActivity` IPC
/// request (focused workspace, move-only, no focus-follow) and
/// expects `Response::Handled`. See module docs for the full error
/// matrix.
///
/// The IPC error is wrapped with
/// `.context("moving workspace to activity")` so the operation
/// surfaces in the stderr chain.
pub(crate) fn run(
    client: &mut dyn NiriClient,
    name: &str,
    _follow: bool,
    _overview: bool,
) -> Result<()> {
    let req = Request::Action(Action::MoveWorkspaceToActivity {
        workspace: None,
        activity: ActivityReferenceArg::Name(name.to_owned()),
        focus: false,
    });
    send_expect_handled(client, req, Some(name)).context("moving workspace to activity")
}

/// Opens a single-select picker over the current activity list, then
/// dispatches [`run`] against the chosen name.
///
/// **Contract:**
/// - Issues `Request::Activities` first.
/// - If the activity list is empty, writes a single-line diagnostic to
///   stderr (`niri-activities: no activities configured; nothing to
///   move workspace to`) and returns `Ok(())` — exit 0. The picker is
///   never spawned because an empty menu is worse UX than a no-op.
/// - Otherwise reorders names with
///   [`names_focused_first`] so the currently-focused activity is the
///   default highlight, calls `pick`, and on `Selected(name)` delegates
///   to [`run`] (which issues a second IPC call:
///   `Request::Action(MoveWorkspaceToActivity)`).
/// - On `Cancelled`, returns `Ok(())` — user dismissal is exit 0.
///
/// The `pick` parameter is a closure so unit tests can inject a stub
/// without spawning `fuzzel`; production wiring passes
/// [`crate::picker::pick_one`].
pub(crate) fn run_picker<F>(
    client: &mut dyn NiriClient,
    pick: F,
    _follow: bool,
    _overview: bool,
) -> Result<()>
where
    F: FnOnce(&str, &[String]) -> Result<PickerOutcome, CliError>,
{
    let activities = send_expect_activities(client).context("requesting activities")?;
    if activities.is_empty() {
        eprintln!("niri-activities: no activities configured; nothing to move workspace to");
        return Ok(());
    }
    let names = names_focused_first(&activities);
    match pick("Move workspace to activity:", &names)? {
        PickerOutcome::Cancelled => Ok(()),
        PickerOutcome::Selected(name) => {
            // `_follow` / `_overview` are accepted by `run` but ignored
            // this commit; Task 1 lands surface-only and pins the
            // signature shape before any behavioral consumption.
            run(client, &name, false, false)
        }
    }
}

#[cfg(test)]
mod tests {
    use niri_ipc::{Action, Activity, ActivityReferenceArg, Reply, Request, Response};

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
        run(&mut client, "Work", false, false).expect("move-workspace succeeds on Handled");
        client.assert_consumed_in_order();
    }

    #[test]
    fn move_workspace_unknown_name_maps_to_activity_not_found() {
        let mut client = MockClient::new();
        client.expect(move_req("Work"), Err("activity not found".to_owned()));
        let err = run(&mut client, "Work", false, false).expect_err("unknown activity must fail");
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
        let err = run(&mut client, "Work", false, false).expect_err("must fail");
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
        let err = run(&mut client, "Work", false, false).expect_err("must fail");
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
        let err = run(&mut client, "Work", false, false).expect_err("wrong variant must fail");
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
        let err = run(&mut client, "Work", false, false).expect_err("server error must surface");
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
        let err = run(&mut client, "Work", false, false).expect_err("must fail");
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

        run_picker(&mut client, pick, false, false).expect("happy path succeeds");
        client.assert_consumed_in_order();
    }

    #[test]
    fn run_picker_empty_activities_warns_and_exits_zero() {
        let mut client = MockClient::new();
        client.expect(Request::Activities, Reply::Ok(Response::Activities(vec![])));

        let pick = |_prompt: &str, _items: &[String]| -> Result<PickerOutcome, CliError> {
            panic!("pick must not be called when activity list is empty");
        };

        run_picker(&mut client, pick, false, false).expect("empty list exits Ok");
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

        run_picker(&mut client, pick, false, false).expect("cancellation is silent Ok");
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

        let err = run_picker(&mut client, pick, false, false).expect_err("wrong variant must fail");
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
}
