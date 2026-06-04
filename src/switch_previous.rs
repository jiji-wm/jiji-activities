//! `switch-previous` subcommand: switch to a previously-active
//! activity by depth in the recency list.
//!
//! Dispatches `Action::SwitchActivityPrevious { depth }` over IPC and
//! expects `Response::Handled`. The `depth` value (default: 1) controls
//! how many steps back in the compositor's activity recency list to step:
//! `depth=1` is the immediately-previous activity (toggle behaviour),
//! `depth=2` is one further back, and so on.
//!
//! Two edge cases are handled entirely compositor-side and never surface
//! as errors here:
//! - `depth=0` is a no-op — the compositor switches to the currently-active
//!   activity (identity switch). `Response::Handled` is returned as usual.
//! - Out-of-range depth (larger than the number of entries in the recency
//!   list) is clamped compositor-side to the oldest-activated activity in
//!   the list. The CLI forwards `depth` verbatim; clamping is transparent.
//!
//! No name argument; no picker. The compositor maintains the activity
//! recency list internally.
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

/// Issues `Request::Action(Action::SwitchActivityPrevious { depth })`.
///
/// `depth` is forwarded verbatim to the compositor, which resolves it
/// against its internal recency list. `depth=1` is the toggle behaviour
/// (immediately-previous activity); higher values step further back.
/// `depth=0` is a no-op (switches to the currently-active activity).
/// Out-of-range depth is clamped compositor-side to the oldest-activated
/// activity — never an error.
///
/// **Contract:** exactly one IPC round-trip; `Response::Handled` ⇒
/// `Ok(())`; everything else routes through
/// [`send_expect_handled`] with `activity_name: None`. See module
/// docs for the full error matrix.
pub(crate) fn run(client: &mut dyn NiriClient, depth: u32) -> Result<()> {
    let req = Request::Action(Action::SwitchActivityPrevious { depth });
    send_expect_handled(client, req, None).context("switching to previous activity")
}

#[cfg(test)]
mod tests {
    use niri_ipc::{Action, Reply, Request, Response};

    use super::*;
    use crate::error::{CliError, MalformedResponseSource};
    use crate::ipc::MockClient;

    fn previous_req_depth(depth: u32) -> Request {
        Request::Action(Action::SwitchActivityPrevious { depth })
    }

    fn previous_req() -> Request {
        previous_req_depth(1)
    }

    #[test]
    fn switch_previous_default_depth_is_one() {
        // Pin the request shape: Action::SwitchActivityPrevious { depth: 1 }
        // for the toggle-to-previous semantics. MockClient's queue-equality
        // already enforces this, but a dedicated test pins the contract at
        // a glance.
        let mut client = MockClient::new();
        client.expect(previous_req(), Reply::Ok(Response::Handled));
        run(&mut client, 1).expect("happy path");
        client.assert_consumed_in_order();
    }

    #[test]
    fn switch_previous_depth_n_forwards_depth_on_wire() {
        // Non-default depth: the helper must forward the caller's value
        // to the compositor verbatim. A hard-coded depth:1 regression
        // would be caught by MockClient's request equality check.
        let mut client = MockClient::new();
        client.expect(previous_req_depth(3), Reply::Ok(Response::Handled));
        run(&mut client, 3).expect("depth=3 happy path");
        client.assert_consumed_in_order();
    }

    #[test]
    fn switch_previous_handled_is_ok() {
        let mut client = MockClient::new();
        client.expect(previous_req(), Reply::Ok(Response::Handled));
        run(&mut client, 1).expect("Response::Handled must succeed");
        client.assert_consumed_in_order();
    }

    #[test]
    fn switch_previous_wrong_variant_is_malformed() {
        let mut client = MockClient::new();
        client.expect(previous_req(), Reply::Ok(Response::Version("v".into())));
        let err = run(&mut client, 1).expect_err("wrong variant must fail");
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
        let err = run(&mut client, 1).expect_err("not-found must surface");
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
        let err = run(&mut client, 1).expect_err("must fail");
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
