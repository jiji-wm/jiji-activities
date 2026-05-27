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
//! **`--follow` path.** When `follow: true`, after a successful dispatch
//! the follow picker is spawned. On confirmation,
//! [`dispatch_follow_activity_and_workspace`] issues up to three more IPC
//! round-trips in strict order: `SwitchActivity`, `FocusWorkspace`, and
//! optionally `OpenOverview` (when `overview: true`). See
//! [`dispatch_follow_activity_and_workspace`] for the full contract.
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
use crate::follow::{self, dispatch_follow_activity_and_workspace};
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
/// When `workspace` is `Some(id)`, the workspace with that id is looked up in
/// the fetched Workspaces snapshot and used as the assignment target instead
/// of the focused workspace. The `Request::Workspaces` fetch always occurs
/// because the handler needs the workspace's current activity membership to
/// seed the multi-select picker's pre-checked rows.
///
/// **Returns `Ok(())` when:**
/// - `SetWorkspaceActivities` is dispatched and the compositor replies
///   `Response::Handled`.
/// - The user cancels at either picker stage.
/// - The activity list is empty (no picker spawn; eprintln diagnostic).
///
/// **Returns `Err` when:**
/// - `workspace` is `Some(id)` and no workspace with that id exists in the
///   snapshot →
///   `CliError::MalformedResponse(Server("workspace id <N> not found in snapshot"))` (exit 65).
///   The `"workspace id <N> not found in snapshot"` string is a
///   **CLI-synthetic** marker, not a compositor wire emission — see the
///   snapshot-miss construction site below.
/// - `workspace` is `None` and no workspace has `is_focused: true` →
///   `CliError::MalformedResponse(Server("no focused workspace"))` (exit 65).
///   The `"no focused workspace"` string is a **CLI-synthetic** marker, not
///   a compositor wire emission — see [`focused_workspace`].
/// - An IPC reply's `Response` variant didn't match the request shape →
///   `CliError::MalformedResponse(WrongVariant { .. })` (exit 65).
/// - Other IPC failures flow through the existing `IpcError → CliError`
///   mapping (`SocketUnavailable`, `MalformedResponse(Server)`, etc.).
///
/// **`--follow`.** When `follow: true`, after a successful dispatch a
/// follow picker is spawned over the saved activity names plus a `« Stay »`
/// sentinel. On confirmation, [`dispatch_follow_activity_and_workspace`] is
/// invoked against the **assignment target** workspace id (the explicit id
/// when `--workspace` is supplied, or the focused workspace id otherwise).
/// When `--workspace <id>` and `--follow` are combined, the follow
/// `FocusWorkspace` targets the explicit workspace, not the focused one.
/// If the picker returns an unrecognised row →
/// `MalformedResponse(Server("follow picker returned row not in items: …"))`
/// (exit 65). `overview: true` causes an additional `OpenOverview` IPC call
/// after `FocusWorkspace` on the follow path.
///
/// **Snapshot freshness.** The activities snapshot read here is
/// point-in-time at picker open: a concurrent `jiji-activities create`
/// while the picker is open does not refresh the menu, and a stale
/// snapshot at save surfaces the compositor's wire error through the
/// `IpcError → CliError` mapping (typically
/// `MalformedResponse(Server)` when the chosen name no longer
/// resolves). Reactive refresh is deferred to v2.
pub(crate) fn run<F>(
    client: &mut dyn NiriClient,
    pick: F,
    follow: bool,
    overview: bool,
    workspace: Option<u64>,
) -> Result<()>
where
    F: FnOnce(&str, &[String]) -> Result<PickerOutcome, CliError>,
{
    let activities = send_expect_activities(client).context("requesting activities")?;
    if activities.is_empty() {
        eprintln!("jiji-activities: no activities configured; nothing to assign");
        return Ok(());
    }
    let workspaces = send_expect_workspaces(client).context("requesting workspaces")?;
    // Select the target workspace: explicit id takes priority over focus.
    //
    // **Synthetic-string discipline.** The `"workspace id <N> not found in
    // snapshot"` literal constructed below is a **CLI-internal** value — it
    // is **not** emitted on the wire by the niri compositor. A future grep
    // that audits compositor wire-string matches must skip this site.
    let ws = match workspace {
        Some(id) => workspaces
            .iter()
            .find(|w| w.id == id)
            .ok_or_else(|| {
                CliError::MalformedResponse(MalformedResponseSource::Server(format!(
                    "workspace id {id} not found in snapshot"
                )))
            })
            .context("locating explicit workspace in snapshot")?,
        None => focused_workspace(&workspaces).context("locating focused workspace")?,
    };
    let ws_id = ws.id;
    let activity_names: Vec<String> = activities.iter().map(|a| a.name.clone()).collect();
    let current = workspace_activity_names(ws, &activities);

    let saved: Option<Vec<String>> = match multi_select::pick_many(&activity_names, &current)
        .context("running assign-workspace picker")?
    {
        MultiPickerOutcome::Cancelled => None,
        MultiPickerOutcome::Selected(names) => {
            dispatch_set(client, ws_id, names.clone())?;
            Some(names)
        }
        MultiPickerOutcome::ChainSingle => {
            match multi_select::pick_one_chained(&activity_names)
                .context("running assign-workspace chained picker")?
            {
                PickerOutcome::Cancelled => None,
                PickerOutcome::Selected(name) => {
                    dispatch_set(client, ws_id, vec![name.clone()])?;
                    Some(vec![name])
                }
            }
        }
    };

    if follow
        && let Some(saved_names) = saved
        && !saved_names.is_empty()
    {
        run_assign_workspace_follow_picker(client, pick, &saved_names, ws_id, overview)?;
    }
    Ok(())
}

