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
use niri_ipc::{Action, ActivityReferenceArg, Request, Response};

use crate::error::{CliError, MalformedResponseSource};
use crate::ipc::{IpcError, NiriClient};

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
    send_expect_handled(client, req, name).context("switching activity")
}

/// Sends `req`, expects `Response::Handled`, and routes the
/// compositor's `"activity not found"` wire string to the typed
/// [`CliError::ActivityNotFound`].
///
/// `activity_name` is forwarded into the typed not-found error so the
/// stderr message names the activity the caller asked for. Other
/// `IpcError::Server(msg)` payloads fall through to the existing
/// `IpcError` → `CliError` mapping (which routes to
/// `MalformedResponse(Server)`).
fn send_expect_handled(
    client: &mut dyn NiriClient,
    req: Request,
    activity_name: &str,
) -> Result<()> {
    match client.send(req) {
        Ok(Response::Handled) => Ok(()),
        Ok(other) => Err(
            CliError::MalformedResponse(MalformedResponseSource::WrongVariant {
                expected: "Response::Handled",
                got: variant_name(&other).into(),
            })
            .into(),
        ),
        Err(IpcError::Server(msg)) if msg == "activity not found" => {
            Err(CliError::ActivityNotFound(activity_name.to_owned()).into())
        }
        Err(other) => Err(CliError::from(other).into()),
    }
}

/// Static variant name for `Response`. Mirrors the helper in
/// [`crate::list`]; kept local rather than shared to avoid widening
/// `list`'s public surface for a single re-use.
fn variant_name(r: &Response) -> &'static str {
    match r {
        Response::Handled => "Response::Handled",
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

#[cfg(test)]
mod tests {
    use niri_ipc::{Action, ActivityReferenceArg, Reply, Request, Response};

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
}
