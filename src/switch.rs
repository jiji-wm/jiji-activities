//! `switch` subcommand: switch to an activity by name.
//!
//! Dispatches `Action::SwitchActivity { activity: Name(name) }` over
//! IPC and expects `Response::Handled`. Two reply shapes get
//! special-cased typing:
//!
//! - `Reply::Err("activity not found")` → [`CliError::ActivityNotFound`]
//!   (exit 66). The literal string is the compositor's wire contract
//!   for an unresolved `ActivityReferenceArg::Name`.
//! - Any other `Reply::Err(msg)` flows through the existing
//!   `IpcError::Server → CliError::MalformedResponse(Server)` mapping
//!   (exit 65) — the CLI cannot safely classify opaque server strings
//!   beyond the one we know about.
//!
//! The compositor returns `Response::Handled` for both newly-switched
//! and already-active activities; both surface here as `Ok(())` (exit
//! 0). Unit tests pin that contract so a future compositor change that
//! started returning a typed error for the already-active case would
//! fail loudly.

use anyhow::{Context, Result};
use niri_ipc::{Action, Activity, ActivityReferenceArg, Request, Response};

use crate::error::{CliError, MalformedResponseSource};
use crate::ipc::NiriClient;
use crate::ipc_helpers::{names_focused_first, send_expect_handled, variant_name};
use crate::picker::PickerOutcome;

/// Switches to the activity named `name` over IPC.
///
/// **Contract:** issues exactly one IPC request
/// (`Request::Action(Action::SwitchActivity { activity: Name(name) })`)
/// and expects `Response::Handled`. Returns:
///
/// - `Ok(())` on `Reply::Ok(Response::Handled)` — covers both
///   newly-switched and already-active no-op shapes.
/// - `CliError::ActivityNotFound(name)` (exit 66) when the compositor
///   returns `Reply::Err("activity not found")`.
/// - `CliError::MalformedResponse(Server(msg))` (exit 65) for any
///   other `Reply::Err(msg)`.
/// - `CliError::MalformedResponse(WrongVariant { .. })` (exit 65) when
///   the reply parsed cleanly but the inner `Response` variant was not
///   `Handled`.
/// - Transport / decode errors flow through the existing `IpcError`
///   → `CliError` `From` impl unchanged.
///
/// The IPC error is wrapped with `.context("switching activity")` so
/// the operation surfaces in the stderr chain.
pub(crate) fn run(client: &mut dyn NiriClient, name: &str) -> Result<()> {
    let req = Request::Action(Action::SwitchActivity {
        activity: ActivityReferenceArg::Name(name.to_owned()),
    });
    send_expect_handled(client, req, Some(name)).context("switching activity")
}