/// Spawns the follow picker after a successful `SetWorkspaceActivities`
/// dispatch. Items: one row per saved activity name plus the
/// `« Stay »` sentinel.
///
/// **Picker contract:**
/// - `PickerOutcome::Cancelled` → `Ok(())` (silent exit 0).
/// - `PickerOutcome::Selected(stay)` → `Ok(())` (silent exit 0). Stay is
///   resolved via [`crate::follow::stay_sentinel`] against the saved
///   activity-name row set, so an activity literally named `« Stay »`
///   does not collide with the sentinel.
/// - `PickerOutcome::Selected(name)` where `name` matches one of the
///   saved activity names →
///   `dispatch_follow_activity_and_workspace(client, &name, ws_id, overview)`.
///   That helper prepends `SwitchActivity { Name(name) }` before the
///   `FocusWorkspace`, scoping the focus to the user's chosen activity
///   context when the workspace is a member of more than one activity.
/// - `PickerOutcome::Selected(unknown)` →
///   `CliError::MalformedResponse(Server("follow picker returned row not
///   in items: ..."))` (exit 65). Contract-violation routing — NEVER
///   folded into `Cancelled`, which would be a silent-failure anti-pattern.
///
/// **Synthetic-string discipline.** The
/// `"follow picker returned row not in items: …"` literal is a
/// CLI-internal value, not on the wire. Same discipline as
/// [`focused_workspace`]'s `"no focused workspace"`.
fn run_assign_workspace_follow_picker<F>(
    client: &mut dyn NiriClient,
    pick: F,
    saved_names: &[String],
    ws_id: u64,
    overview: bool,
) -> Result<()>
where
    F: FnOnce(&str, &[String]) -> Result<PickerOutcome, CliError>,
{
    let row_refs: Vec<&str> = saved_names.iter().map(String::as_str).collect();
    let stay = follow::stay_sentinel(&row_refs);
    let mut items: Vec<String> = saved_names.to_vec();
    items.push(stay.to_owned());
    let outcome = pick("Follow?", &items).context("running assign-workspace follow picker")?;
    match outcome {
        PickerOutcome::Cancelled => Ok(()),
        PickerOutcome::Selected(row) if row == stay => Ok(()),
        PickerOutcome::Selected(row) if saved_names.iter().any(|n| n == &row) => {
            dispatch_follow_activity_and_workspace(client, &row, ws_id, overview)
        }
        PickerOutcome::Selected(row) => Err(CliError::MalformedResponse(
            MalformedResponseSource::Server(format!(
                "follow picker returned row not in items: {row:?}"
            )),
        )
        .into()),
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

    /// Wires the full happy-path flow through `run`, with stub
    /// multi-select / chained-single pickers. Mirrors the production
    /// `run` shape including the post-save `--follow` picker so the
    /// follow path is unit-testable without spawning fuzzel.
    fn run_with_pickers<MS, OS, FP>(
        client: &mut dyn NiriClient,
        multi: MS,
        one: OS,
        follow_pick: FP,
        follow: bool,
        overview: bool,
        workspace: Option<u64>,
    ) -> Result<()>
    where
        MS: FnOnce(&[String], &HashSet<String>) -> Result<MultiPickerOutcome, CliError>,
        OS: FnOnce(&[String]) -> Result<PickerOutcome, CliError>,
        FP: FnOnce(&str, &[String]) -> Result<PickerOutcome, CliError>,
    {
        let activities = send_expect_activities(client).context("requesting activities")?;
        if activities.is_empty() {
            eprintln!("jiji-activities: no activities configured; nothing to assign");
            return Ok(());
        }
        let workspaces = send_expect_workspaces(client).context("requesting workspaces")?;
        let ws = match workspace {
            Some(id) => workspaces
                .iter()
                .find(|w| w.id == id)
                .ok_or_else(|| {
                    CliError::MalformedResponse(MalformedResponseSource::Server(format!(
                        "workspace id {id} not found in snapshot"
                    )))
                })
                .context("locating explicit workspace in snapshot")?,
            None => focused_workspace(&workspaces).context("locating focused workspace")?,
        };
        let ws_id = ws.id;
        let activity_names: Vec<String> = activities.iter().map(|a| a.name.clone()).collect();
        let current = workspace_activity_names(ws, &activities);
        let saved: Option<Vec<String>> =
            match multi(&activity_names, &current).context("running assign-workspace picker")? {
                MultiPickerOutcome::Cancelled => None,
                MultiPickerOutcome::Selected(names) => {
                    dispatch_set(client, ws_id, names.clone())?;
                    Some(names)
                }
                MultiPickerOutcome::ChainSingle => {
                    match one(&activity_names).context("running assign-workspace chained picker")? {
                        PickerOutcome::Cancelled => None,
                        PickerOutcome::Selected(name) => {
                            dispatch_set(client, ws_id, vec![name.clone()])?;
                            Some(vec![name])
                        }
                    }
                }
            };
        if follow
            && let Some(saved_names) = saved
            && !saved_names.is_empty()
        {
            run_assign_workspace_follow_picker(client, follow_pick, &saved_names, ws_id, overview)?;
        }
        Ok(())
    }

    /// `follow_pick` stub that panics — for tests where `--follow` is
    /// off, so the follow picker must NOT spawn.
    fn no_follow_pick(_: &str, _: &[String]) -> Result<PickerOutcome, CliError> {
        panic!("follow picker must NOT be invoked when --follow is off");
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
        run_with_pickers(&mut client, multi, one, no_follow_pick, false, false, None)
            .expect("happy path succeeds");
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
        run_with_pickers(&mut client, multi, one, no_follow_pick, false, false, None)
            .expect("chain path succeeds");
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
        run_with_pickers(&mut client, multi, one, no_follow_pick, false, false, None)
            .expect("cancellation is silent Ok");
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
        run_with_pickers(&mut client, multi, one, no_follow_pick, false, false, None)
            .expect("cancellation is silent Ok");
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
        let err = run_with_pickers(&mut client, multi, one, no_follow_pick, false, false, None)
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
        run_with_pickers(&mut client, multi, one, no_follow_pick, false, false, None)
            .expect("empty list exits Ok");
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
        let err = run_with_pickers(&mut client, multi, one, no_follow_pick, false, false, None)
            .expect_err("wrong variant must surface");
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
        let err = run_with_pickers(&mut client, multi, one, no_follow_pick, false, false, None)
            .expect_err("wrong variant must surface");
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
        let err = run_with_pickers(&mut client, multi, one, no_follow_pick, false, false, None)
            .expect_err("server error must propagate");
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

    // ---- --follow ---------------------------------------------------------

    fn switch_activity_req(name: &str) -> Request {
        Request::Action(Action::SwitchActivity {
            activity: ActivityReferenceArg::Name(name.to_owned()),
        })
    }

    fn focus_workspace_req(ws_id: u64) -> Request {
        Request::Action(Action::FocusWorkspace {
            reference: WorkspaceReferenceArg::Id(ws_id),
        })
    }

    fn three_ipc_follow_setup(client: &mut MockClient) {
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
                ],
            }),
            Reply::Ok(Response::Handled),
        );
    }

    fn multi_work_personal(
        _: &[String],
        _: &HashSet<String>,
    ) -> Result<MultiPickerOutcome, CliError> {
        Ok(MultiPickerOutcome::Selected(vec![
            "Work".into(),
            "Personal".into(),
        ]))
    }

    /// Pins the item shape presented to the follow picker: one row per
    /// saved activity name (in save order) plus `« Stay »` as the last row.
    #[test]
    fn assign_workspace_follow_picker_item_shape_matches_saved_plus_stay() {
        let mut client = MockClient::new();
        three_ipc_follow_setup(&mut client);
        let one = |_: &[String]| -> Result<PickerOutcome, CliError> { unreachable!() };
        let follow_pick = |prompt: &str, items: &[String]| -> Result<PickerOutcome, CliError> {
            assert_eq!(prompt, "Follow?");
            assert_eq!(items, &["Work", "Personal", "« Stay »"]);
            Ok(PickerOutcome::Cancelled)
        };
        run_with_pickers(
            &mut client,
            multi_work_personal,
            one,
            follow_pick,
            true,
            false,
            None,
        )
        .expect("item-shape assertion exits Ok");
        client.assert_consumed_in_order();
    }

    /// Pins the cancellation contract: when the follow picker returns
    /// `Cancelled` (Escape), no follow IPC is dispatched — only the
    /// three earlier calls are consumed.
    #[test]
    fn assign_workspace_follow_cancelled_dispatches_no_follow_ipc() {
        let mut client = MockClient::new();
        three_ipc_follow_setup(&mut client);
        let one = |_: &[String]| -> Result<PickerOutcome, CliError> { unreachable!() };
        // Return Cancelled — the IPC queue must be fully consumed with no
        // additional requests beyond the three set up above.
        let follow_pick = |_: &str, _: &[String]| -> Result<PickerOutcome, CliError> {
            Ok(PickerOutcome::Cancelled)
        };
        run_with_pickers(
            &mut client,
            multi_work_personal,
            one,
            follow_pick,
            true,
            false,
            None,
        )
        .expect("cancelled follow picker exits Ok");
        client.assert_consumed_in_order();
    }

    /// `Selected("Work")` in the follow picker dispatches
    /// `SwitchActivity { Name("Work") }` then `FocusWorkspace { Id(42) }`.
    /// Pins the strict IPC ordering across the full assign-workspace
    /// --follow happy path.
    #[test]
    fn assign_workspace_follow_dispatches_switch_then_focus_against_picked_activity() {
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
            Reply::Ok(Response::Workspaces(vec![ws(42, true, vec![1])])),
        );
        client.expect(
            Request::Action(Action::SetWorkspaceActivities {
                workspace: Some(WorkspaceReferenceArg::Id(42)),
                activities: vec![
                    ActivityReferenceArg::Name("Work".into()),
                    ActivityReferenceArg::Name("Personal".into()),
                ],
            }),
            Reply::Ok(Response::Handled),
        );
        client.expect(switch_activity_req("Work"), Reply::Ok(Response::Handled));
        client.expect(focus_workspace_req(42), Reply::Ok(Response::Handled));

        let multi = |_: &[String], _: &HashSet<String>| {
            Ok(MultiPickerOutcome::Selected(vec![
                "Work".into(),
                "Personal".into(),
            ]))
        };
        let one = |_: &[String]| -> Result<PickerOutcome, CliError> { unreachable!() };
        let follow_pick = |_: &str, items: &[String]| -> Result<PickerOutcome, CliError> {
            // Pick the first saved activity name.
            assert_eq!(items[0], "Work");
            Ok(PickerOutcome::Selected("Work".into()))
        };
        run_with_pickers(&mut client, multi, one, follow_pick, true, false, None)
            .expect("follow happy-path succeeds");
        client.assert_consumed_in_order();
    }

    /// `--follow` with picker returning an unknown row: routes to
    /// `MalformedResponse(Server(_))` (exit 65), NOT folded into
    /// `Cancelled`. Pins the resolver-enums silent-failure discipline.
    #[test]
    fn assign_workspace_follow_unknown_row_routes_to_malformed_server() {
        let mut client = MockClient::new();
        client.expect(
            Request::Activities,
            Reply::Ok(Response::Activities(vec![act(1, "Work")])),
        );
        client.expect(
            Request::Workspaces,
            Reply::Ok(Response::Workspaces(vec![ws(42, true, vec![1])])),
        );
        client.expect(
            Request::Action(Action::SetWorkspaceActivities {
                workspace: Some(WorkspaceReferenceArg::Id(42)),
                activities: vec![ActivityReferenceArg::Name("Work".into())],
            }),
            Reply::Ok(Response::Handled),
        );

        let multi = |_: &[String], _: &HashSet<String>| {
            Ok(MultiPickerOutcome::Selected(vec!["Work".into()]))
        };
        let one = |_: &[String]| -> Result<PickerOutcome, CliError> { unreachable!() };
        let follow_pick = |_: &str, _: &[String]| -> Result<PickerOutcome, CliError> {
            Ok(PickerOutcome::Selected("not-an-item".into()))
        };
        let err = run_with_pickers(&mut client, multi, one, follow_pick, true, false, None)
            .expect_err("unknown row must surface as error");
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

    // ---- --workspace explicit-target -----------------------------------------

    /// `--workspace <id>` present in the snapshot: the explicit workspace is
    /// used as the assignment target (not the focused one). The picker is seeded
    /// from the explicit workspace's activity membership, and
    /// `SetWorkspaceActivities` is dispatched with the explicit workspace's id.
    ///
    /// Uses a setup where the focused workspace (id=7, activity=[1]) differs
    /// from the explicit-id workspace (id=99, activity=[2]), so the seeding-
    /// from-explicit-not-focused property is actually verified rather than
    /// coincidentally passing.
    #[test]
    fn assign_explicit_workspace_id_uses_that_workspace_not_focused() {
        let mut client = MockClient::new();
        client.expect(
            Request::Activities,
            Reply::Ok(Response::Activities(vec![
                act(1, "Work"),
                act(2, "Personal"),
            ])),
        );
        // Workspace 7 is focused (activity 1). Workspace 99 is not focused
        // (activity 2). The explicit --workspace 99 must select workspace 99.
        client.expect(
            Request::Workspaces,
            Reply::Ok(Response::Workspaces(vec![
                ws(7, true, vec![1]),
                ws(99, false, vec![2]),
            ])),
        );
        // Dispatch must use Id(99), not Id(7).
        client.expect(
            Request::Action(Action::SetWorkspaceActivities {
                workspace: Some(WorkspaceReferenceArg::Id(99)),
                activities: vec![ActivityReferenceArg::Name("Personal".into())],
            }),
            Reply::Ok(Response::Handled),
        );

        // The multi picker must be seeded with "Personal" as the pre-checked
        // set (because workspace 99 is in activity 2 = "Personal").
        let multi = |_names: &[String], current: &HashSet<String>| {
            assert!(
                current.contains("Personal"),
                "picker must be seeded from explicit workspace (Personal), not focused (Work)",
            );
            assert!(
                !current.contains("Work"),
                "focused workspace activity (Work) must NOT appear in pre-checked set",
            );
            Ok(MultiPickerOutcome::Selected(vec!["Personal".into()]))
        };
        let one = |_: &[String]| -> Result<PickerOutcome, CliError> { unreachable!() };
        run_with_pickers(
            &mut client,
            multi,
            one,
            no_follow_pick,
            false,
            false,
            Some(99),
        )
        .expect("explicit-workspace happy path succeeds");
        client.assert_consumed_in_order();
    }

    /// `--workspace <id>` with an id NOT in the snapshot: must return
    /// `MalformedResponse(Server("workspace id <N> not found in snapshot"))`,
    /// exit 65, and must NOT dispatch `SetWorkspaceActivities`.
    #[test]
    fn assign_explicit_workspace_id_not_in_snapshot_routes_to_malformed_response() {
        let mut client = MockClient::new();
        client.expect(
            Request::Activities,
            Reply::Ok(Response::Activities(vec![act(1, "Work")])),
        );
        // Snapshot contains only workspace 5 — not workspace 42.
        client.expect(
            Request::Workspaces,
            Reply::Ok(Response::Workspaces(vec![ws(5, true, vec![1])])),
        );
        let multi = |_: &[String], _: &HashSet<String>| -> Result<MultiPickerOutcome, CliError> {
            panic!("picker must not be called when explicit workspace id is missing from snapshot");
        };
        let one = |_: &[String]| -> Result<PickerOutcome, CliError> {
            panic!("chained picker must not be called when explicit workspace id is missing");
        };
        let err = run_with_pickers(
            &mut client,
            multi,
            one,
            no_follow_pick,
            false,
            false,
            Some(42),
        )
        .expect_err("missing explicit workspace id must surface as error");
        let cli_err = err
            .chain()
            .find_map(|e| e.downcast_ref::<CliError>())
            .expect("CliError must be in chain");
        match cli_err {
            CliError::MalformedResponse(MalformedResponseSource::Server(msg)) => {
                assert!(
                    msg.contains("workspace id 42 not found in snapshot"),
                    "expected synthetic-string carrier message, got: {msg}",
                );
            }
            other => panic!("expected MalformedResponse(Server), got {other:?}"),
        }
        assert_eq!(cli_err.exit_code(), 65);
        client.assert_consumed_in_order();
    }

    /// `--workspace <id>` combined with `--follow`: the follow leg's
    /// `FocusWorkspace` must target the EXPLICIT workspace id (ws 99), not the
    /// focused workspace (ws 7). Mirrors
    /// `move_workspace_explicit_workspace_with_follow_skips_capture_roundtrip`
    /// for the `assign-workspace` verb.
    ///
    /// Setup: ws 7 is focused (activity 1 = "Work"), ws 99 is not focused
    /// (activity 2 = "Personal"). `--workspace 99 --follow` is passed.
    /// The expected IPC sequence is:
    ///   1. `Activities` (fetch)
    ///   2. `Workspaces` (fetch)
    ///   3. `SetWorkspaceActivities { Id(99), ["Personal"] }` (assign)
    ///   4. `SwitchActivity { Name("Personal") }` (follow)
    ///   5. `FocusWorkspace { Id(99) }` (follow — explicit workspace, not ws 7)
    #[test]
    fn assign_explicit_workspace_with_follow_targets_explicit_workspace() {
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
            Reply::Ok(Response::Workspaces(vec![
                ws(7, true, vec![1]),
                ws(99, false, vec![2]),
            ])),
        );
        client.expect(
            Request::Action(Action::SetWorkspaceActivities {
                workspace: Some(WorkspaceReferenceArg::Id(99)),
                activities: vec![ActivityReferenceArg::Name("Personal".into())],
            }),
            Reply::Ok(Response::Handled),
        );
        // Follow: switch to "Personal" then focus ws 99 (explicit id).
        client.expect(
            switch_activity_req("Personal"),
            Reply::Ok(Response::Handled),
        );
        client.expect(focus_workspace_req(99), Reply::Ok(Response::Handled));

        let multi = |_names: &[String], current: &HashSet<String>| {
            // Picker is seeded from ws 99's membership ("Personal").
            assert!(current.contains("Personal"));
            Ok(MultiPickerOutcome::Selected(vec!["Personal".into()]))
        };
        let one = |_: &[String]| -> Result<PickerOutcome, CliError> { unreachable!() };
        let follow_pick = |_: &str, items: &[String]| -> Result<PickerOutcome, CliError> {
            // Pick "Personal" to trigger the follow path.
            Ok(PickerOutcome::Selected(items[0].clone()))
        };
        run_with_pickers(&mut client, multi, one, follow_pick, true, false, Some(99))
            .expect("explicit workspace + follow targets the explicit workspace");
        client.assert_consumed_in_order();
    }

    /// `--workspace` unset (regression pin): the focused-workspace path runs
    /// unchanged — the focused workspace's id is used for dispatch and the
    /// picker is seeded from its activity membership.
    #[test]
    fn assign_workspace_unset_uses_focused_workspace() {
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
            Reply::Ok(Response::Workspaces(vec![
                ws(7, true, vec![1]),
                ws(99, false, vec![2]),
            ])),
        );
        // Dispatch must use the focused workspace id (7), not the non-focused
        // one (99).
        client.expect(
            Request::Action(Action::SetWorkspaceActivities {
                workspace: Some(WorkspaceReferenceArg::Id(7)),
                activities: vec![ActivityReferenceArg::Name("Work".into())],
            }),
            Reply::Ok(Response::Handled),
        );

        // Pre-checked set must reflect the focused workspace's memberships.
        let multi = |_: &[String], current: &HashSet<String>| {
            assert!(
                current.contains("Work"),
                "picker must be seeded from focused workspace (Work)",
            );
            assert!(
                !current.contains("Personal"),
                "non-focused workspace activity (Personal) must NOT appear in pre-checked set",
            );
            Ok(MultiPickerOutcome::Selected(vec!["Work".into()]))
        };
        let one = |_: &[String]| -> Result<PickerOutcome, CliError> { unreachable!() };
        run_with_pickers(&mut client, multi, one, no_follow_pick, false, false, None)
            .expect("unset workspace uses focused path");
        client.assert_consumed_in_order();
    }
}
