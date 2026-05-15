//! `remove` subcommand: remove a runtime activity by name.
//!
//! Dispatches `Action::RemoveActivity { activity: Name(name) }` over IPC
//! and expects `Response::Handled`.
//!
//! ## Why this module reuses [`send_expect_handled`]
//!
//! [`crate::ipc_helpers::send_expect_handled`] maps `"activity not found"`
//! → [`CliError::ActivityNotFound`] and routes all other server errors
//! through `MalformedResponse(Server)`. That mapping is exactly the
//! `remove` error matrix: `RemoveActivityError::NotFound` emits
//! `"activity not found"` on the wire, and every other
//! `RemoveActivityError` variant surfaces verbatim. No open-coded match
//! is needed here (contrast `create.rs`, which has no `NotFound` arm and
//! requires two additional string-equality branches).
//!
//! ## Error model
//!
//! - `Reply::Ok(Response::Handled)` → `Ok(())`.
//! - `Reply::Ok(other)` →
//!   `CliError::MalformedResponse(WrongVariant { .. })` (exit 65).
//! - `Reply::Err("activity not found")` →
//!   `CliError::ActivityNotFound(name)` (exit 66).
//! - All other failure modes — every `RemoveActivityError` variant other
//!   than `NotFound` (see the compositor's `RemoveActivityError::Display`
//!   impl for the canonical wire strings) — flow through to
//!   `CliError::MalformedResponse(Server(msg))` (exit 65) with the
//!   message verbatim. The compositor is the source of truth for the
//!   text; surfacing it unmolested keeps the user-facing diagnostic
//!   in sync as the compositor refines its tokens.
//! - Transport / decode errors flow through `IpcError → CliError`
//!   unchanged.
//!
//! The IPC call is wrapped with `.context("removing activity")` so the
//! operation surfaces in the stderr chain.

use anyhow::{Context, Result};
use niri_ipc::{Action, ActivityReferenceArg, Request};

use crate::ipc::NiriClient;
use crate::ipc_helpers::send_expect_handled;

/// Removes the runtime activity named `name`.
///
/// **Contract:** issues exactly one `RemoveActivity` IPC request
/// referencing the activity by name (not id) and expects
/// `Response::Handled`. See module docs for the full error matrix.
pub(crate) fn run(client: &mut dyn NiriClient, name: &str) -> Result<()> {
    let req = Request::Action(Action::RemoveActivity {
        activity: ActivityReferenceArg::Name(name.to_owned()),
    });
    send_expect_handled(client, req, Some(name)).context("removing activity")
}

#[cfg(test)]
mod tests {
    use niri_ipc::{Action, ActivityReferenceArg, Reply, Request, Response};

    use super::*;
    use crate::error::{CliError, MalformedResponseSource};
    use crate::ipc::MockClient;

    fn remove_req(name: &str) -> Request {
        Request::Action(Action::RemoveActivity {
            activity: ActivityReferenceArg::Name(name.to_owned()),
        })
    }

    #[test]
    fn remove_dispatches_action_with_name_arg() {
        // Pins two load-bearing fields:
        //  - activity carrier: ActivityReferenceArg::Name(_) (not Id)
        //  - request shape: Action::RemoveActivity { .. }
        // MockClient's queue-equality enforces both. A regression that
        // mis-built the carrier (Id vs Name) would fail here.
        let mut client = MockClient::new();
        client.expect(remove_req("Work"), Reply::Ok(Response::Handled));
        run(&mut client, "Work").expect("remove succeeds on Handled");
        client.assert_consumed_in_order();
    }

    #[test]
    fn remove_unknown_name_maps_to_activity_not_found_66() {
        let mut client = MockClient::new();
        client.expect(remove_req("Work"), Err("activity not found".to_owned()));
        let err = run(&mut client, "Work").expect_err("unknown activity must fail");
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
    fn remove_config_declared_routes_to_malformed_server() {
        // Pins the verbatim-passthrough contract: config-declared
        // refusals surface the compositor's wire string unmolested via
        // MalformedResponse(Server) (exit 65). Any future compositor
        // tweak to the wording propagates without a CLI change.
        let mut client = MockClient::new();
        client.expect(
            remove_req("Work"),
            Err("activity is config-declared; edit config and reload to remove".to_owned()),
        );
        let err = run(&mut client, "Work").expect_err("config-declared must fail");
        let cli_err = err
            .chain()
            .find_map(|e| e.downcast_ref::<CliError>())
            .expect("CliError must be in chain");
        match cli_err {
            CliError::MalformedResponse(MalformedResponseSource::Server(msg)) => {
                assert_eq!(
                    msg,
                    "activity is config-declared; edit config and reload to remove"
                );
            }
            other => panic!("expected MalformedResponse(Server), got {other:?}"),
        }
        assert_eq!(cli_err.exit_code(), 65);
        client.assert_consumed_in_order();
    }

    #[test]
    fn remove_last_activity_routes_to_malformed_server() {
        let mut client = MockClient::new();
        client.expect(
            remove_req("Work"),
            Err("cannot remove the last remaining activity".to_owned()),
        );
        let err = run(&mut client, "Work").expect_err("last-activity must fail");
        let cli_err = err
            .chain()
            .find_map(|e| e.downcast_ref::<CliError>())
            .expect("CliError must be in chain");
        match cli_err {
            CliError::MalformedResponse(MalformedResponseSource::Server(msg)) => {
                assert_eq!(msg, "cannot remove the last remaining activity");
            }
            other => panic!("expected MalformedResponse(Server), got {other:?}"),
        }
        assert_eq!(cli_err.exit_code(), 65);
        client.assert_consumed_in_order();
    }

    #[test]
    fn remove_workspace_has_windows_routes_to_malformed_server() {
        // Pins the verbatim-passthrough contract for the
        // ExclusiveWorkspaceHasWindows refusal: the compositor's wire string
        // surfaces unmolested via MalformedResponse(Server) (exit 65).
        let mut client = MockClient::new();
        client.expect(
            remove_req("Work"),
            Err(
                "activity owns an exclusive workspace with windows; close or move them first"
                    .to_owned(),
            ),
        );
        let err = run(&mut client, "Work").expect_err("workspace-has-windows must fail");
        let cli_err = err
            .chain()
            .find_map(|e| e.downcast_ref::<CliError>())
            .expect("CliError must be in chain");
        match cli_err {
            CliError::MalformedResponse(MalformedResponseSource::Server(msg)) => {
                assert_eq!(
                    msg,
                    "activity owns an exclusive workspace with windows; close or move them first",
                );
            }
            other => panic!("expected MalformedResponse(Server), got {other:?}"),
        }
        assert_eq!(cli_err.exit_code(), 65);
        client.assert_consumed_in_order();
    }

    #[test]
    fn remove_wrong_response_variant_is_malformed() {
        let mut client = MockClient::new();
        client.expect(remove_req("Work"), Reply::Ok(Response::Version("v".into())));
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
    fn remove_preserves_context_in_error_chain() {
        let mut client = MockClient::new();
        client.expect(remove_req("Work"), Err("activity not found".to_owned()));
        let err = run(&mut client, "Work").expect_err("must fail");
        let formatted = format!("{err:#}");
        assert!(
            formatted.contains("removing activity"),
            "context layer missing from chain: {formatted}",
        );
        assert!(
            formatted.contains("no such activity: Work"),
            "ActivityNotFound Display missing from chain: {formatted}",
        );
        client.assert_consumed_in_order();
    }
}
