//! `rename` subcommand: rename a runtime activity.
//!
//! Dispatches `Action::RenameActivity { activity: Name(target), name: new_name }`
//! over IPC and expects `Response::Handled`. Two entry points:
//!
//! - [`run`] — named-target path; used when `--activity <ref>` is supplied.
//! - [`run_picker`] — picker path; fetches the activity list first, then
//!   opens a single-select menu to choose the target, and delegates to
//!   [`run`] on selection.
//!
//! ## Error model
//!
//! [`run`] routes through [`send_expect_handled`] with the **target**
//! activity name as the `activity_name` carrier. This means:
//!
//! - `Reply::Ok(Response::Handled)` → `Ok(())`.
//! - `Reply::Ok(other)` →
//!   `CliError::MalformedResponse(WrongVariant { .. })` (exit 65).
//! - `Reply::Err("activity not found")` →
//!   `CliError::ActivityNotFound(target)` (exit 66).
//! - All other `Reply::Err(msg)` — including compositor refusals such as
//!   `"activity is config-declared; edit config and reload to rename"`,
//!   `"activity name must not be empty"`, and `"activity name already
//!   exists"` — flow through to `CliError::MalformedResponse(Server(msg))`
//!   (exit 65) verbatim. The compositor is the source of truth for those
//!   strings; surfacing them unmolested keeps the user-facing diagnostic
//!   in sync as the compositor refines its tokens.
//! - Transport / decode errors flow through `IpcError → CliError`
//!   unchanged.
//!
//! The IPC call is wrapped with `.context("renaming activity")` so the
//! operation surfaces in the stderr chain.

use anyhow::{Context, Result};
use niri_ipc::{Action, ActivityReferenceArg, Request};

use crate::error::CliError;
use crate::ipc::NiriClient;
use crate::ipc_helpers::{names_focused_first, send_expect_activities, send_expect_handled};
use crate::picker::PickerOutcome;

/// Renames the activity identified by `target` to `new_name`.
///
/// **Contract:** issues exactly one `RenameActivity` IPC request referencing
/// the activity by `target` and expects `Response::Handled`. The
/// `target_name_for_err` parameter is forwarded to [`send_expect_handled`] as
/// the `activity_name` carrier — it must be the target activity's name (not
/// the new name) so a `"activity not found"` wire error maps to
/// `ActivityNotFound(<target>)`.
///
/// See module docs for the full error matrix.
pub(crate) fn run(
    client: &mut dyn NiriClient,
    target: &ActivityReferenceArg,
    new_name: &str,
    target_name_for_err: Option<&str>,
) -> Result<()> {
    let req = Request::Action(Action::RenameActivity {
        activity: target.clone(),
        name: new_name.to_owned(),
    });
    send_expect_handled(client, req, target_name_for_err).context("renaming activity")
}

/// Opens a single-select picker over the current activity list, then
/// dispatches [`run`] against the chosen target with `new_name`.
///
/// **Contract:**
/// - Issues `Request::Activities` first.
/// - If the activity list is empty, writes a single-line diagnostic to
///   stderr (`jiji-activities: no activities configured; nothing to
///   rename`) and returns `Ok(())` — exit 0. The picker is never spawned.
/// - Otherwise reorders names with [`names_focused_first`] so the
///   currently-focused activity is the default highlight, calls `pick`,
///   and on `Selected(target)` delegates to [`run`] (which issues a second
///   IPC call: `Request::Action(RenameActivity)`).
/// - On `Cancelled`, returns `Ok(())` — user dismissal is exit 0.
///
/// The `pick` parameter is a closure so unit tests can inject a stub
/// without spawning `fuzzel`; production wiring passes
/// [`crate::picker::pick_one`].
pub(crate) fn run_picker<F>(client: &mut dyn NiriClient, new_name: &str, pick: F) -> Result<()>
where
    F: FnOnce(&str, &[String]) -> Result<PickerOutcome, CliError>,
{
    let activities = send_expect_activities(client).context("requesting activities")?;
    if activities.is_empty() {
        eprintln!("jiji-activities: no activities configured; nothing to rename");
        return Ok(());
    }
    let names = names_focused_first(&activities);
    match pick("Rename activity:", &names)? {
        PickerOutcome::Cancelled => Ok(()),
        PickerOutcome::Selected(target) => run(
            client,
            &ActivityReferenceArg::Name(target.clone()),
            new_name,
            Some(&target),
        ),
    }
}

