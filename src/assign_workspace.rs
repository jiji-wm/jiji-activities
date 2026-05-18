//! `assign-workspace` subcommand: edit the activity-membership set of
//! the focused workspace via a rofi-backed multi-select picker.
//!
//! Three IPC round-trips in the happy path:
//! 1. `Request::Activities` → name list for the picker.
//! 2. `Request::Workspaces` → locate the focused workspace and compute
//!    its current activity membership for pre-marking.
//! 3. `Request::Action(Action::SetWorkspaceActivities { workspace, activities })`
//!    → dispatch the selection.
//!
//! The chained-single leg (`« Only one… »` sentinel) adds a fourth
//! step — the chained fuzzel picker — between (2) and (3).
//!
//! ## Why the workspace arg is `Id(ws.id)`, not `None`
//!
//! `Action::SetWorkspaceActivities { workspace: None, ... }` defaults
//! to the currently-focused workspace. That looks ergonomic, but focus
//! can drift between the picker opening and the user confirming
//! (Alt-Tab, accidental click, urgent-window pop, ...). Pinning the
//! id we read at picker-open time means the user-visible "workspace I
//! was assigning" is what actually gets the new membership, even if
//! focus moves before they hit Enter.

use std::collections::HashSet;

use anyhow::{Context, Result};
use niri_ipc::{
    Action, Activity, ActivityReferenceArg, Request, Response, Workspace, WorkspaceReferenceArg,
};

use crate::error::{CliError, MalformedResponseSource};
use crate::ipc::{IpcError, NiriClient};
use crate::ipc_helpers::{send_expect_activities, send_expect_workspaces, variant_name};
use crate::picker::PickerOutcome;
use crate::picker::multi_select::{self, MultiPickerOutcome};

/// Drives the `assign-workspace` subcommand end-to-end.
///
/// Issues 2–3 IPC round-trips depending on user choice. The picker
/// (rofi for multi-select, optionally fuzzel for the chained
/// single-select) is invoked between IPC calls.
///
/// **Returns `Ok(())` when:**
/// - `SetWorkspaceActivities` is dispatched and the compositor replies
///   `Response::Handled`.
/// - The user cancels at either picker stage.
/// - The activity list is empty (no picker spawn; eprintln diagnostic).
///
/// **Returns `Err` when:**
/// - No workspace has `is_focused: true` →
///   `CliError::MalformedResponse(Server("no focused workspace"))` (exit 65).
///   The `"no focused workspace"` string is a **CLI-synthetic** marker, not
///   a compositor wire emission — see [`focused_workspace`].
/// - An IPC reply's `Response` variant didn't match the request shape →
///   `CliError::MalformedResponse(WrongVariant { .. })` (exit 65).
/// - Other IPC failures flow through the existing `IpcError → CliError`
///   mapping (`SocketUnavailable`, `MalformedResponse(Server)`, etc.).
///
/// **Snapshot freshness.** The activities snapshot read here is
/// point-in-time at picker open: a concurrent `niri-activities create`
/// while the picker is open does not refresh the menu, and a stale
/// snapshot at save surfaces the compositor's wire error through the
/// `IpcError → CliError` mapping (typically
/// `MalformedResponse(Server)` when the chosen name no longer
/// resolves). Reactive refresh is deferred to v2.
pub(crate) fn run(client: &mut dyn NiriClient, _follow: bool, _overview: bool) -> Result<()> {
    let activities = send_expect_activities(client).context("requesting activities")?;
    if activities.is_empty() {
        eprintln!("niri-activities: no activities configured; nothing to assign");
        return Ok(());
    }
    let workspaces = send_expect_workspaces(client).context("requesting workspaces")?;
    let ws = focused_workspace(&workspaces).context("locating focused workspace")?;
    let activity_names: Vec<String> = activities.iter().map(|a| a.name.clone()).collect();
    let current = workspace_activity_names(ws, &activities);

    match multi_select::pick_many(&activity_names, &current)
        .context("running assign-workspace picker")?
    {
        MultiPickerOutcome::Cancelled => Ok(()),
        MultiPickerOutcome::Selected(names) => dispatch_set(client, ws.id, names),
        MultiPickerOutcome::ChainSingle => {
            match multi_select::pick_one_chained(&activity_names)
                .context("running assign-workspace chained picker")?
            {
                PickerOutcome::Cancelled => Ok(()),
                PickerOutcome::Selected(name) => dispatch_set(client, ws.id, vec![name]),
            }
        }
    }
}

