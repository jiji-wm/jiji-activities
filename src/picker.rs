//! `fuzzel`-backed single-select picker.
//!
//! This module wraps a `fuzzel --dmenu` invocation behind a small typed
//! API: callers hand it a prompt and a slice of items, and receive either
//! the user's selection or a cancellation signal. The intent is that the
//! picker is a leaf module — no IPC, no clap surface, just stdio piping.
//!
//! ## Cancellation contract
//!
//! `fuzzel` exits non-zero with empty stdout when the user dismisses the
//! menu (Escape, ctrl-C, etc.). The picker classifies that as
//! [`PickerOutcome::Cancelled`] — **not** an error. Defensive fallbacks
//! (non-zero exit with stdout, exit 0 with empty stdout) are also folded
//! into `Cancelled` so the caller never has to second-guess the wire-level
//! exit shape.
//!
//! ## Failure modes
//!
//! Every infrastructural failure (`fuzzel` not on `PATH`, spawn failure,
//! write-to-stdin failure, wait failure) routes to
//! [`CliError::SocketUnavailable`]. We deliberately reuse that variant
//! rather than minting a `PickerUnavailable` — picker breakage and socket
//! breakage are both "an external dependency required for this command is
//! gone," and they share an exit code (`EX_UNAVAILABLE` = 69). The
//! stderr message names `fuzzel` so the user knows which dep failed.

use std::io::Write;
use std::process::{Command, Stdio};

use crate::error::CliError;

/// Outcome of a picker invocation.
///
/// Cancellation is **not** an error — see module docs.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PickerOutcome {
    /// User selected one item. The carried `String` is the first line of
    /// `fuzzel`'s stdout, trimmed of trailing whitespace.
    Selected(String),
    /// User dismissed the picker without selecting anything.
    Cancelled,
}

/// Stderr message attached to the missing-`fuzzel` failure.
///
/// Exposed `pub(crate)` so tests have a single canonical source for the
/// message rather than duplicating the literal and risking drift. The
/// integration test currently matches on a substring (`"fuzzel"`) rather
/// than the full constant; import this constant in any test that needs to
/// pin the full message text.
pub(crate) const PICKER_MISSING_MESSAGE: &str =
    "fuzzel: command not found on $PATH (required for single-select picker)";

/// Checks that `fuzzel` is on `$PATH` and returns
/// [`CliError::SocketUnavailable`] with [`PICKER_MISSING_MESSAGE`] when
/// it isn't.
///
/// **Why this is exposed at module scope:** the production caller
/// (`cli::cmd_switch`) needs to verify availability *before* issuing
/// any IPC round-trips, so a missing-fuzzel install surfaces with the
/// fuzzel-naming stderr message rather than "niri socket unavailable."
/// `pick_one` re-runs the same check internally so it stays correct
/// when invoked from other call sites in the future.
pub(crate) fn ensure_available() -> Result<(), CliError> {
    which_in_path("fuzzel").map_err(|inner| {
        CliError::SocketUnavailable(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("{PICKER_MISSING_MESSAGE} ({inner})"),
        ))
    })
}