/// Sends [`Request::Activities`] and unwraps the expected
/// [`Response::Activities`].
///
/// Mirrors `list::send_expect_activities`; kept local rather than shared
/// because re-exporting that helper would widen `list`'s public surface
/// for a single re-use. The mismatch path produces the same typed
/// `WrongVariant` error so behavioural parity is preserved.
fn send_expect_activities(client: &mut dyn NiriClient) -> Result<Vec<Activity>> {
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

/// Opens a single-select picker over the current activity list, then
/// dispatches `switch::run` against the chosen name.
///
/// **Contract:**
/// - Issues `Request::Activities` first.
/// - If the activity list is empty, writes a single-line diagnostic to
///   stderr (`niri-activities: no activities configured; nothing to
///   switch to`) and returns `Ok(())` — exit 0. The picker is never
///   spawned because an empty menu is worse UX than a no-op; the
///   stderr line tells the user *why* nothing happened.
/// - Otherwise reorders names with [`names_focused_first`] so the
///   currently-focused activity is the default highlight, calls `pick`,
///   and on `Selected(name)` delegates to [`run`] (which issues a second
///   IPC call: `Request::Action(SwitchActivity)`).
/// - On `Cancelled`, returns `Ok(())` — user dismissal is exit 0.
///
/// The `pick` parameter is a closure so unit tests can inject a stub
/// without spawning `fuzzel`; production wiring passes
/// [`picker::pick_one`].
pub(crate) fn run_picker<F>(client: &mut dyn NiriClient, pick: F) -> Result<()>
where
    F: FnOnce(&str, &[String]) -> Result<PickerOutcome, CliError>,
{
    let activities = send_expect_activities(client).context("requesting activities")?;
    if activities.is_empty() {
        // Nothing to pick. Skip the picker spawn — an empty `fuzzel`
        // menu is worse UX than a no-op — and tell the user why nothing
        // happened so the silence is diagnosable.
        eprintln!("niri-activities: no activities configured; nothing to switch to");
        return Ok(());
    }
    let names = names_focused_first(&activities);
    match pick("Switch to activity:", &names)? {
        PickerOutcome::Cancelled => Ok(()),
        PickerOutcome::Selected(name) => run(client, &name),
    }
}

#[cfg(test)]
mod tests {
    use niri_ipc::{Action, Activity, ActivityReferenceArg, Reply, Request, Response};

    use super::*;
    use crate::error::{CliError, MalformedResponseSource};
    use crate::ipc::MockClient;

    fn switch_req(name: &str) -> Request {
        Request::Action(Action::SwitchActivity {
            activity: ActivityReferenceArg::Name(name.to_owned()),
        })
    }

    #[test]
    fn switch_dispatches_action_with_name_arg() {
        let mut client = MockClient::new();
        client.expect(switch_req("Work"), Reply::Ok(Response::Handled));
        run(&mut client, "Work").expect("switch succeeds on Handled");
        client.assert_consumed_in_order();
    }

    #[test]
    fn switch_unknown_name_maps_to_activity_not_found() {
        let mut client = MockClient::new();
        client.expect(switch_req("Work"), Err("activity not found".to_owned()));
        let err = run(&mut client, "Work").expect_err("unknown name must fail");
        let cli_err = err
            .chain()
            .find_map(|e| e.downcast_ref::<CliError>())
            .expect("CliError must remain downcastable through .context wrap");
        match cli_err {
            CliError::ActivityNotFound(name) => assert_eq!(name, "Work"),
            other => panic!("expected ActivityNotFound, got {other:?}"),
        }
        assert_eq!(cli_err.exit_code(), 66);
        client.assert_consumed_in_order();
    }

    #[test]
    fn switch_already_active_is_silent_ok() {
        // The compositor returns Response::Handled for an already-active
        // activity (no-op shape, same as a real switch). Pin that the
        // CLI surfaces Ok(()) — a future compositor change that started
        // returning Reply::Err for the no-op case would fail this test.
        let mut client = MockClient::new();
        client.expect(switch_req("Work"), Reply::Ok(Response::Handled));
        run(&mut client, "Work").expect("already-active no-op exits Ok");
        client.assert_consumed_in_order();
    }

    #[test]
    fn switch_other_server_error_routes_to_malformed_response() {
        // Any Reply::Err(msg) other than the literal "activity not found"
        // falls through to the existing IpcError::Server →
        // MalformedResponse(Server) mapping (exit 65). The compositor's
        // "activity switch blocked: …" envelope is the motivating case.
        let mut client = MockClient::new();
        client.expect(
            switch_req("Work"),
            Err("activity switch blocked: workspace switch gesture".to_owned()),
        );
        let err = run(&mut client, "Work").expect_err("server error must surface");
        let cli_err = err
            .chain()
            .find_map(|e| e.downcast_ref::<CliError>())
            .expect("CliError must be in chain");
        match cli_err {
            CliError::MalformedResponse(MalformedResponseSource::Server(msg)) => {
                assert_eq!(msg, "activity switch blocked: workspace switch gesture");
            }
            other => panic!("expected MalformedResponse(Server), got {other:?}"),
        }
        assert_eq!(cli_err.exit_code(), 65);
        client.assert_consumed_in_order();
    }

    // Negative-space tests for the strict-equality "activity not found" match.
    // Any variant of that string (suffix, case, trailing punct) must route to
    // MalformedResponse(Server) (exit 65), NOT ActivityNotFound (exit 66).
    // This pins that the match is strict equality, not substring/prefix/case.

    #[test]
    fn switch_not_found_with_name_suffix_routes_to_malformed_not_not_found() {
        // Most plausible future drift: compositor adds the name to the
        // message ("activity not found: Work"). Must NOT match exit 66.
        let mut client = MockClient::new();
        client.expect(
            switch_req("Work"),
            Err("activity not found: Work".to_owned()),
        );
        let err = run(&mut client, "Work").expect_err("must fail");
        let cli_err = err
            .chain()
            .find_map(|e| e.downcast_ref::<CliError>())
            .expect("CliError must be in chain");
        match cli_err {
            CliError::MalformedResponse(MalformedResponseSource::Server(_)) => {}
            other => {
                panic!("expected MalformedResponse(Server), not ActivityNotFound; got {other:?}")
            }
        }
        assert_eq!(cli_err.exit_code(), 65);
        client.assert_consumed_in_order();
    }

    #[test]
    fn switch_not_found_capitalized_routes_to_malformed_not_not_found() {
        // Case drift: "Activity not found" (capital A). Must NOT match exit 66.
        let mut client = MockClient::new();
        client.expect(switch_req("Work"), Err("Activity not found".to_owned()));
        let err = run(&mut client, "Work").expect_err("must fail");
        let cli_err = err
            .chain()
            .find_map(|e| e.downcast_ref::<CliError>())
            .expect("CliError must be in chain");
        match cli_err {
            CliError::MalformedResponse(MalformedResponseSource::Server(_)) => {}
            other => {
                panic!("expected MalformedResponse(Server), not ActivityNotFound; got {other:?}")
            }
        }
        assert_eq!(cli_err.exit_code(), 65);
        client.assert_consumed_in_order();
    }

    #[test]
    fn switch_wrong_response_variant_is_malformed() {
        // A Reply::Ok with a non-Handled inner variant must surface as
        // WrongVariant naming "Response::Handled" as the expectation.
        let mut client = MockClient::new();
        client.expect(switch_req("Work"), Reply::Ok(Response::Version("v".into())));
        let err = run(&mut client, "Work").expect_err("wrong variant must fail");
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
    fn switch_request_is_action_variant_not_raw() {
        // Pin the request shape: Request::Action(Action::SwitchActivity {
        // activity: Name(name) }) and not some sibling variant. The
        // MockClient's queue-equality check would already reject a
        // mismatched request, but stating it as its own test makes the
        // contract visible at a glance.
        let mut client = MockClient::new();
        let expected = Request::Action(Action::SwitchActivity {
            activity: ActivityReferenceArg::Name("Focus".to_owned()),
        });
        client.expect(expected, Reply::Ok(Response::Handled));
        run(&mut client, "Focus").expect("matching shape succeeds");
        client.assert_consumed_in_order();
    }

    #[test]
    fn switch_preserves_context_in_error_chain() {
        // `{err:#}` must include both the .context("switching activity")
        // layer and the underlying "activity not found" leaf — proving
        // .context() did not shadow the typed CliError's Display.
        let mut client = MockClient::new();
        client.expect(switch_req("Work"), Err("activity not found".to_owned()));
        let err = run(&mut client, "Work").expect_err("must fail");
        let formatted = format!("{err:#}");
        assert!(
            formatted.contains("switching activity"),
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
    fn run_picker_selects_and_dispatches_switch() {
        // Two IPC calls: Activities (for the menu), then SwitchActivity
        // (after picker returns Selected). MockClient pins the order.
        let mut client = MockClient::new();
        client.expect(
            Request::Activities,
            Reply::Ok(Response::Activities(vec![
                act(1, "Work", true),
                act(2, "Personal", false),
            ])),
        );
        client.expect(switch_req("Personal"), Reply::Ok(Response::Handled));

        let pick = |prompt: &str, items: &[String]| -> Result<PickerOutcome, CliError> {
            assert_eq!(prompt, "Switch to activity:");
            // Focused-first reordering: Work (focused) precedes Personal.
            assert_eq!(items, &["Work".to_owned(), "Personal".to_owned()]);
            Ok(PickerOutcome::Selected("Personal".to_owned()))
        };

        run_picker(&mut client, pick).expect("happy path succeeds");
        client.assert_consumed_in_order();
    }

    #[test]
    fn run_picker_empty_activities_warns_and_exits_zero() {
        // Empty activity list: exactly one IPC call (Activities), no
        // pick invocation, no Switch dispatch. Exits Ok. (The stderr
        // diagnostic is asserted by the integration test in
        // `tests/picker_shim.rs`; this unit test pins the no-pick /
        // no-second-IPC / Ok(()) contract.)
        let mut client = MockClient::new();
        client.expect(Request::Activities, Reply::Ok(Response::Activities(vec![])));

        let pick = |_prompt: &str, _items: &[String]| -> Result<PickerOutcome, CliError> {
            panic!("pick must not be called when activity list is empty");
        };

        run_picker(&mut client, pick).expect("empty list exits Ok");
        client.assert_consumed_in_order();
    }

    #[test]
    fn run_picker_cancellation_skips_switch_dispatch() {
        // User dismisses the menu → no Switch IPC call. Only one
        // queued reply (Activities); if `run_picker` dispatched a
        // Switch the MockClient would panic on unexpected request.
        let mut client = MockClient::new();
        client.expect(
            Request::Activities,
            Reply::Ok(Response::Activities(vec![act(1, "Work", true)])),
        );

        let pick = |_prompt: &str, _items: &[String]| -> Result<PickerOutcome, CliError> {
            Ok(PickerOutcome::Cancelled)
        };

        run_picker(&mut client, pick).expect("cancellation is silent Ok");
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

        let err = run_picker(&mut client, pick).expect_err("wrong variant must fail");
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