/// Returns the workspace whose `is_focused` flag is `true`, or
/// `MalformedResponse(Server("no focused workspace"))` if no such
/// workspace exists.
///
/// **Synthetic-string discipline.** The literal `"no focused workspace"`
/// embedded in the `Server` payload here is a **CLI-internal** value —
/// it is **not** emitted on the wire by the niri compositor. A future
/// grep that audits compositor wire-string matches must skip this
/// site. The string was chosen so a stderr-reading user sees a
/// human-readable diagnostic via the existing
/// `IpcError::Server → MalformedResponseSource::Server` `Display` path
/// (`malformed compositor response: server error: no focused
/// workspace`).
fn focused_workspace(workspaces: &[Workspace]) -> Result<&Workspace, CliError> {
    workspaces.iter().find(|w| w.is_focused).ok_or_else(|| {
        CliError::MalformedResponse(MalformedResponseSource::Server(
            "no focused workspace".to_owned(),
        ))
    })
}

/// Builds the set of activity *names* the workspace currently belongs
/// to by intersecting `ws.activities` (ids) against the `activities`
/// snapshot.
///
/// Ids that don't resolve in the snapshot are silently dropped. This
/// can happen between the `Activities` and `Workspaces` IPC calls if
/// an activity is removed between them — the right behaviour is to
/// render the workspace as having no membership in that stale id,
/// which the user can then re-affirm by ticking what's actually
/// available.
fn workspace_activity_names(ws: &Workspace, activities: &[Activity]) -> HashSet<String> {
    let by_id: std::collections::HashMap<u64, &str> =
        activities.iter().map(|a| (a.id, a.name.as_str())).collect();
    ws.activities
        .iter()
        .filter_map(|id| by_id.get(id).map(|name| (*name).to_owned()))
        .collect()
}

/// Dispatches the `SetWorkspaceActivities` action against a pinned
/// workspace id. `names` must be non-empty — empty selections are
/// short-circuited to cancellation upstream in
/// [`crate::picker::multi_select::resolve_outcome`].
///
/// Wraps the IPC call with `.context("assigning workspace activities")`
/// so the operation surfaces in the stderr chain.
fn dispatch_set(client: &mut dyn NiriClient, ws_id: u64, names: Vec<String>) -> Result<()> {
    let req = Request::Action(Action::SetWorkspaceActivities {
        workspace: Some(WorkspaceReferenceArg::Id(ws_id)),
        activities: names.into_iter().map(ActivityReferenceArg::Name).collect(),
    });
    send_expect_handled(client, req).context("assigning workspace activities")
}

/// Sends a request and expects `Response::Handled`. Mismatched
/// variants surface as `MalformedResponse(WrongVariant)`; transport
/// and server errors flow through the existing `IpcError → CliError`
/// mapping unchanged.
///
/// This is a local variant of `ipc_helpers::send_expect_handled`
/// that omits the `activity_name` parameter — `assign-workspace`
/// uses `SetWorkspaceActivities` (not an activity-by-name action),
/// so the `"activity not found"` special-case routing never applies.
/// `None` in the shared helper does the same thing, but keeping this
/// local makes the absence of name-routing explicit at the call site.
fn send_expect_handled(client: &mut dyn NiriClient, req: Request) -> Result<()> {
    match client.send(req) {
        Ok(Response::Handled) => Ok(()),
        Ok(other) => Err(
            CliError::MalformedResponse(MalformedResponseSource::WrongVariant {
                expected: "Response::Handled",
                got: variant_name(&other).into(),
            })
            .into(),
        ),
        Err(IpcError::Server(msg)) => {
            Err(CliError::MalformedResponse(MalformedResponseSource::Server(msg)).into())
        }
        Err(other) => Err(CliError::from(other).into()),
    }
}

#[cfg(test)]
mod tests {
    use niri_ipc::{Action, Reply, Request, Response, Workspace};

    use super::*;
    use crate::ipc::MockClient;

    fn act(id: u64, name: &str) -> Activity {
        Activity {
            id,
            name: name.into(),
            is_active: false,
            is_config_declared: true,
            ..Default::default()
        }
    }

