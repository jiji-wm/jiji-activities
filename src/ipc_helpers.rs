//! Shared IPC plumbing reused across subcommand modules.
//!
//! The following helpers live here:
//!
//! - [`variant_name`] — static name for a [`Response`] variant, used
//!   by every `Response::Handled`-or-mismatch site to populate the
//!   `WrongVariant::got` field without dragging the full Debug payload
//!   (which can be arbitrarily large for e.g. `Response::Windows(...)`).
//! - [`send_expect_handled`] — wraps a `client.send(req)` call that
//!   expects `Response::Handled`, routing the compositor's
//!   `"activity not found"` wire string to the typed
//!   [`CliError::ActivityNotFound`] when an activity name is in scope.
//! - [`send_expect_handled_or_no_op`] — like [`send_expect_handled`] but
//!   additionally accepts [`Response::NoOp`] as a non-error outcome,
//!   surfacing the reason via [`HandledOutcome`]. Used by the
//!   `MoveWindowToWorkspace*` dispatchers, whose compositor handlers
//!   may legitimately reply with a durable no-op signal.
//! - [`send_expect_activities`] — wraps a `client.send(Request::Activities)`
//!   call that expects `Response::Activities`; mismatched variants surface
//!   as `WrongVariant`.
//! - [`send_expect_workspaces`] — wraps a `client.send(Request::Workspaces)`
//!   call that expects `Response::Workspaces`; mismatched variants surface
//!   as `WrongVariant`.
//! - [`names_focused_first`] — pure helper that reorders an
//!   [`Activity`] slice's names so the focused activity (if any) is
//!   first; remaining names preserve their compositor-supplied order.
//!
//! Centralising these avoids N-way duplication between
//! `crate::switch`, `crate::switch_previous`,
//! `crate::move_workspace`, `crate::assign_workspace`, `crate::list`,
//! and `crate::move_window`, and closes the latent risk that
//! independent definitions drift apart.

use niri_ipc::{Activity, NoOpReason, Request, Response, Workspace};

use crate::error::{CliError, MalformedResponseSource};
use crate::ipc::{IpcError, NiriClient};

/// Outcome of a dispatch that may legitimately resolve to a no-op.
///
/// Strictly mirrors the two compositor reply variants
/// [`send_expect_handled_or_no_op`] accepts: [`Response::Handled`] (state
/// changed) and [`Response::NoOp`] (preconditions met, target already
/// matched current state). Any other reply variant is treated as a
/// contract violation and surfaces as
/// [`MalformedResponseSource::WrongVariant`] from the helper — it does
/// NOT reach this enum.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HandledOutcome {
    Handled,
    NoOp(NoOpReason),
}

/// Static variant name for a [`Response`].
///
/// Returns a `&'static str` matching the variant constructor name
/// (`"Response::Handled"`, `"Response::Activities"`, ...). Avoids
/// formatting the full Debug payload — for variants that carry large
/// vectors this would otherwise produce arbitrarily large
/// `WrongVariant::got` strings. The catch-all arm
/// (`"Response::<unknown>"`) accommodates `#[non_exhaustive]` additions
/// to `Response` without breaking the build.
pub(crate) fn variant_name(r: &Response) -> &'static str {
    match r {
        Response::Handled => "Response::Handled",
        Response::NoOp(_) => "Response::NoOp",
        Response::Version(_) => "Response::Version",
        Response::Outputs(_) => "Response::Outputs",
        Response::Workspaces(_) => "Response::Workspaces",
        Response::Windows(_) => "Response::Windows",
        Response::Layers(_) => "Response::Layers",
        Response::KeyboardLayouts(_) => "Response::KeyboardLayouts",
        Response::FocusedOutput(_) => "Response::FocusedOutput",
        Response::Activities(_) => "Response::Activities",
        Response::FocusedActivity(_) => "Response::FocusedActivity",
        Response::FocusedWindow(_) => "Response::FocusedWindow",
        Response::PickedWindow(_) => "Response::PickedWindow",
        Response::PickedColor(_) => "Response::PickedColor",
        Response::OutputConfigChanged(_) => "Response::OutputConfigChanged",
        Response::OverviewState(_) => "Response::OverviewState",
        Response::Casts(_) => "Response::Casts",
        _ => "Response::<unknown>",
    }
}

