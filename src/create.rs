//! `create` subcommand: create a new runtime activity by name.
//!
//! Dispatches `Action::CreateActivity { name }` over IPC and expects
//! `Response::Handled`.
//!
//! ## Why this module does not use [`send_expect_handled`]
//!
//! [`crate::ipc_helpers::send_expect_handled`] hard-codes the
//! `"activity not found"` → [`CliError::ActivityNotFound`] mapping. The
//! `create` error matrix has no `"activity not found"` arm — by
//! definition the name does not refer to an existing activity. Reusing
//! the helper would either misroute the unrelated `"activity name
//! already exists"` and `"activity name must not be empty"` strings
//! through the helper's catch-all (correct by accident, but obscures
//! intent) or require teaching the helper about a second carrier
//! (overfitting). The match is therefore open-coded here.
//!
//! ## Error model
//!
//! - `Reply::Ok(Response::Handled)` → `Ok(())`.
//! - `Reply::Ok(other)` →
//!   `CliError::MalformedResponse(WrongVariant { .. })` (exit 65).
//! - `Reply::Err("activity name already exists")` →
//!   `CliError::CantCreate("activity \"{name}\" already exists")`
//!   (exit 73). The user-supplied name is quote-wrapped so names with
//!   embedded whitespace remain unambiguous in stderr.
//! - `Reply::Err("activity name must not be empty")` →
//!   `CliError::Usage("activity name must not be empty")` (exit 64).
//!   Clap accepts non-empty `String` arguments but does not strip
//!   whitespace; an invocation like `create "   "` could reach the
//!   wire. The compositor's wire string is the source of truth for
//!   what counts as "empty"; the CLI does not pre-validate.
//! - Other `Reply::Err(msg)` →
//!   `CliError::MalformedResponse(Server(msg))` (exit 65).
//! - Transport / decode errors flow through `IpcError → CliError`
//!   unchanged.
//!
//! The IPC call is wrapped with `.context("creating activity")` so the
//! operation surfaces in the stderr chain.

use anyhow::{Context, Result};
use niri_ipc::{Action, Request, Response};

use crate::error::{CliError, MalformedResponseSource};
use crate::ipc::{IpcError, NiriClient};
use crate::ipc_helpers::variant_name;

/// Dispatches `Action::CreateActivity { name }` and maps the compositor's
/// wire-error matrix to `CliError`. See module docs for the full error
/// matrix.
///
/// Shared core for [`run`] (the standalone `create` verb) and
/// [`crate::move_window::create_activity_via_ipc`] (a step of the
/// move-window picker's stage-1 new-activity pipeline). Callers attach
/// their own `.context(...)` label — this function does not.
pub(crate) fn dispatch(client: &mut dyn NiriClient, name: &str) -> Result<()> {
    let req = Request::Action(Action::CreateActivity {
        name: name.to_owned(),
    });
    // The explicit type annotation is required: rustc cannot infer the `Ok`
    // variant's type from `?`-propagation alone (E0282) when the match arms
    // produce heterogeneous `Err` forms through `.into()`.
    let result: anyhow::Result<()> = match client.send(req) {
        Ok(Response::Handled) => Ok(()),
        Ok(other) => Err(
            CliError::MalformedResponse(MalformedResponseSource::WrongVariant {
                expected: "Response::Handled",
                got: variant_name(&other).into(),
            })
            .into(),
        ),
        Err(IpcError::Server(msg)) if msg == "activity name already exists" => {
            Err(CliError::CantCreate(format!("activity \"{name}\" already exists")).into())
        }
        Err(IpcError::Server(msg)) if msg == "activity name must not be empty" => {
            Err(CliError::Usage("activity name must not be empty".to_owned()).into())
        }
        Err(other) => Err(CliError::from(other).into()),
    };
    result
}

/// Creates a new runtime activity named `name`.
///
/// **Contract:** issues exactly one `CreateActivity` IPC request and
/// expects `Response::Handled`. See module docs for the full error
/// matrix.
pub(crate) fn run(client: &mut dyn NiriClient, name: &str) -> Result<()> {
    dispatch(client, name).context("creating activity")
}

#[cfg(test)]
mod tests {
    use niri_ipc::{Action, Reply, Request, Response};

    use super::*;
    use crate::error::{CliError, MalformedResponseSource};
    use crate::ipc::MockClient;

    fn create_req(name: &str) -> Request {
        Request::Action(Action::CreateActivity {
            name: name.to_owned(),
        })
    }

    #[test]
    fn create_dispatches_action_with_name_arg() {
        // Pins the request shape: Action::CreateActivity { name: _ } with
        // the user-supplied name verbatim. MockClient's queue-equality
        // catches a regression that built the wrong variant.
        let mut client = MockClient::new();
        client.expect(create_req("Work"), Reply::Ok(Response::Handled));
        run(&mut client, "Work").expect("create succeeds on Handled");
        client.assert_consumed_in_order();
    }

    #[test]
    fn create_handled_is_ok() {
        let mut client = MockClient::new();
        client.expect(create_req("Work"), Reply::Ok(Response::Handled));
        run(&mut client, "Work").expect("Response::Handled must succeed");
        client.assert_consumed_in_order();
    }