    fn ws(id: u64, focused: bool, activities: Vec<u64>) -> Workspace {
        Workspace {
            id,
            idx: 0,
            name: None,
            output: None,
            is_urgent: false,
            is_active: false,
            is_focused: focused,
            active_window_id: None,
            activities,
            is_sticky: false,
            is_in_active_activity: focused,
        }
    }

    /// Dispatches a `SetWorkspaceActivities` request directly without
    /// going through the picker — exercises `dispatch_set` in
    /// isolation.
    #[test]
    fn assign_dispatches_set_with_workspace_id_and_names() {
        let mut client = MockClient::new();
        let expected = Request::Action(Action::SetWorkspaceActivities {
            workspace: Some(WorkspaceReferenceArg::Id(7)),
            activities: vec![
                ActivityReferenceArg::Name("Work".into()),
                ActivityReferenceArg::Name("Gaming".into()),
            ],
        });
        client.expect(expected, Reply::Ok(Response::Handled));
        dispatch_set(&mut client, 7, vec!["Work".into(), "Gaming".into()])
            .expect("dispatch succeeds");
        client.assert_consumed_in_order();
    }

    /// Wires the full happy-path flow through `run`, with a stub
    /// multi-select picker returning a literal `Selected` outcome.
    /// Verifies all three IPC calls fire in order.
    fn run_with_pickers<MS, OS>(client: &mut dyn NiriClient, multi: MS, one: OS) -> Result<()>
    where
        MS: FnOnce(&[String], &HashSet<String>) -> Result<MultiPickerOutcome, CliError>,
        OS: FnOnce(&[String]) -> Result<PickerOutcome, CliError>,
    {
        let activities = send_expect_activities(client).context("requesting activities")?;
        if activities.is_empty() {
            eprintln!("niri-activities: no activities configured; nothing to assign");
            return Ok(());
        }
        let workspaces = send_expect_workspaces(client).context("requesting workspaces")?;
        let ws = focused_workspace(&workspaces)?;
        let activity_names: Vec<String> = activities.iter().map(|a| a.name.clone()).collect();
        let current = workspace_activity_names(ws, &activities);
        match multi(&activity_names, &current).context("running assign-workspace picker")? {
            MultiPickerOutcome::Cancelled => Ok(()),
            MultiPickerOutcome::Selected(names) => dispatch_set(client, ws.id, names),
            MultiPickerOutcome::ChainSingle => {
                match one(&activity_names).context("running assign-workspace chained picker")? {
                    PickerOutcome::Cancelled => Ok(()),
                    PickerOutcome::Selected(name) => dispatch_set(client, ws.id, vec![name]),
                }
            }
        }
    }

    #[test]
    fn assign_select_all_resolves_to_all_activity_names() {
        // Picker returns Selected(all names) — emulating « Select all »
        // already resolved by the picker module. Run must dispatch
        // SetWorkspaceActivities against the focused workspace's id
        // with the full name list.
        let mut client = MockClient::new();
        client.expect(
            Request::Activities,
            Reply::Ok(Response::Activities(vec![
                act(1, "Work"),
                act(2, "Personal"),
                act(3, "Gaming"),
            ])),
        );
        client.expect(
            Request::Workspaces,
            Reply::Ok(Response::Workspaces(vec![ws(42, true, vec![1])])),
        );
        client.expect(
            Request::Action(Action::SetWorkspaceActivities {
                workspace: Some(WorkspaceReferenceArg::Id(42)),
                activities: vec![
                    ActivityReferenceArg::Name("Work".into()),
                    ActivityReferenceArg::Name("Personal".into()),
                    ActivityReferenceArg::Name("Gaming".into()),
                ],
            }),
            Reply::Ok(Response::Handled),
        );

        let multi = |names: &[String], current: &HashSet<String>| {
            assert_eq!(names, &["Work", "Personal", "Gaming"]);
            assert_eq!(current.len(), 1);
            assert!(current.contains("Work"));
            Ok(MultiPickerOutcome::Selected(vec![
                "Work".into(),
                "Personal".into(),
                "Gaming".into(),
            ]))
        };
        let one = |_: &[String]| -> Result<PickerOutcome, CliError> {
            panic!("chained picker must not be called for Selected outcome");
        };
        run_with_pickers(&mut client, multi, one).expect("happy path succeeds");
        client.assert_consumed_in_order();
    }

