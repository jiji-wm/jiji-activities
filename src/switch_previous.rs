//! `switch-previous` subcommand: switch to the previously-active
//! activity (toggle behaviour).
//!
//! Dispatches `Action::SwitchActivityPrevious {}` over IPC and expects
//! `Response::Handled`. No name argument; no picker. The compositor
//! maintains the "previous activity" pointer internally — the CLI is a
//! one-shot trigger.
//!
//! ## Error model
//!
//! - `Reply::Ok(Response::Handled)` → `Ok(())`.
//! - `Reply::Ok(other)` →
//!   `CliError::MalformedResponse(WrongVariant { .. })` (exit 65).
//! - `Reply::Err("activity not found")` falls through to
//!   `CliError::MalformedResponse(Server(_))` (exit 65) rather than
//!   `ActivityNotFound` — there is no name in scope to attach to the
//!   carrier. This is the intended `send_expect_handled(_, _, None)`
//!   contract; see [`crate::ipc_helpers::send_expect_handled`].
//! - Other `Reply::Err(msg)` flows through the normal
//!   `MalformedResponse(Server)` mapping.
//! - Transport / decode errors flow through `IpcError → CliError`
//!   unchanged.
//!
//! The IPC call is wrapped with
//! `.context("switching to previous activity")` so the operation
//! surfaces in the stderr chain.

use anyhow::{Context, Result};
use niri_ipc::{Action, Request};

use crate::ipc::NiriClient;
use crate::ipc_helpers::send_expect_handled;

/// Issues `Request::Action(Action::SwitchActivityPrevious {})`.
///
/// **Contract:** exactly one IPC round-trip; `Response::Handled` ⇒
/// `Ok(())`; everything else routes through
/// [`send_expect_handled`] with `activity_name: None`. See module
/// docs for the full error matrix.
pub(crate) fn run(client: &mut dyn NiriClient) -> Result<()> {
    let req = Request::Action(Action::SwitchActivityPrevious {});
    send_expect_handled(client, req, None).context("switching to previous activity")
}

#[cfg(test)]
mod tests {
    use niri_ipc::{Action, Reply, Request, Response};

    use super::*;
    use crate::error::{CliError, MalformedResponseSource};
    use crate::ipc::MockClient;

    fn previous_req() -> Request {
        Request::Action(Action::SwitchActivityPrevious {})
    }

    #[test]
    fn switch_previous_dispatches_action_with_no_args() {
        // Pin the request shape: Action::SwitchActivityPrevious {} —
        // an empty-struct variant, not a sibling that happens to share
        // a name. MockClient's queue-equality already enforces this,
        // but a dedicated test pins the contract at a glance.
        let mut client = MockClient::new();
        client.expect(previous_req(), Reply::Ok(Response::Handled));
        run(&mut client).expect("happy path");
        client.assert_consumed_in_order();
    }

    #[test]
    fn switch_previous_handled_is_ok() {
        let mut client = MockClient::new();
        client.expect(previous_req(), Reply::Ok(Response::Handled));
        run(&mut client).expect("Response::Handled must succeed");
        client.assert_consumed_in_order();
    }

    #[test]
    fn switch_previous_wrong_variant_is_malformed() {
        let mut client = MockClient::new();
        client.expect(previous_req(), Reply::Ok(Response::Version("v".into())));
        let err = run(&mut client).expect_err("wrong variant must fail");
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
    fn switch_previous_server_error_routes_to_malformed_not_not_found() {
        // No activity name is in scope (the SwitchActivityPrevious variant
        // takes no args), so even the literal "activity not found" wire
        // string must route to MalformedResponse(Server) — fabricating an
        // ActivityNotFound("") would be wrong. Pin the carrier.
        let mut client = MockClient::new();
        client.expect(previous_req(), Err("activity not found".to_owned()));
        let err = run(&mut client).expect_err("not-found must surface");
        let cli_err = err
            .chain()
            .find_map(|e| e.downcast_ref::<CliError>())
            .expect("CliError must be in chain");
        match cli_err {
            CliError::MalformedResponse(MalformedResponseSource::Server(msg)) => {
                assert_eq!(msg, "activity not found");
            }
            other => {
                panic!("expected MalformedResponse(Server), not ActivityNotFound; got {other:?}",)
            }
        }
        assert_eq!(cli_err.exit_code(), 65);
        client.assert_consumed_in_order();
    }

    #[test]
    fn switch_previous_preserves_context_in_error_chain() {
        // `{err:#}` must include both the .context("switching to
        // previous activity") layer and the underlying server-error
        // leaf — proving .context() did not shadow the typed Display.
        let mut client = MockClient::new();
        client.expect(previous_req(), Err("some failure".to_owned()));
        let err = run(&mut client).expect_err("must fail");
        let formatted = format!("{err:#}");
        assert!(
            formatted.contains("switching to previous activity"),
            "context layer missing from chain: {formatted}",
        );
        assert!(
            formatted.contains("some failure"),
            "leaf error missing from chain: {formatted}",
        );
        client.assert_consumed_in_order();
    }
}