/// Runs `fuzzel --dmenu --prompt <prompt>` with `items` joined by `\n`
/// piped to stdin, and returns the selection.
///
/// **Contract:**
/// - Returns `Ok(PickerOutcome::Selected(name))` when `fuzzel` exits 0
///   with a non-empty stdout. `name` is the first line, trimmed.
/// - Returns `Ok(PickerOutcome::Cancelled)` on user dismissal (non-zero
///   exit + empty stdout) and on any defensive fallback (non-zero exit
///   with stdout, zero exit with no stdout).
/// - Returns `Err(CliError::SocketUnavailable)` when `fuzzel` is missing
///   from `$PATH`, fails to spawn, fails to accept stdin, or fails to
///   wait. The stderr message names `fuzzel` so the user can tell it
///   apart from socket-side breakage.
pub(crate) fn pick_one(prompt: &str, items: &[String]) -> Result<PickerOutcome, CliError> {
    // Belt-and-suspenders: `cmd_switch` is expected to call
    // `ensure_available()` first so the missing-fuzzel error surfaces
    // before any IPC round-trip (otherwise the user gets "niri socket
    // unavailable" when the real cause is a missing dep). Repeating the
    // check here keeps `pick_one` correct in isolation.
    ensure_available()?;

    let mut child = Command::new("fuzzel")
        .args(["--dmenu", "--prompt", prompt])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(CliError::SocketUnavailable)?;

    // Take ownership of stdin so we can drop it (signal EOF) before
    // `wait_with_output()`. Leaving the stdin handle open would deadlock
    // fuzzel: it reads until EOF before drawing the menu.
    {
        let mut stdin = child.stdin.take().ok_or_else(|| {
            CliError::SocketUnavailable(std::io::Error::other(
                "fuzzel: stdin pipe missing after spawn",
            ))
        })?;
        let payload = items_to_payload(items);
        if let Err(e) = stdin.write_all(payload.as_bytes()) {
            // Best-effort cleanup: don't leave fuzzel as a long-lived orphan.
            // The stdin drop below sends EOF so fuzzel will exit on its own,
            // but kill()+wait() bounds the cleanup time.
            drop(stdin);
            let _ = child.kill();
            let _ = child.wait();
            return Err(CliError::SocketUnavailable(e));
        }
        // `stdin` drops here → EOF on fuzzel's read side.
    }

    let output = child
        .wait_with_output()
        .map_err(CliError::SocketUnavailable)?;
    // Forward any stderr fuzzel wrote (GTK warnings, Wayland errors, etc.)
    // prefixed so the user knows it came from fuzzel, not the CLI itself.
    if !output.stderr.is_empty() {
        let stderr_text = String::from_utf8_lossy(&output.stderr);
        for line in stderr_text.lines() {
            eprintln!("fuzzel: {line}");
        }
    }
    Ok(classify_output(output.status.success(), &output.stdout))
}

/// Joins `items` with newlines, with a trailing newline so the last item
/// is delimited the same as the rest. Defined as a pure helper so the
/// stdin payload shape is unit-testable.
fn items_to_payload(items: &[String]) -> String {
    let mut s = items.join("\n");
    if !items.is_empty() {
        s.push('\n');
    }
    s
}

/// Classifies the child's exit status + stdout into a [`PickerOutcome`].
///
/// See module docs for the four cases:
/// - success + non-empty stdout → `Selected(first_line_trimmed)`.
/// - success + empty stdout → `Cancelled` (defensive).
/// - failure + empty stdout → `Cancelled` (the canonical user-dismiss
///   case).
/// - failure + non-empty stdout → `Cancelled` (defensive; some fuzzel
///   builds have been observed to print a final-frame line before
///   exiting non-zero on Escape).
fn classify_output(success: bool, stdout: &[u8]) -> PickerOutcome {
    let text = String::from_utf8_lossy(stdout);
    let first = text.lines().next().map(str::trim).unwrap_or("");
    match (success, first.is_empty()) {
        (true, false) => PickerOutcome::Selected(first.to_owned()),
        (true, true) => {
            // Defensive: fuzzel exited 0 with no stdout. Should not happen
            // in normal usage; log to stderr so future fuzzel changes that
            // trigger this path are diagnosable rather than silently eaten.
            eprintln!(
                "niri-activities: fuzzel exited 0 with empty stdout (treating as cancellation)"
            );
            PickerOutcome::Cancelled
        }
        (false, false) => {
            // Defensive: fuzzel exited non-zero but wrote stdout. Some
            // fuzzel builds emit a final-frame line before non-zero exit on
            // Escape; treat as cancellation but log so the case is visible.
            eprintln!(
                "niri-activities: fuzzel exited non-zero with stdout {:?} (treating as cancellation)",
                first
            );
            PickerOutcome::Cancelled
        }
        (false, true) => PickerOutcome::Cancelled,
    }
}

/// Walks `$PATH` looking for an executable named `bin`. Returns `Ok(())`
/// on the first hit, otherwise [`std::io::ErrorKind::NotFound`].
///
/// Implemented locally rather than pulling in the `which` crate: the
/// behaviour we need (Unix `PATH` walk + executable-bit check) is
/// ~20 lines and adds zero supply-chain weight.
///
/// **Unix-only** — uses `PermissionsExt::mode()` for the executable-bit
/// check. Behind `#[cfg(unix)]` so the crate would still compile on a
/// hypothetical non-Unix target, even though the CLI is Wayland-only.
fn which_in_path(bin: &str) -> Result<(), std::io::Error> {
    let path = std::env::var_os("PATH")
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "$PATH is unset"))?;
    for dir in std::env::split_paths(&path) {
        // POSIX: an empty string in `$PATH` means the current working
        // directory. That behaviour is deprecated and a security hazard;
        // skip empty entries rather than resolving against cwd.
        if dir.as_os_str().is_empty() {
            continue;
        }
        let candidate = dir.join(bin);
        if is_executable_file(&candidate) {
            return Ok(());
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        format!("{bin}: not found on $PATH"),
    ))
}

