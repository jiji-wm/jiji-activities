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
use niri_ipc::{Action, ActivityReferenceArg, Request};

use crate::cli::Order;
use crate::error::CliError;
use crate::ipc::NiriClient;
use crate::ipc_helpers::{names_for_switch, send_expect_activities, send_expect_handled};
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

/// Opens a single-select picker over the current activity list, then
/// dispatches `switch::run` against the chosen name.
///
/// **Contract:**
/// - Issues `Request::Activities` first.
/// - If the activity list is empty, writes a single-line diagnostic to
///   stderr (`jiji-activities: no activities configured; nothing to
///   switch to`) and returns `Ok(())` — exit 0. The picker is never
///   spawned because an empty menu is worse UX than a no-op; the
///   stderr line tells the user *why* nothing happened.
/// - If the activity list has exactly one entry (the active one), no
///   rows remain to switch to. A distinct diagnostic fires (`jiji-activities:
///   only the active activity exists; nothing to switch to`) and the
///   picker is not spawned.
/// - Otherwise builds a [`crate::ipc_helpers::SwitchMenu`] via
///   [`names_for_switch`] (using `order`) and calls `pick` with the
///   prompt `"Switch to activity:"`. When the active activity is known
///   it is appended as the **last** row, marked `"<name> (current)"` —
///   it gets its own line instead of stretching the prompt (fuzzel has
///   no second prompt line), while the first row (the previous
///   activity under MRU order) stays preselected. Selecting the marked
///   row dispatches a switch to the already-active activity, which the
///   compositor handles as a no-op (`Handled`).
/// - On `Selected(name)` delegates to [`run`] (second IPC call:
///   `Request::Action(SwitchActivity)`). A selection equal to the
///   marked current label maps back to the bare activity name first.
/// - On `Cancelled`, returns `Ok(())` — user dismissal is exit 0.
///
/// The `pick` parameter is a closure so unit tests can inject a stub
/// without spawning `fuzzel`; production wiring passes
/// [`picker::pick_one`].
pub(crate) fn run_picker<F>(client: &mut dyn NiriClient, order: Order, pick: F) -> Result<()>
where
    F: FnOnce(&str, &[String]) -> Result<PickerOutcome, CliError>,
{
    let activities = send_expect_activities(client).context("requesting activities")?;
    if activities.is_empty() {
        // Nothing to pick. Skip the picker spawn — an empty `fuzzel`
        // menu is worse UX than a no-op — and tell the user why nothing
        // happened so the silence is diagnosable.
        eprintln!("jiji-activities: no activities configured; nothing to switch to");
        return Ok(());
    }
    let menu = names_for_switch(&activities, order);
    if menu.rows.is_empty() {
        // Only one activity (the active one) — no other activity to switch
        // to. Emit a truthful diagnostic (there IS one activity, it is just
        // the currently-active one) and do not spawn the picker.
        eprintln!("jiji-activities: only the active activity exists; nothing to switch to");
        return Ok(());
    }
    // The current activity rides along as a marked last row rather than in
    // the prompt: fuzzel's prompt and input share one fixed-width line, so
    // long names would squeeze the typing area, while a row scales for
    // free. Last keeps the previous activity (row 0 under MRU) preselected.
    let mut rows = menu.rows;
    let current_label = menu.current.as_ref().map(|n| format!("{n} (current)"));
    if let Some(label) = &current_label {
        rows.push(label.clone());
    }
    match pick("Switch to activity:", &rows)? {
        PickerOutcome::Cancelled => Ok(()),
        PickerOutcome::Selected(selection) => {
            // Map the marked label back to the bare name. Exact-equality:
            // a hypothetical activity literally named "<current> (current)"
            // would collide, but the menu cannot render the two rows
            // distinctly anyway, and the collision resolves to a no-op
            // switch rather than a wrong switch.
            let name = match (&current_label, &menu.current) {
                (Some(label), Some(current)) if *label == selection => current.clone(),
                _ => selection,
            };
            run(client, &name)
        }
    }
}

#[cfg(test)]
mod tests {
    use niri_ipc::{Action, Activity, ActivityReferenceArg, Reply, Request, Response};

    use super::*;
    use crate::cli::Order;
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

    fn act_seq(id: u64, name: &str, is_active: bool, seq: u64) -> Activity {
        Activity {
            id,
            name: name.into(),
            is_active,
            is_config_declared: true,
            last_active_seq: seq,
            ..Default::default()
        }
    }

    #[test]
    fn run_picker_mru_marks_active_as_last_row_with_short_prompt() {
        // MRU order: Work is active (seq=2), Personal (seq=1) leads the rows.
        // The active activity rides along as a marked LAST row (its own line,
        // not in the prompt), keeping the previous activity preselected.
        let mut client = MockClient::new();
        client.expect(
            Request::Activities,
            Reply::Ok(Response::Activities(vec![
                act_seq(1, "Work", true, 2),
                act_seq(2, "Personal", false, 1),
            ])),
        );
        client.expect(switch_req("Personal"), Reply::Ok(Response::Handled));

        let pick = |prompt: &str, items: &[String]| -> Result<PickerOutcome, CliError> {
            // The prompt stays short; the current activity is a row instead.
            assert_eq!(prompt, "Switch to activity:");
            // Previous activity first (preselected); marked current last.
            assert_eq!(items, &["Personal".to_owned(), "Work (current)".to_owned()]);
            Ok(PickerOutcome::Selected("Personal".to_owned()))
        };

        run_picker(&mut client, Order::Mru, pick).expect("happy path succeeds");
        client.assert_consumed_in_order();
    }