/// Sends `req`, expects `Response::Handled`, and routes the
/// compositor's `"activity not found"` wire string to the typed
/// [`CliError::ActivityNotFound`] when an activity name is in scope.
///
/// **Contract:**
/// - `Ok(Response::Handled)` → `Ok(())`.
/// - `Ok(other)` →
///   `CliError::MalformedResponse(WrongVariant { expected: "Response::Handled", got: variant_name(&other) })`.
/// - `Err(IpcError::Server("activity not found"))` →
///   `CliError::ActivityNotFound(name)` when `activity_name` is
///   `Some(name)`; falls through to
///   `CliError::MalformedResponse(Server)` when `activity_name` is
///   `None` (the caller has no name in scope to attach — e.g.
///   `switch-previous`, where the compositor implicitly picks the
///   previous activity).
/// - Other `Err(IpcError::*)` flow through the existing
///   `IpcError → CliError` `From` mapping unchanged.
///
/// The match is strict equality on `"activity not found"` — any
/// drift (suffix, case, trailing punct) falls through to
/// `MalformedResponse(Server)`, matching the existing
/// `crate::switch` contract.
pub(crate) fn send_expect_handled(
    client: &mut dyn NiriClient,
    req: Request,
    activity_name: Option<&str>,
) -> anyhow::Result<()> {
    debug_assert!(
        activity_name.is_none_or(|n| !n.is_empty()),
        "send_expect_handled: activity_name must be non-empty when Some",
    );
    match client.send(req) {
        Ok(Response::Handled) => Ok(()),
        Ok(other) => Err(
            CliError::MalformedResponse(MalformedResponseSource::WrongVariant {
                expected: "Response::Handled",
                got: variant_name(&other).into(),
            })
            .into(),
        ),
        Err(IpcError::Server(msg)) if msg == "activity not found" => match activity_name {
            Some(name) => Err(CliError::ActivityNotFound(name.to_owned()).into()),
            None => Err(CliError::MalformedResponse(MalformedResponseSource::Server(msg)).into()),
        },
        Err(other) => Err(CliError::from(other).into()),
    }
}