#[cfg(unix)]
fn is_executable_file(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    // Use `metadata`, not `symlink_metadata`: a `fuzzel` symlink in
    // `/usr/local/bin` pointing at the real binary must still count.
    match std::fs::metadata(path) {
        Ok(md) => md.is_file() && md.permissions().mode() & 0o111 != 0,
        Err(_) => false,
    }
}

#[cfg(not(unix))]
fn is_executable_file(path: &std::path::Path) -> bool {
    // Non-Unix fallback: file existence is the best we can do without
    // platform-specific exec-bit semantics. The CLI is Wayland-only in
    // practice, so this branch is unreachable on the target platform —
    // kept compiling so a hypothetical non-Unix `cargo check` doesn't fail.
    path.is_file()
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- which_in_path ---------------------------------------------------

    #[test]
    fn which_in_path_finds_common_shell_utility() {
        // `sh` is on every Unix `$PATH` in practice (POSIX requires
        // `/bin/sh` to exist). Using a builtin we know is present is
        // more reliable than synthesising one in a tempdir, which would
        // require touching the `tempfile` crate just for a unit test.
        which_in_path("sh").expect("sh must be on $PATH on any Unix host");
    }

    #[test]
    fn which_in_path_missing_returns_not_found() {
        let err = which_in_path("definitely-not-a-real-binary-xyz123")
            .expect_err("missing binary must not be found");
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    }

    // ---- classify_output -------------------------------------------------

    #[test]
    fn classify_output_success_non_empty_is_selected() {
        match classify_output(true, b"Work\n") {
            PickerOutcome::Selected(name) => assert_eq!(name, "Work"),
            PickerOutcome::Cancelled => panic!("expected Selected"),
        }
    }

    #[test]
    fn classify_output_failure_empty_is_cancelled() {
        // Canonical user-dismiss: non-zero exit + empty stdout.
        assert!(matches!(
            classify_output(false, b""),
            PickerOutcome::Cancelled
        ));
    }

    #[test]
    fn classify_output_success_empty_is_cancelled_defensive() {
        // Defensive fallback: zero exit with empty stdout is treated as
        // cancellation, not a selection of "".
        assert!(matches!(
            classify_output(true, b""),
            PickerOutcome::Cancelled
        ));
    }

    #[test]
    fn classify_output_failure_with_stdout_is_cancelled_defensive() {
        // Defensive fallback: non-zero exit but stdout has content. Some
        // fuzzel builds have been observed to emit a final-frame line
        // before non-zero exit on Escape.
        assert!(matches!(
            classify_output(false, b"Work\n"),
            PickerOutcome::Cancelled
        ));
    }

    #[test]
    fn classify_output_takes_first_line_only_and_trims() {
        // Defensive against trailing whitespace and stray multi-line
        // output. Pins that we take only the first line and strip its
        // surrounding whitespace.
        match classify_output(true, b"  Work  \nPersonal\n") {
            PickerOutcome::Selected(name) => assert_eq!(name, "Work"),
            PickerOutcome::Cancelled => panic!("expected Selected"),
        }
    }

    // ---- items_to_payload ------------------------------------------------

    #[test]
    fn items_to_payload_joins_and_terminates() {
        let items = vec![
            "Work".to_owned(),
            "Personal".to_owned(),
            "Gaming".to_owned(),
        ];
        assert_eq!(items_to_payload(&items), "Work\nPersonal\nGaming\n");
    }

    #[test]
    fn items_to_payload_empty_is_empty_string() {
        // Caller short-circuits before invoking the picker for empty
        // lists, but the helper still has to be well-defined on []
        // because the contract is "pure transformation."
        assert_eq!(items_to_payload(&[]), "");
    }
}