    #[test]
    fn run_picker_marked_current_row_selection_dispatches_bare_name() {
        // Selecting the marked "<name> (current)" row must dispatch the bare
        // activity name on the wire (a no-op switch the compositor Handles),
        // not the decorated label.
        let mut client = MockClient::new();
        client.expect(
            Request::Activities,
            Reply::Ok(Response::Activities(vec![
                act_seq(1, "Work", true, 2),
                act_seq(2, "Personal", false, 1),
            ])),
        );
        client.expect(switch_req("Work"), Reply::Ok(Response::Handled));

        let pick = |_prompt: &str, _items: &[String]| -> Result<PickerOutcome, CliError> {
            Ok(PickerOutcome::Selected("Work (current)".to_owned()))
        };

        run_picker(&mut client, Order::Mru, pick).expect("marked-row selection succeeds");
        client.assert_consumed_in_order();
    }

    #[test]
    fn run_picker_static_order_marks_active_last_preserves_declaration_order() {
        // Static order: declaration order preserved for the non-active rows;
        // the marked current row is appended last, same as MRU.
        let mut client = MockClient::new();
        client.expect(
            Request::Activities,
            Reply::Ok(Response::Activities(vec![
                act_seq(1, "Work", true, 2),
                act_seq(2, "Personal", false, 1),
                act_seq(3, "Gaming", false, 0),
            ])),
        );
        client.expect(switch_req("Gaming"), Reply::Ok(Response::Handled));

        let pick = |prompt: &str, items: &[String]| -> Result<PickerOutcome, CliError> {
            assert_eq!(prompt, "Switch to activity:");
            // Static: declaration order minus active, then the marked row.
            assert_eq!(
                items,
                &[
                    "Personal".to_owned(),
                    "Gaming".to_owned(),
                    "Work (current)".to_owned()
                ]
            );
            Ok(PickerOutcome::Selected("Gaming".to_owned()))
        };

        run_picker(&mut client, Order::Static, pick).expect("static order happy path succeeds");
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

        run_picker(&mut client, Order::Mru, pick).expect("empty list exits Ok");
        client.assert_consumed_in_order();
    }

    #[test]
    fn run_picker_single_activity_warns_and_exits_zero() {
        // Single activity (the active one): after excluding it, rows is empty.
        // The picker must NOT be spawned; a truthful diagnostic fires naming
        // the single-activity (not the "no activities configured") case.
        //
        // This test pins the no-spawn + Ok(()) contract. The end-to-end
        // stderr assertion is pinned by the integration test in
        // `tests/picker_shim.rs` (`run_picker_single_activity_warns_and_exits_zero`).
        let mut client = MockClient::new();
        client.expect(
            Request::Activities,
            Reply::Ok(Response::Activities(vec![act_seq(1, "Work", true, 1)])),
        );

        let pick = |_prompt: &str, _items: &[String]| -> Result<PickerOutcome, CliError> {
            panic!("pick must not be called when only the active activity exists");
        };

        run_picker(&mut client, Order::Mru, pick).expect("single-activity exits Ok");
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
            Reply::Ok(Response::Activities(vec![
                act_seq(1, "Work", true, 2),
                act_seq(2, "Personal", false, 1),
            ])),
        );

        let pick = |_prompt: &str, _items: &[String]| -> Result<PickerOutcome, CliError> {
            Ok(PickerOutcome::Cancelled)
        };

        run_picker(&mut client, Order::Mru, pick).expect("cancellation is silent Ok");
        client.assert_consumed_in_order();
    }

    #[test]
    fn run_picker_no_active_activity_uses_bare_prompt_and_presents_all_rows() {
        // Degenerate compositor state: all activities are inactive (no
        // `is_active=true`). `names_for_switch` returns `current=None`, so
        // `run_picker` must use the bare prompt "Switch to activity:" and
        // must present all activity names in rows (nothing to exclude).
        let mut client = MockClient::new();
        client.expect(
            Request::Activities,
            Reply::Ok(Response::Activities(vec![
                act_seq(1, "Work", false, 2),
                act_seq(2, "Personal", false, 1),
            ])),
        );
        // Picker will cancel; no switch dispatch needed.

        let pick = |prompt: &str, items: &[String]| -> Result<PickerOutcome, CliError> {
            // No active activity → bare prompt, no "(current: …)" suffix.
            assert_eq!(prompt, "Switch to activity:");
            // Both activities appear in rows (none excluded).
            assert_eq!(items, &["Work".to_owned(), "Personal".to_owned()]);
            Ok(PickerOutcome::Cancelled)
        };

        run_picker(&mut client, Order::Mru, pick)
            .expect("no-active-activity degenerate state exits Ok on cancellation");
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

        let err = run_picker(&mut client, Order::Mru, pick).expect_err("wrong variant must fail");
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
