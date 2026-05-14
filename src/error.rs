//! Typed CLI errors and exit-code mapping.
//!
//! Every fallible CLI path that wants a non-1 exit code returns a
//! [`CliError`]. `main()` downcasts the top-level [`anyhow::Error`] to
//! recover the variant and translate it via [`CliError::exit_code`];
//! anything that isn't a `CliError` falls through to exit code 1.
//!
//! Exit codes follow `<sysexits.h>` (BSD) where possible:
//!
//! | Variant              | Code | Meaning                                 |
//! | -------------------- | ---- | --------------------------------------- |
//! | `Usage`              |   64 | argument-parse failure (clap or our own) |
//! | `MalformedResponse`  |   65 | compositor returned an unexpected shape  |
//! | `ActivityNotFound`   |   66 | named activity does not exist            |
//! | `SocketUnavailable`  |   69 | `$NIRI_SOCKET` unreachable / IPC failed  |
//! | `NotImplemented`     |   70 | subcommand stub not yet wired            |
//! | `CantCreate`         |   73 | `create`/`save` could not produce target |

use std::fmt;

/// Source carrier for [`CliError::MalformedResponse`].
///
/// The compositor can return a malformed reply in two distinct ways:
/// the line on the wire fails to deserialize as a `Reply` (`Decode`),
/// or the `Reply::Err(String)` arm fires with a server-supplied error
/// message (`Server`). Keeping these as typed sources rather than
/// stringifying eagerly lets `Display` differentiate the two and
/// preserves the underlying `serde_json::Error` chain through
/// [`std::error::Error::source`].
#[derive(Debug)]
pub(crate) enum MalformedResponseSource {
    /// The reply line did not deserialize as a `Reply`.
    Decode(serde_json::Error),
    /// The compositor responded with `Reply::Err(String)`. The string
    /// is opaque from our side; we surface it verbatim.
    Server(String),
}

impl fmt::Display for MalformedResponseSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MalformedResponseSource::Decode(e) => write!(f, "decode error: {e}"),
            MalformedResponseSource::Server(msg) => write!(f, "server error: {msg}"),
        }
    }
}

/// Errors with an associated exit code that `main()` propagates verbatim.
///
/// Carriers are kept minimal and typed so the variants are a stable
/// contract — adding a new failure mode is a deliberate decision, not
/// a drive-by `anyhow::bail!`.
//
// `dead_code` is allowed because the typed contract is established
// here ahead of the call sites that produce each variant. The IPC
// client, picker, and list-output work will consume the remaining
// variants; removing this allowance early would force premature
// placeholders. Remove this attribute once every variant has at least
// one production call site.
#[allow(dead_code)]
#[derive(Debug)]
pub(crate) enum CliError {
    /// User-facing argument-parse failure. Triggered when clap rejects
    /// the invocation (unknown subcommand, missing required arg, etc.)
    /// or when our own dispatch validates an arg combination clap
    /// could not. Exit code 64 (`EX_USAGE`).
    Usage(String),

    /// Compositor responded with a payload whose shape we did not
    /// expect (wrong `Response` variant, unparseable inner JSON,
    /// or a `Reply::Err(String)` from the server). Exit code 65
    /// (`EX_DATAERR`). The typed [`MalformedResponseSource`] keeps
    /// the underlying `serde_json::Error` reachable via
    /// [`std::error::Error::source`] when it applies.
    MalformedResponse(MalformedResponseSource),

    /// Subcommand referenced an activity name that does not exist in
    /// the compositor's current `Activities` snapshot. Exit code 66
    /// (`EX_NOINPUT`).
    ActivityNotFound(String),

    /// `$NIRI_SOCKET` is unset, the socket file is missing, or the IPC
    /// round-trip failed at the transport layer. Distinct from
    /// [`Self::MalformedResponse`] so the user can tell "niri isn't
    /// running" from "niri is running but spoke gibberish."
    /// Exit code 69 (`EX_UNAVAILABLE`). The typed `io::Error` source
    /// remains reachable via [`std::error::Error::source`].
    SocketUnavailable(std::io::Error),

    /// Subcommand stub has not been wired to its IPC call yet.
    /// Carries the subcommand name so the stderr message names the
    /// gap. Exit code 70 (`EX_SOFTWARE`).
    NotImplemented(&'static str),

    /// `create` or `save` could not produce the requested activity
    /// (name collision, compositor refused). Exit code 73
    /// (`EX_CANTCREAT`).
    CantCreate(String),
}

impl CliError {
    /// Returns the process exit code associated with this variant.
    ///
    /// All values fit in `u8` (max sysexits code is 78); see
    /// [`map_exit`] for the narrowing cast at the `main()` boundary.
    pub(crate) fn exit_code(&self) -> i32 {
        match self {
            CliError::Usage(_) => 64,
            CliError::MalformedResponse(_) => 65,
            CliError::ActivityNotFound(_) => 66,
            CliError::SocketUnavailable(_) => 69,
            CliError::NotImplemented(_) => 70,
            CliError::CantCreate(_) => 73,
        }
    }
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CliError::Usage(msg) => write!(f, "usage: {msg}"),
            CliError::MalformedResponse(src) => write!(f, "malformed compositor response: {src}"),
            CliError::ActivityNotFound(name) => write!(f, "no such activity: {name}"),
            CliError::SocketUnavailable(io) => write!(f, "niri socket unavailable: {io}"),
            CliError::NotImplemented(name) => write!(f, "subcommand not yet implemented: {name}"),
            CliError::CantCreate(msg) => write!(f, "cannot create activity: {msg}"),
        }
    }
}