/// Like [`send_expect_handled`], but additionally accepts
/// [`Response::NoOp`] as a non-error outcome.
///
/// **Contract:**
/// - `Ok(Response::Handled)` → `Ok(HandledOutcome::Handled)`.
/// - `Ok(Response::NoOp(reason))` → `Ok(HandledOutcome::NoOp(reason))`.
/// - `Ok(other)` →
///   `CliError::MalformedResponse(WrongVariant { expected:
///   "Response::Handled | Response::NoOp", got: variant_name(&other) })`.
/// - `Err(IpcError::Server(msg))` →
///   `CliError::MalformedResponse(Server(msg))` (server errors flow
///   through the existing mapping; this helper has no activity-name
///   routing because its only callers — `MoveWindowToWorkspace*`
///   dispatches — reference workspaces by id, not activities by name).
/// - Other `Err(IpcError::*)` flow through the existing
///   `IpcError → CliError` `From` mapping unchanged.
///
/// **Intended caller family:** `Action::MoveWindowToWorkspace` /
/// `Action::MoveWindowToWorkspaceById` dispatches (and any future action
/// whose compositor handler may legitimately reply with
/// [`Response::NoOp`]). Other dispatches MUST continue to use
/// [`send_expect_handled`] — accepting `NoOp` on, e.g., `CreateActivity`
/// would be a silent contract drift.
pub(crate) fn send_expect_handled_or_no_op(
    client: &mut dyn NiriClient,
    req: Request,
) -> anyhow::Result<HandledOutcome> {
    match client.send(req) {
        Ok(Response::Handled) => Ok(HandledOutcome::Handled),
        Ok(Response::NoOp(reason)) => Ok(HandledOutcome::NoOp(reason)),
        Ok(other) => Err(
            CliError::MalformedResponse(MalformedResponseSource::WrongVariant {
                expected: "Response::Handled | Response::NoOp",
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

/// Sends [`Request::Activities`] and unwraps the matching
/// [`Response::Activities`] payload.
///
/// **Contract:**
/// - `Response::Activities(v)` → `Ok(v)`.
/// - Any other `Response` variant →
///   `CliError::MalformedResponse(WrongVariant { expected:
///   "Response::Activities", got: variant_name(&other) })`.
/// - Transport / decode / server errors flow through the existing
///   `IpcError → CliError` mapping unchanged.
pub(crate) fn send_expect_activities(client: &mut dyn NiriClient) -> anyhow::Result<Vec<Activity>> {
    let resp = client.send(Request::Activities).map_err(CliError::from)?;
    match resp {
        Response::Activities(v) => Ok(v),
        other => Err(
            CliError::MalformedResponse(MalformedResponseSource::WrongVariant {
                expected: "Response::Activities",
                got: variant_name(&other).into(),
            })
            .into(),
        ),
    }
}

/// Sends [`Request::Workspaces`] and unwraps the matching
/// [`Response::Workspaces`] payload.
///
/// **Contract:**
/// - `Response::Workspaces(v)` → `Ok(v)`.
/// - Any other `Response` variant →
///   `CliError::MalformedResponse(WrongVariant { expected:
///   "Response::Workspaces", got: variant_name(&other) })`.
/// - Transport / decode / server errors flow through the existing
///   `IpcError → CliError` mapping unchanged.
pub(crate) fn send_expect_workspaces(
    client: &mut dyn NiriClient,
) -> anyhow::Result<Vec<Workspace>> {
    let resp = client.send(Request::Workspaces).map_err(CliError::from)?;
    match resp {
        Response::Workspaces(v) => Ok(v),
        other => Err(
            CliError::MalformedResponse(MalformedResponseSource::WrongVariant {
                expected: "Response::Workspaces",
                got: variant_name(&other).into(),
            })
            .into(),
        ),
    }
}

/// Returns activity names with the focused one (if any) hoisted to the
/// front; remaining names preserve their compositor-supplied order.
///
/// Pure helper, no IPC. The first `is_active` activity wins; the
/// compositor invariant is that at most one activity is active at a
/// time, but defensively coping with multiple keeps the helper total.
pub(crate) fn names_focused_first(activities: &[Activity]) -> Vec<String> {
    let mut focused: Option<String> = None;
    let mut rest: Vec<String> = Vec::with_capacity(activities.len());
    for a in activities {
        if a.is_active && focused.is_none() {
            focused = Some(a.name.clone());
        } else {
            rest.push(a.name.clone());
        }
    }
    let mut out = Vec::with_capacity(activities.len());
    if let Some(f) = focused {
        out.push(f);
    }
    out.extend(rest);
    out
}

#[cfg(test)]
mod tests {
    use niri_ipc::{Activity, NoOpReason, Reply, Request, Response};

    use super::*;
    use crate::error::{CliError, MalformedResponseSource};
    use crate::ipc::MockClient;

    fn act(id: u64, name: &str, is_active: bool) -> Activity {
        Activity {
            id,
            name: name.into(),
            is_active,
            is_config_declared: true,
            ..Default::default()
        }
    }

    // ---- variant_name ----

    #[test]
    fn variant_name_recognises_handled() {
        assert_eq!(variant_name(&Response::Handled), "Response::Handled");
    }

    #[test]
    fn variant_name_recognises_activities() {
        assert_eq!(
            variant_name(&Response::Activities(vec![])),
            "Response::Activities",
        );
    }

    #[test]
    fn variant_name_recognises_workspaces() {
        assert_eq!(
            variant_name(&Response::Workspaces(vec![])),
            "Response::Workspaces",
        );
    }

    /// Pins every known [`Response`] variant so an upstream addition that
    /// silently degrades to `"Response::<unknown>"` surfaces as a test
    /// failure rather than a silent wrong-name in `WrongVariant::got`.
    ///
    /// When niri-ipc adds a new variant: add a row here and update the
    /// `variant_name` match arm above. Both must move together.
    #[test]
    fn variant_name_covers_all_known_variants() {
        use niri_ipc::{KeyboardLayouts, NoOpReason, OutputConfigChanged, Overview, Response};
        let cases: &[(&Response, &str)] = &[
            (&Response::Handled, "Response::Handled"),
            (
                &Response::NoOp(NoOpReason::AlreadyOnTarget { workspace_id: 0 }),
                "Response::NoOp",
            ),
            (&Response::Version("v".into()), "Response::Version"),
            (
                &Response::Outputs(std::collections::HashMap::new()),
                "Response::Outputs",
            ),
            (&Response::Workspaces(vec![]), "Response::Workspaces"),
            (&Response::Windows(vec![]), "Response::Windows"),
            (&Response::Layers(vec![]), "Response::Layers"),
            (
                &Response::KeyboardLayouts(KeyboardLayouts {
                    names: vec![],
                    current_idx: 0,
                }),
                "Response::KeyboardLayouts",
            ),
            (&Response::FocusedOutput(None), "Response::FocusedOutput"),
            (&Response::Activities(vec![]), "Response::Activities"),
            (
                &Response::FocusedActivity(niri_ipc::Activity::default()),
                "Response::FocusedActivity",
            ),
            (&Response::FocusedWindow(None), "Response::FocusedWindow"),
            (&Response::PickedWindow(None), "Response::PickedWindow"),
            (&Response::PickedColor(None), "Response::PickedColor"),
            (
                &Response::OutputConfigChanged(OutputConfigChanged::Applied),
                "Response::OutputConfigChanged",
            ),
            (
                &Response::OverviewState(Overview { is_open: false }),
                "Response::OverviewState",
            ),
            (&Response::Casts(vec![]), "Response::Casts"),
        ];
        for (resp, expected) in cases {
            assert_eq!(
                variant_name(resp),
                *expected,
                "variant_name mismatch for {resp:?}",
            );
        }
    }

    // ---- send_expect_handled ----

    #[test]
    fn send_expect_handled_handled_is_ok() {
        let mut client = MockClient::new();
        client.expect(Request::Version, Reply::Ok(Response::Handled));
        send_expect_handled(&mut client, Request::Version, Some("Work"))
            .expect("Handled reply must succeed");
        client.assert_consumed_in_order();
    }

    #[test]
    fn send_expect_handled_wrong_variant_is_malformed() {
        let mut client = MockClient::new();
        client.expect(Request::Version, Reply::Ok(Response::Version("v".into())));
        let err = send_expect_handled(&mut client, Request::Version, Some("Work"))
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
        client.assert_consumed_in_order();
    }

    #[test]
    fn send_expect_handled_with_name_routes_not_found_to_typed_error() {
        let mut client = MockClient::new();
        client.expect(Request::Version, Err("activity not found".to_owned()));
        let err = send_expect_handled(&mut client, Request::Version, Some("Work"))
            .expect_err("not-found must fail");
        let cli_err = err
            .chain()
            .find_map(|e| e.downcast_ref::<CliError>())
            .expect("CliError must be in chain");
        match cli_err {
            CliError::ActivityNotFound(name) => assert_eq!(name, "Work"),
            other => panic!("expected ActivityNotFound, got {other:?}"),
        }
        client.assert_consumed_in_order();
    }

    #[test]
    fn send_expect_handled_without_name_routes_not_found_to_malformed() {
        // No activity name in scope (the `switch-previous` shape). The
        // "activity not found" wire string MUST fall through to
        // MalformedResponse(Server) rather than fabricating an
        // ActivityNotFound with an empty name.
        let mut client = MockClient::new();
        client.expect(Request::Version, Err("activity not found".to_owned()));
        let err = send_expect_handled(&mut client, Request::Version, None)
            .expect_err("not-found must fail");
        let cli_err = err
            .chain()
            .find_map(|e| e.downcast_ref::<CliError>())
            .expect("CliError must be in chain");
        match cli_err {
            CliError::MalformedResponse(MalformedResponseSource::Server(msg)) => {
                assert_eq!(msg, "activity not found");
            }
            other => panic!("expected MalformedResponse(Server), got {other:?}"),
        }
        client.assert_consumed_in_order();
    }

    #[test]
    fn send_expect_handled_other_server_error_routes_to_malformed() {
        let mut client = MockClient::new();
        client.expect(Request::Version, Err("some other failure".to_owned()));
        let err = send_expect_handled(&mut client, Request::Version, Some("Work"))
            .expect_err("server error must fail");
        let cli_err = err
            .chain()
            .find_map(|e| e.downcast_ref::<CliError>())
            .expect("CliError must be in chain");
        match cli_err {
            CliError::MalformedResponse(MalformedResponseSource::Server(msg)) => {
                assert_eq!(msg, "some other failure");
            }
            other => panic!("expected MalformedResponse(Server), got {other:?}"),
        }
        client.assert_consumed_in_order();
    }

    // ---- send_expect_handled_or_no_op ----

    #[test]
    fn send_expect_handled_or_no_op_handled_is_ok() {
        let mut client = MockClient::new();
        client.expect(Request::Version, Reply::Ok(Response::Handled));
        let outcome = send_expect_handled_or_no_op(&mut client, Request::Version)
            .expect("Handled reply must succeed");
        assert_eq!(outcome, HandledOutcome::Handled);
        client.assert_consumed_in_order();
    }

    #[test]
    fn send_expect_handled_or_no_op_no_op_is_returned() {
        let mut client = MockClient::new();
        client.expect(
            Request::Version,
            Reply::Ok(Response::NoOp(NoOpReason::AlreadyOnTarget {
                workspace_id: 42,
            })),
        );
        let outcome = send_expect_handled_or_no_op(&mut client, Request::Version)
            .expect("NoOp reply must succeed");
        assert_eq!(
            outcome,
            HandledOutcome::NoOp(NoOpReason::AlreadyOnTarget { workspace_id: 42 }),
        );
        client.assert_consumed_in_order();
    }

    #[test]
    fn send_expect_handled_or_no_op_server_error_routes_to_malformed() {
        let mut client = MockClient::new();
        client.expect(Request::Version, Err("some server failure".to_owned()));
        let err = send_expect_handled_or_no_op(&mut client, Request::Version)
            .expect_err("server error must fail");
        let cli_err = err
            .chain()
            .find_map(|e| e.downcast_ref::<CliError>())
            .expect("CliError must be in chain");
        match cli_err {
            CliError::MalformedResponse(MalformedResponseSource::Server(msg)) => {
                assert_eq!(msg, "some server failure");
            }
            other => panic!("expected MalformedResponse(Server), got {other:?}"),
        }
        client.assert_consumed_in_order();
    }

    #[test]
    fn send_expect_handled_or_no_op_wrong_variant_is_malformed() {
        let mut client = MockClient::new();
        client.expect(Request::Version, Reply::Ok(Response::Version("v".into())));
        let err = send_expect_handled_or_no_op(&mut client, Request::Version)
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
                assert_eq!(*expected, "Response::Handled | Response::NoOp");
                assert_eq!(got, "Response::Version");
            }
            other => panic!("expected WrongVariant, got {other:?}"),
        }
        client.assert_consumed_in_order();
    }

    // ---- send_expect_activities / send_expect_workspaces ----

    #[test]
    fn send_expect_activities_wrong_variant_is_malformed() {
        let mut client = MockClient::new();
        client.expect(
            Request::Activities,
            Reply::Ok(Response::Version("v".into())),
        );
        let err = send_expect_activities(&mut client).expect_err("wrong variant must fail");
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
    fn send_expect_workspaces_wrong_variant_is_malformed() {
        let mut client = MockClient::new();
        client.expect(
            Request::Workspaces,
            Reply::Ok(Response::Version("v".into())),
        );
        let err = send_expect_workspaces(&mut client).expect_err("wrong variant must fail");
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

    // ---- names_focused_first ----

    #[test]
    fn names_focused_first_hoists_focused_to_front() {
        let acts = vec![
            act(1, "Work", false),
            act(2, "Personal", true),
            act(3, "Gaming", false),
        ];
        let names = names_focused_first(&acts);
        assert_eq!(names, vec!["Personal", "Work", "Gaming"]);
    }

    #[test]
    fn names_focused_first_no_focused_passes_through_unchanged() {
        let acts = vec![act(1, "Work", false), act(2, "Personal", false)];
        let names = names_focused_first(&acts);
        assert_eq!(names, vec!["Work", "Personal"]);
    }

    #[test]
    fn names_focused_first_empty_is_empty() {
        let names = names_focused_first(&[]);
        assert!(names.is_empty());
    }

    #[test]
    fn names_focused_first_multi_active_hoists_first_only() {
        // Defensive: compositor invariant says at most one activity is
        // active, but if multiple arrive only the first is hoisted.
        let acts = vec![
            act(1, "Work", true),
            act(2, "Personal", true),
            act(3, "Gaming", false),
        ];
        let names = names_focused_first(&acts);
        assert_eq!(names, vec!["Work", "Personal", "Gaming"]);
    }
}