#[cfg(test)]
mod tests {
    use niri_ipc::{Action, Activity, ActivityReferenceArg, Reply, Request, Response};

    use super::*;
    use crate::error::{CliError, MalformedResponseSource};
    use crate::ipc::MockClient;

    fn rename_req(target: &str, new_name: &str) -> Request {
        Request::Action(Action::RenameActivity {
            activity: ActivityReferenceArg::Name(target.to_owned()),
            name: new_name.to_owned(),
        })
    }

    fn make_target(target: &str) -> ActivityReferenceArg {
        ActivityReferenceArg::Name(target.to_owned())
    }

    #[test]
    fn rename_dispatches_action_with_name_and_target() {
        // Pins two load-bearing fields:
        //  - activity carrier: ActivityReferenceArg::Name(target) — NOT the new name
        //  - name field: the new name string
        // MockClient's queue-equality enforces the full request shape. A
        // regression that swapped target/new_name in the Action construction
        // would fail here.
        let mut client = MockClient::new();
        client.expect(rename_req("Work", "Work2"), Reply::Ok(Response::Handled));
        run(&mut client, &make_target("Work"), "Work2", Some("Work"))
            .expect("rename succeeds on Handled");
        client.assert_consumed_in_order();
    }

    #[test]
    fn rename_unknown_target_maps_to_activity_not_found_66() {
        // Pins the carrier contract: the target name, not the new name,
        // is passed as `activity_name` so a "activity not found" wire error
        // maps to ActivityNotFound(<target>).
        let mut client = MockClient::new();
        client.expect(
            rename_req("Work", "Renamed"),
            Err("activity not found".to_owned()),
        );
        let err = run(&mut client, &make_target("Work"), "Renamed", Some("Work"))
            .expect_err("unknown target must fail");
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
    fn rename_new_name_already_exists_routes_to_malformed_server() {
        // Pins the resolved error model: "activity name already exists" does
        // NOT map to CantCreate(73) — it flows through to
        // MalformedResponse(Server) (exit 65) verbatim. The `send_expect_handled`
        // seam only special-cases "activity not found"; everything else is
        // verbatim passthrough.
        let mut client = MockClient::new();
        client.expect(
            rename_req("Work", "Personal"),
            Err("activity name already exists".to_owned()),
        );
        let err = run(&mut client, &make_target("Work"), "Personal", Some("Work"))
            .expect_err("name-collision must fail");
        let cli_err = err
            .chain()
            .find_map(|e| e.downcast_ref::<CliError>())
            .expect("CliError must be in chain");
        match cli_err {
            CliError::MalformedResponse(MalformedResponseSource::Server(msg)) => {
                assert_eq!(msg, "activity name already exists");
            }
            other => panic!("expected MalformedResponse(Server), got {other:?}"),
        }
        assert_eq!(cli_err.exit_code(), 65);
        client.assert_consumed_in_order();
    }

    #[test]
    fn rename_config_declared_routes_to_malformed_server() {
        // Config-declared activities cannot be renamed via IPC; the
        // compositor returns this wire string verbatim. Exit 65.
        let mut client = MockClient::new();
        client.expect(
            rename_req("Work", "NewWork"),
            Err("activity is config-declared; edit config and reload to rename".to_owned()),
        );
        let err = run(&mut client, &make_target("Work"), "NewWork", Some("Work"))
            .expect_err("config-declared must fail");
        let cli_err = err
            .chain()
            .find_map(|e| e.downcast_ref::<CliError>())
            .expect("CliError must be in chain");
        match cli_err {
            CliError::MalformedResponse(MalformedResponseSource::Server(msg)) => {
                assert_eq!(
                    msg,
                    "activity is config-declared; edit config and reload to rename"
                );
            }
            other => panic!("expected MalformedResponse(Server), got {other:?}"),
        }
        assert_eq!(cli_err.exit_code(), 65);
        client.assert_consumed_in_order();
    }

    #[test]
    fn rename_wrong_response_variant_is_malformed() {
        let mut client = MockClient::new();
        client.expect(
            rename_req("Work", "NewWork"),
            Reply::Ok(Response::Version("v".into())),
        );
        let err = run(&mut client, &make_target("Work"), "NewWork", Some("Work"))
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
    fn rename_preserves_context_in_error_chain() {
        // `{err:#}` must include both the .context("renaming activity")
        // layer and the ActivityNotFound Display leaf — proving .context()
        // did not shadow the typed CliError's Display.
        let mut client = MockClient::new();
        client.expect(
            rename_req("Work", "NewWork"),
            Err("activity not found".to_owned()),
        );
        let err =
            run(&mut client, &make_target("Work"), "NewWork", Some("Work")).expect_err("must fail");
        let formatted = format!("{err:#}");
        assert!(
            formatted.contains("renaming activity"),
            "context layer missing from chain: {formatted}",
        );
        assert!(
            formatted.contains("no such activity: Work"),
            "ActivityNotFound Display missing from chain: {formatted}",
        );
        client.assert_consumed_in_order();
    }

    // ---- run_picker --------------------------------------------------------

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
    fn run_picker_selects_target_and_dispatches_rename() {
        // Two IPC calls: Activities (for the menu), then RenameActivity
        // (after picker returns Selected). MockClient pins the two-call
        // order and the exact request shape.
        let mut client = MockClient::new();
        client.expect(
            Request::Activities,
            Reply::Ok(Response::Activities(vec![
                act(1, "Work", true),
                act(2, "Personal", false),
            ])),
        );
        client.expect(
            rename_req("Personal", "Social"),
            Reply::Ok(Response::Handled),
        );

        let pick = |prompt: &str, items: &[String]| -> Result<PickerOutcome, CliError> {
            assert_eq!(prompt, "Rename activity:");
            // Focused-first reordering: Work (focused) precedes Personal.
            assert_eq!(items, &["Work".to_owned(), "Personal".to_owned()]);
            Ok(PickerOutcome::Selected("Personal".to_owned()))
        };

        run_picker(&mut client, "Social", pick).expect("happy path succeeds");
        client.assert_consumed_in_order();
    }

    #[test]
    fn run_picker_empty_activities_warns_and_exits_zero() {
        // Empty activity list: exactly one IPC call (Activities), no
        // pick invocation, no RenameActivity dispatch. Exits Ok.
        let mut client = MockClient::new();
        client.expect(Request::Activities, Reply::Ok(Response::Activities(vec![])));

        let pick = |_prompt: &str, _items: &[String]| -> Result<PickerOutcome, CliError> {
            panic!("pick must not be called when activity list is empty");
        };

        run_picker(&mut client, "NewName", pick).expect("empty list exits Ok");
        client.assert_consumed_in_order();
    }

    #[test]
    fn run_picker_cancellation_skips_rename_dispatch() {
        // User dismisses the menu → no RenameActivity IPC call. Only one
        // queued reply (Activities); if `run_picker` dispatched a Rename
        // the MockClient would panic on unexpected request.
        let mut client = MockClient::new();
        client.expect(
            Request::Activities,
            Reply::Ok(Response::Activities(vec![act(1, "Work", true)])),
        );

        let pick = |_prompt: &str, _items: &[String]| -> Result<PickerOutcome, CliError> {
            Ok(PickerOutcome::Cancelled)
        };

        run_picker(&mut client, "NewName", pick).expect("cancellation is silent Ok");
        client.assert_consumed_in_order();
    }
}