impl std::error::Error for CliError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            CliError::SocketUnavailable(io) => Some(io),
            CliError::MalformedResponse(MalformedResponseSource::Decode(e)) => Some(e),
            // `Server(String)` has no further source — the string IS the leaf.
            CliError::MalformedResponse(MalformedResponseSource::Server(_)) => None,
            CliError::Usage(_)
            | CliError::ActivityNotFound(_)
            | CliError::NotImplemented(_)
            | CliError::CantCreate(_) => None,
        }
    }
}

/// Maps a top-level [`anyhow::Error`] to a process exit code.
///
/// If any error in the chain is a [`CliError`], its
/// [`exit_code`](CliError::exit_code) is returned. Anything else
/// falls through to 1 — un-typed errors are treated as generic
/// failure so callers can rely on a non-zero exit without a
/// matching variant.
///
/// Walking the full chain (via [`anyhow::Error::chain`]) means a
/// `CliError` wrapped by `.context("…")` still produces its typed
/// code rather than silently falling through to 1.
pub(crate) fn map_exit(err: &anyhow::Error) -> i32 {
    err.chain()
        .find_map(|e| e.downcast_ref::<CliError>())
        .map(CliError::exit_code)
        .unwrap_or(1)
}

#[cfg(test)]
mod tests {
    use std::io;

    use super::*;

    fn sample_io_error() -> io::Error {
        io::Error::new(io::ErrorKind::NotFound, "ENOENT")
    }

    fn sample_decode_error() -> serde_json::Error {
        serde_json::from_str::<()>("not json")
            .expect_err("malformed JSON must fail to deserialize as ()")
    }

    #[test]
    fn usage_is_64() {
        assert_eq!(CliError::Usage("nope".into()).exit_code(), 64);
    }

    #[test]
    fn malformed_response_is_65() {
        assert_eq!(
            CliError::MalformedResponse(MalformedResponseSource::Decode(sample_decode_error()))
                .exit_code(),
            65,
        );
    }

    #[test]
    fn activity_not_found_is_66() {
        assert_eq!(CliError::ActivityNotFound("work".into()).exit_code(), 66);
    }

    #[test]
    fn socket_unavailable_is_69() {
        assert_eq!(
            CliError::SocketUnavailable(sample_io_error()).exit_code(),
            69,
        );
    }

    #[test]
    fn not_implemented_is_70() {
        assert_eq!(CliError::NotImplemented("switch").exit_code(), 70);
    }

    #[test]
    fn cant_create_is_73() {
        assert_eq!(CliError::CantCreate("dup name".into()).exit_code(), 73);
    }

    #[test]
    fn untyped_fallback_is_1() {
        let err = anyhow::anyhow!("untyped");
        assert_eq!(map_exit(&err), 1);
    }

    #[test]
    fn socket_unavailable_survives_context_wrap() {
        // A CliError wrapped by .context("…") must still produce its typed
        // exit code — not fall through to 1. This pins the chain-walk
        // contract before any real IPC layer exists to exercise it.
        let base: anyhow::Error = CliError::SocketUnavailable(sample_io_error()).into();
        let wrapped = base.context("connecting to $NIRI_SOCKET");
        // Pin the post-context downcast: chain-walk must still recover the
        // typed CliError once wrapped.
        let recovered = wrapped
            .chain()
            .find_map(|e| e.downcast_ref::<CliError>())
            .expect("CliError must remain downcastable through .context wrap");
        assert!(matches!(recovered, CliError::SocketUnavailable(_)));
        assert_eq!(map_exit(&wrapped), 69);
    }

    #[test]
    fn cli_error_survives_context_wrap_in_alternate_format() {
        // {:#} on an anyhow chain must include both the context layer and
        // the CliError Display output. Pins the display contract before
        // any real chain flows through main.
        let base: anyhow::Error = CliError::SocketUnavailable(sample_io_error()).into();
        let wrapped = base.context("connecting to $NIRI_SOCKET");
        let formatted = format!("{wrapped:#}");
        assert!(
            formatted.contains("connecting to $NIRI_SOCKET"),
            "context layer missing from: {formatted}",
        );
        assert!(
            formatted.contains("ENOENT"),
            "source layer missing from: {formatted}",
        );
    }
}