    #[test]
    fn assign_only_one_chains_single_select_and_dispatches_one_name() {
        // Multi-select returns ChainSingle → chained single-select
        // picker fires → user picks one name → SetWorkspaceActivities
        // dispatched with a single-element name list.
        let mut client = MockClient::new();
        client.expect(
            Request::Activities,
            Reply::Ok(Response::Activities(vec![
                act(1, "Work"),
                act(2, "Personal"),
            ])),
        );
        client.expect(
            Request::Workspaces,
            Reply::Ok(Response::Workspaces(vec![ws(5, true, vec![1, 2])])),
        );
        client.expect(
            Request::Action(Action::SetWorkspaceActivities {
                workspace: Some(WorkspaceReferenceArg::Id(5)),
                activities: vec![ActivityReferenceArg::Name("Personal".into())],
            }),
            Reply::Ok(Response::Handled),
        );

        let multi = |_: &[String], _: &HashSet<String>| Ok(MultiPickerOutcome::ChainSingle);
        let one = |names: &[String]| {
            assert_eq!(names, &["Work", "Personal"]);
            Ok(PickerOutcome::Selected("Personal".into()))
        };
        run_with_pickers(&mut client, multi, one).expect("chain path succeeds");
        client.assert_consumed_in_order();
    }

    #[test]
    fn assign_only_one_cancellation_dispatches_nothing() {
        // Multi-select returns ChainSingle → user cancels the chained
        // picker → no SetWorkspaceActivities call. Only two IPC calls
        // (Activities + Workspaces) are consumed.
        let mut client = MockClient::new();
        client.expect(
            Request::Activities,
            Reply::Ok(Response::Activities(vec![act(1, "Work")])),
        );
        client.expect(
            Request::Workspaces,
            Reply::Ok(Response::Workspaces(vec![ws(5, true, vec![1])])),
        );
        let multi = |_: &[String], _: &HashSet<String>| Ok(MultiPickerOutcome::ChainSingle);
        let one = |_: &[String]| Ok(PickerOutcome::Cancelled);
        run_with_pickers(&mut client, multi, one).expect("cancellation is silent Ok");
        client.assert_consumed_in_order();
    }

    #[test]
    fn assign_cancellation_dispatches_nothing() {
        // Multi-select returns Cancelled → no chained picker, no
        // SetWorkspaceActivities. Only Activities + Workspaces consumed.
        let mut client = MockClient::new();
        client.expect(
            Request::Activities,
            Reply::Ok(Response::Activities(vec![act(1, "Work")])),
        );
        client.expect(
            Request::Workspaces,
            Reply::Ok(Response::Workspaces(vec![ws(5, true, vec![1])])),
        );
        let multi = |_: &[String], _: &HashSet<String>| Ok(MultiPickerOutcome::Cancelled);
        let one = |_: &[String]| -> Result<PickerOutcome, CliError> {
            panic!("chained picker must not be called on Cancelled");
        };
        run_with_pickers(&mut client, multi, one).expect("cancellation is silent Ok");
        client.assert_consumed_in_order();
    }