    #[test]
    fn create_name_already_exists_maps_to_cant_create_73() {
        let mut client = MockClient::new();
        client.expect(
            create_req("Work"),
            Err("activity name already exists".to_owned()),
        );
        let err = run(&mut client, "Work").expect_err("collision must fail");
        let cli_err = err
            .chain()
            .find_map(|e| e.downcast_ref::<CliError>())
            .expect("CliError must be in chain");
        match cli_err {
            CliError::CantCreate(msg) => {
                // Quote-wrapped name keeps whitespace-bearing names legible
                // in stderr.
                assert!(
                    msg.contains("\"Work\""),
                    "CantCreate message must quote-wrap the name; got: {msg}",
                );
                assert!(
                    msg.contains("already exists"),
                    "CantCreate message must explain the cause; got: {msg}",
                );
            }
            other => panic!("expected CantCreate, got {other:?}"),
        }
        assert_eq!(cli_err.exit_code(), 73);
        client.assert_consumed_in_order();
    }

    #[test]
    fn create_name_must_not_be_empty_maps_to_usage_64() {
        // The compositor's wire string is the source of truth for
        // "empty" — the CLI does not pre-validate. A `create "   "`
        // invocation (clap accepts it) reaches this arm.
        let mut client = MockClient::new();
        client.expect(
            create_req("   "),
            Err("activity name must not be empty".to_owned()),
        );
        let err = run(&mut client, "   ").expect_err("empty-name must fail");
        let cli_err = err
            .chain()
            .find_map(|e| e.downcast_ref::<CliError>())
            .expect("CliError must be in chain");
        match cli_err {
            CliError::Usage(msg) => {
                assert_eq!(msg, "activity name must not be empty");
            }
            other => panic!("expected Usage, got {other:?}"),
        }
        assert_eq!(cli_err.exit_code(), 64);
        client.assert_consumed_in_order();
    }

    #[test]
    fn create_name_already_exists_with_suffix_routes_to_malformed() {
        // Pins strict-equality on the "activity name already exists" match
        // arm. If the compositor ever adds the name to the message (e.g.,
        // "activity name already exists: Work"), the CLI must NOT route it
        // to CantCreate — it must fall through to MalformedResponse(Server).
        let mut client = MockClient::new();
        client.expect(
            create_req("Work"),
            Err("activity name already exists: Work".to_owned()),
        );
        let err = run(&mut client, "Work").expect_err("suffixed string must not match CantCreate");
        let cli_err = err
            .chain()
            .find_map(|e| e.downcast_ref::<CliError>())
            .expect("CliError must be in chain");
        match cli_err {
            CliError::MalformedResponse(MalformedResponseSource::Server(_)) => {}
            other => panic!("expected MalformedResponse(Server), not CantCreate; got {other:?}"),
        }
        assert_eq!(cli_err.exit_code(), 65);
        client.assert_consumed_in_order();
    }

    #[test]
    fn create_name_must_not_be_empty_capitalized_routes_to_malformed() {
        // Pins strict-equality on the "activity name must not be empty" match
        // arm. A capitalized variant ("Activity name must not be empty") must
        // NOT route to Usage — it must fall through to MalformedResponse(Server).
        let mut client = MockClient::new();
        client.expect(
            create_req("   "),
            Err("Activity name must not be empty".to_owned()),
        );
        let err = run(&mut client, "   ").expect_err("capitalized string must not match Usage");
        let cli_err = err
            .chain()
            .find_map(|e| e.downcast_ref::<CliError>())
            .expect("CliError must be in chain");
        match cli_err {
            CliError::MalformedResponse(MalformedResponseSource::Server(_)) => {}
            other => panic!("expected MalformedResponse(Server), not Usage; got {other:?}"),
        }
        assert_eq!(cli_err.exit_code(), 65);
        client.assert_consumed_in_order();
    }

    #[test]
    fn create_other_server_error_routes_to_malformed_response() {
        let mut client = MockClient::new();
        client.expect(create_req("Work"), Err("some other failure".to_owned()));
        let err = run(&mut client, "Work").expect_err("server error must surface");
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
        assert_eq!(cli_err.exit_code(), 65);
        client.assert_consumed_in_order();
    }

    #[test]
    fn create_wrong_response_variant_is_malformed() {
        let mut client = MockClient::new();
        client.expect(create_req("Work"), Reply::Ok(Response::Version("v".into())));
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
    fn create_preserves_context_in_error_chain() {
        // {err:#} must include both the .context("creating activity")
        // layer and the CantCreate Display output.
        let mut client = MockClient::new();
        client.expect(
            create_req("Work"),
            Err("activity name already exists".to_owned()),
        );
        let err = run(&mut client, "Work").expect_err("must fail");
        let formatted = format!("{err:#}");
        assert!(
            formatted.contains("creating activity"),
            "context layer missing from chain: {formatted}",
        );
        assert!(
            formatted.contains("\"Work\""),
            "leaf CantCreate (with quoted name) missing from chain: {formatted}",
        );
        client.assert_consumed_in_order();
    }
}
