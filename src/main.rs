//! `niri-activities` — KDE-style Activities CLI for the niri compositor.
//!
//! `main` is intentionally thin: parse the CLI, dispatch, then map any
//! error back to a sysexits-style exit code. All real work lives in
//! [`cli::dispatch`] and the per-subcommand stubs.

use std::process::ExitCode;

use clap::Parser;

mod assign_workspace;
mod cli;
mod create;
mod error;
mod ipc;
mod ipc_helpers;
mod list;
mod move_window;
mod move_workspace;
mod picker;
mod remove;
mod save;
mod switch;
mod switch_previous;

fn main() -> ExitCode {
    let parsed = match cli::Cli::try_parse() {
        Ok(parsed) => parsed,
        Err(err) => {
            // clap prints its own message; we only decide the code.
            // `--help` / `--version` use stdout and exit 0; real
            // parse errors use stderr and exit 64 (`EX_USAGE`,
            // overriding clap's default of 2).
            let _ = err.print();
            return if err.use_stderr() {
                ExitCode::from(64)
            } else {
                ExitCode::SUCCESS
            };
        }
    };

    match cli::dispatch(parsed) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            // `OutputPipeClosed` means the stdout consumer closed its read
            // end (e.g. `niri-activities list | head -1`). This is not an
            // error — suppress the message and exit 0, matching standard
            // Unix tool behaviour.
            //
            // Precedence contract: `is_stdout_pipe_closed` matches only
            // `CliError::OutputPipeClosed`, which is produced exclusively
            // by stdout write / flush failures in `list::run`. IPC-layer
            // `BrokenPipe` (compositor crash mid-write) surfaces as
            // `CliError::SocketUnavailable` and is not suppressed.
            if is_stdout_pipe_closed(&err) {
                return ExitCode::SUCCESS;
            }
            // `{:#}` walks the full anyhow chain so context layers
            // added via `.context(...)` survive to stderr.
            eprintln!("niri-activities: {err:#}");
            ExitCode::from(u8::try_from(error::map_exit(&err)).unwrap_or(1))
        }
    }
}

/// Returns `true` when `err` carries a [`error::CliError::OutputPipeClosed`]
/// anywhere in its chain — indicating that a stdout write hit EPIPE and
/// the process should exit 0 silently.
///
/// Matching on the typed variant (rather than walking the chain for any
/// `io::Error` with `kind() == BrokenPipe`) ensures that IPC-transport
/// `BrokenPipe`s (compositor crash mid-write) are not incorrectly
/// suppressed.
fn is_stdout_pipe_closed(err: &anyhow::Error) -> bool {
    err.chain().any(|e| {
        matches!(
            e.downcast_ref::<error::CliError>(),
            Some(error::CliError::OutputPipeClosed)
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::CliError;

    #[test]
    fn is_stdout_pipe_closed_matches_output_pipe_closed() {
        let err: anyhow::Error = CliError::OutputPipeClosed.into();
        assert!(
            is_stdout_pipe_closed(&err),
            "bare OutputPipeClosed must be detected",
        );
    }

    #[test]
    fn is_stdout_pipe_closed_matches_through_context_wrap() {
        let err: anyhow::Error =
            anyhow::Error::from(CliError::OutputPipeClosed).context("flushing stdout");
        assert!(
            is_stdout_pipe_closed(&err),
            "OutputPipeClosed wrapped by .context() must still be detected",
        );
    }

    #[test]
    fn is_stdout_pipe_closed_does_not_match_raw_broken_pipe_io_error() {
        // A raw BrokenPipe io::Error (e.g. from the IPC transport layer)
        // must NOT trigger the suppression — only OutputPipeClosed does.
        let io_err = std::io::Error::new(std::io::ErrorKind::BrokenPipe, "ipc pipe");
        let err: anyhow::Error = io_err.into();
        assert!(
            !is_stdout_pipe_closed(&err),
            "raw BrokenPipe io::Error must not be suppressed",
        );
    }

    #[test]
    fn is_stdout_pipe_closed_does_not_match_socket_unavailable() {
        // SocketUnavailable wrapping a BrokenPipe io::Error (compositor
        // crash mid-write) must NOT be suppressed.
        let io_err = std::io::Error::new(std::io::ErrorKind::BrokenPipe, "socket broken");
        let err: anyhow::Error = CliError::SocketUnavailable(io_err).into();
        assert!(
            !is_stdout_pipe_closed(&err),
            "SocketUnavailable(BrokenPipe) must not be suppressed",
        );
    }

    #[test]
    fn is_stdout_pipe_closed_does_not_match_other_cli_errors() {
        let err: anyhow::Error = CliError::NotImplemented("test").into();
        assert!(
            !is_stdout_pipe_closed(&err),
            "unrelated CliError must not be suppressed",
        );
    }
}