    #[test]
    fn assign_no_focused_workspace_routes_to_malformed_response() {
        // The Workspaces reply contains no `is_focused: true` workspace.
        // Run must return MalformedResponse(Server("no focused workspace"))
        // — exit 65 — and must NOT spawn either picker or dispatch a
        // SetWorkspaceActivities call.
        let mut client = MockClient::new();
        client.expect(
            Request::Activities,
            Reply::Ok(Response::Activities(vec![act(1, "Work")])),
        );
        client.expect(
            Request::Workspaces,
            Reply::Ok(Response::Workspaces(vec![ws(5, false, vec![1])])),
        );
        let multi = |_: &[String], _: &HashSet<String>| -> Result<MultiPickerOutcome, CliError> {
            panic!("picker must not be called when no workspace is focused");
        };
        let one = |_: &[String]| -> Result<PickerOutcome, CliError> {
            panic!("chained picker must not be called when no workspace is focused");
        };
        let err = run_with_pickers(&mut client, multi, one)
            .expect_err("no focused workspace must surface as error");
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

    #[test]
    fn assign_empty_activities_warns_and_exits_zero() {
        // Activities reply is empty → eprintln + Ok(()), no Workspaces
        // request, no picker spawn.
        let mut client = MockClient::new();
        client.expect(Request::Activities, Reply::Ok(Response::Activities(vec![])));
        let multi = |_: &[String], _: &HashSet<String>| -> Result<MultiPickerOutcome, CliError> {
            panic!("picker must not be called when activity list is empty");
        };
        let one = |_: &[String]| -> Result<PickerOutcome, CliError> {
            panic!("chained picker must not be called when activity list is empty");
        };
        run_with_pickers(&mut client, multi, one).expect("empty list exits Ok");
        client.assert_consumed_in_order();
    }

    #[test]
    fn assign_wrong_variant_on_activities_response_is_malformed() {
        let mut client = MockClient::new();
        client.expect(
            Request::Activities,
            Reply::Ok(Response::Version("v".into())),
        );
        let multi = |_: &[String], _: &HashSet<String>| -> Result<MultiPickerOutcome, CliError> {
            panic!("picker must not be called on malformed Activities response");
        };
        let one = |_: &[String]| -> Result<PickerOutcome, CliError> { unreachable!() };
        let err =
            run_with_pickers(&mut client, multi, one).expect_err("wrong variant must surface");
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
        client.assert_consumed_in_order();
    }

    #[test]
    fn assign_wrong_variant_on_workspaces_response_is_malformed() {
        let mut client = MockClient::new();
        client.expect(
            Request::Activities,
            Reply::Ok(Response::Activities(vec![act(1, "Work")])),
        );
        client.expect(
            Request::Workspaces,
            Reply::Ok(Response::Version("v".into())),
        );
        let multi = |_: &[String], _: &HashSet<String>| -> Result<MultiPickerOutcome, CliError> {
            panic!("picker must not be called on malformed Workspaces response");
        };
        let one = |_: &[String]| -> Result<PickerOutcome, CliError> { unreachable!() };
        let err =
            run_with_pickers(&mut client, multi, one).expect_err("wrong variant must surface");
        let cli_err = err
            .chain()
            .find_map(|e| e.downcast_ref::<CliError>())
            .expect("CliError must be in chain");
        match cli_err {
            CliError::MalformedResponse(MalformedResponseSource::WrongVariant {
                expected,
                got,
            }) => {
                assert_eq!(*expected, "Response::Workspaces");
                assert_eq!(got, "Response::Version");
            }
            other => panic!("expected WrongVariant, got {other:?}"),
        }
        client.assert_consumed_in_order();
    }

    #[test]
    fn assign_server_err_on_set_surfaces_as_malformed_response_server() {
        // The compositor replies Reply::Err("workspace not found") to the
        // SetWorkspaceActivities request. Must surface as
        // MalformedResponse(Server("workspace not found")) — exit 65.
        let mut client = MockClient::new();
        client.expect(
            Request::Activities,
            Reply::Ok(Response::Activities(vec![act(1, "Work")])),
        );
        client.expect(
            Request::Workspaces,
            Reply::Ok(Response::Workspaces(vec![ws(9, true, vec![1])])),
        );
        client.expect(
            Request::Action(Action::SetWorkspaceActivities {
                workspace: Some(WorkspaceReferenceArg::Id(9)),
                activities: vec![ActivityReferenceArg::Name("Work".into())],
            }),
            Reply::Err("workspace not found".into()),
        );

        let multi = |_: &[String], _: &HashSet<String>| {
            Ok(MultiPickerOutcome::Selected(vec!["Work".into()]))
        };
        let one = |_: &[String]| -> Result<PickerOutcome, CliError> { unreachable!() };
        let err =
            run_with_pickers(&mut client, multi, one).expect_err("server error must propagate");
        let cli_err = err
            .chain()
            .find_map(|e| e.downcast_ref::<CliError>())
            .expect("CliError must be in chain");
        match cli_err {
            CliError::MalformedResponse(MalformedResponseSource::Server(msg)) => {
                assert_eq!(msg, "workspace not found");
            }
            other => panic!("expected MalformedResponse(Server), got {other:?}"),
        }
        assert_eq!(cli_err.exit_code(), 65);
        client.assert_consumed_in_order();
    }
}
