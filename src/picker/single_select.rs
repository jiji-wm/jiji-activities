//! `fuzzel`-backed single-select picker.
//!
//! This module wraps a `fuzzel --dmenu` invocation behind a small typed
//! API: callers hand it a prompt and a slice of items, and receive either
//! the user's selection or a cancellation signal. The intent is that the
//! single-select picker is a leaf module — no IPC, no clap surface, just
//! stdio piping.
//!
//! ## Cancellation contract
//!
//! `fuzzel` exits non-zero with empty stdout when the user dismisses the
//! menu (Escape, ctrl-C, etc.). The single-select picker classifies that
//! as
//! [`PickerOutcome::Cancelled`] — **not** an error. Defensive fallbacks
//! (non-zero exit with stdout, exit 0 with empty stdout) are also folded
//! into `Cancelled` so the caller never has to second-guess the wire-level
//! exit shape.
//!
//! ## Failure modes
//!
//! Every infrastructural failure (`fuzzel` not on `PATH`, spawn failure,
//! write-to-stdin failure, wait failure) routes to
//! [`CliError::PickerUnavailable`] — the typed carrier for picker-dep
//! failure. It shares exit code 69 (`EX_UNAVAILABLE`) with
//! [`CliError::SocketUnavailable`]; the two are disambiguated for the
//! user by the `Display` prefix (`picker unavailable:` vs
//! `niri socket unavailable:`). The stderr message names `fuzzel` so
//! the user knows which dep failed.

use std::io::Write;
use std::process::{Command, Stdio};

use crate::error::CliError;

/// Outcome of a single-select picker invocation.
///
/// Cancellation is **not** an error — see module docs.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PickerOutcome {
    /// User selected one item. The carried `String` is the first line of
    /// `fuzzel`'s stdout, trimmed of trailing whitespace.
    Selected(String),
    /// User dismissed the single-select picker without selecting anything.
    Cancelled,
}

/// Stderr message attached to the missing-`fuzzel` failure.
///
/// Used **only** for the `NotFound` arm of `ensure_available`: when
/// `which_in_path` returns `NotFound` (fuzzel absent from `$PATH` or
/// `$PATH` unset), this constant replaces the per-directory walker
/// message with user-actionable "install fuzzel" framing. Non-`NotFound`
/// errors (e.g. `PermissionDenied`) surface the original
/// `io::Error::to_string()` verbatim; they do **not** use this constant.
///
/// Exposed `pub(crate)` so tests have a single canonical source for the
/// message rather than duplicating the literal and risking drift. The
/// integration test currently matches on a substring (`"fuzzel"`) rather
/// than the full constant; import this constant in any test that needs to
/// pin the full message text.
pub(crate) const PICKER_MISSING_MESSAGE: &str =
    "fuzzel: not on $PATH (required for single-select picker)";

/// Checks that `fuzzel` is on `$PATH` and returns
/// [`CliError::PickerUnavailable`] when it isn't.
///
/// **Two-arm contract:**
/// - If `which_in_path` reports
///   [`std::io::ErrorKind::NotFound`] (either `$PATH` is unset, or no
///   entry contains an executable `fuzzel`), we synthesize a fresh `NotFound`
///   `io::Error` whose message is [`PICKER_MISSING_MESSAGE`]. That
///   way the stderr line carries the user-actionable "install fuzzel"
///   framing rather than a per-directory walker error.
/// - For any other `io::ErrorKind` (e.g. `PermissionDenied`,
///   `FilesystemLoop`, `EIO`), the original `io::Error` is preserved
///   verbatim inside `CliError::PickerUnavailable`. Wrapping it in a
///   synthetic `NotFound` would lie about the failure kind and erase
///   the actionable detail.
///
/// **Why this is exposed at module scope:** the production caller
/// (`cli::cmd_switch`) needs to verify availability *before* issuing
/// any IPC round-trips, so a missing-fuzzel install surfaces with the
/// picker-naming stderr message rather than "niri socket unavailable."
/// `pick_one` re-runs the same check internally so it stays correct
/// when invoked from other call sites in the future.
pub(crate) fn ensure_available() -> Result<(), CliError> {
    match which_in_path("fuzzel") {
        Ok(()) => Ok(()),
        Err(inner) if inner.kind() == std::io::ErrorKind::NotFound => {
            Err(CliError::PickerUnavailable(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                PICKER_MISSING_MESSAGE,
            )))
        }
        Err(inner) => Err(CliError::PickerUnavailable(inner)),
    }
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
/// - Returns `Err(CliError::PickerUnavailable)` when `fuzzel` is missing
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
        .map_err(CliError::PickerUnavailable)?;

    // Take ownership of stdin so we can drop it (signal EOF) before
    // `wait_with_output()`. Leaving the stdin handle open would deadlock
    // fuzzel: it reads until EOF before drawing the menu.
    {
        let mut stdin = child.stdin.take().ok_or_else(|| {
            CliError::PickerUnavailable(std::io::Error::other(
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
            return Err(CliError::PickerUnavailable(e));
        }
        // `stdin` drops here → EOF on fuzzel's read side.
    }

    let output = child
        .wait_with_output()
        .map_err(CliError::PickerUnavailable)?;
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
/// on the first hit, [`std::io::ErrorKind::NotFound`] if no entry on
/// `$PATH` resolves, and any other `io::ErrorKind` verbatim when a
/// per-entry stat fails for a reason other than "missing" (e.g.
/// `PermissionDenied`, `FilesystemLoop`, `EIO`).
///
/// Errors short-circuit the walk: we do **not** `continue` past a
/// non-`NotFound` stat failure, because doing so would silently hide
/// the real cause and let the caller think `fuzzel` is simply missing.
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
        match is_executable_file(&candidate) {
            Ok(true) => return Ok(()),
            Ok(false) => continue,
            // Non-NotFound stat error: short-circuit. Continuing would
            // hide the real cause (PermissionDenied on a $PATH entry,
            // symlink loop, EIO) behind a generic "not found on $PATH"
            // message. Surface the original error verbatim so
            // `ensure_available` can preserve its `ErrorKind`.
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        format!("{bin}: not found on $PATH"),
    ))
}

#[cfg(unix)]
fn is_executable_file(path: &std::path::Path) -> Result<bool, std::io::Error> {
    use std::os::unix::fs::PermissionsExt;
    // Use `metadata`, not `symlink_metadata`: a `fuzzel` symlink in
    // `/usr/local/bin` pointing at the real binary must still count.
    //
    // `NotFound` collapses to `Ok(false)` — a missing path is a normal
    // outcome of a `$PATH` walk, not an error. Every other kind
    // (`PermissionDenied`, `FilesystemLoop`, `EIO`, …) is bubbled out
    // verbatim so `which_in_path` can short-circuit.
    match std::fs::metadata(path) {
        Ok(md) => Ok(md.is_file() && md.permissions().mode() & 0o111 != 0),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

#[cfg(not(unix))]
fn is_executable_file(path: &std::path::Path) -> Result<bool, std::io::Error> {
    // Non-Unix fallback: file existence is the best we can do without
    // platform-specific exec-bit semantics. The CLI is Wayland-only in
    // practice, so this branch is unreachable on the target platform —
    // kept compiling so a hypothetical non-Unix `cargo check` doesn't fail.
    Ok(path.is_file())
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
        //
        // Lock against the PATH-mutating sibling test
        // (`which_in_path_propagates_permission_denied`); without this
        // serialisation the two can race on `$PATH`.
        let _lock = PATH_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        which_in_path("sh").expect("sh must be on $PATH on any Unix host");
    }

    #[test]
    fn which_in_path_missing_returns_not_found() {
        // See `which_in_path_finds_common_shell_utility` for why we lock.
        let _lock = PATH_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
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
        // Caller short-circuits before invoking the single-select picker
        // for empty lists, but the helper still has to be well-defined on
        // [] because the contract is "pure transformation."
        assert_eq!(items_to_payload(&[]), "");
    }

    // ---- which_in_path / is_executable_file error propagation ----------
    //
    // These tests stage a `chmod 000` directory on `$PATH` and assert that
    // stat failures with `ErrorKind::PermissionDenied` bubble out of
    // `is_executable_file` / `which_in_path` verbatim — they must NOT
    // silently collapse to "not found", which would erase the actionable
    // detail when `ensure_available` wraps the error.
    //
    // Skipped when running as root: a root EUID can stat through a
    // `chmod 000` directory, which would defeat the test setup.

    /// Returns `true` when the caller is running as UID 0 (real **or**
    /// effective). Parsed from `/proc/self/status` so we don't need a
    /// libc dep.
    ///
    /// The defensive form (rUID *or* EUID == 0) covers `sudo`-invoked
    /// sessions where real ≠ effective: permission bypass on `chmod 000`
    /// is driven by EUID, but checking both keeps the guard correct in
    /// any privilege-escalation scenario.
    fn running_as_root() -> bool {
        match std::fs::read_to_string("/proc/self/status") {
            Ok(s) => s.lines().any(|line| {
                line.strip_prefix("Uid:")
                    .map(|rest| rest.split_whitespace().take(2).any(|uid| uid == "0"))
                    .unwrap_or(false)
            }),
            // If we can't read /proc/self/status, assume non-root and let
            // the test attempt to run; a real failure will surface as a
            // genuine assertion error rather than a silent skip.
            Err(_) => false,
        }
    }

    /// Hand-rolled tempdir helper to avoid adding `tempfile` as a
    /// dev-dependency. PID + counter keeps concurrent test jobs disjoint.
    /// Cleans up on `Drop`; the `chmod 000` is reverted in the destructor
    /// so the dir can actually be removed.
    struct UnreadableDir {
        path: std::path::PathBuf,
    }

    impl UnreadableDir {
        fn new(tag: &str) -> Self {
            use std::os::unix::fs::PermissionsExt;
            use std::sync::atomic::{AtomicU64, Ordering};
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "niri-activities-picker-{}-{}-{}",
                std::process::id(),
                n,
                tag,
            ));
            std::fs::create_dir_all(&path).expect("create unreadable tempdir");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000))
                .expect("chmod 000 on tempdir");
            UnreadableDir { path }
        }
    }

    impl Drop for UnreadableDir {
        fn drop(&mut self) {
            use std::os::unix::fs::PermissionsExt;
            // Restore mode so we can actually remove the directory.
            let _ = std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o700));
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    /// Mutex serialising `$PATH` mutation across unit tests. `cargo test`
    /// runs tests on a thread pool, so the `which_in_path_*` tests below
    /// must not run concurrently with `which_in_path_finds_common_shell_utility`
    /// or each other — they all read `$PATH` and one of them clobbers it.
    /// `parking_lot`-free `std::sync::Mutex` is sufficient; we don't care
    /// about poisoning because the next test would just observe whatever
    /// PATH was left and likely fail loudly.
    static PATH_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn which_in_path_propagates_permission_denied() {
        if running_as_root() {
            // Root bypasses dir-mode permission checks; the chmod 000
            // setup wouldn't produce PermissionDenied. Skip rather than
            // produce a misleading pass.
            return;
        }
        let _lock = PATH_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let dir = UnreadableDir::new("which-perm");
        // Use a guard so we restore PATH even on assertion failure —
        // a poisoned `$PATH` would break sibling tests in the same
        // process.
        struct PathGuard(Option<std::ffi::OsString>);
        impl Drop for PathGuard {
            fn drop(&mut self) {
                // SAFETY: env mutation is unsafe in Rust 2024 because
                // concurrent threads could be reading $PATH via getenv.
                // We serialise PATH-mutating tests via PATH_MUTEX so no
                // sibling unit test is reading PATH while we're writing
                // it. Integration tests live in their own process.
                unsafe {
                    match self.0.take() {
                        Some(v) => std::env::set_var("PATH", v),
                        None => std::env::remove_var("PATH"),
                    }
                }
            }
        }
        let _guard = PathGuard(std::env::var_os("PATH"));
        // SAFETY: see PathGuard::drop above.
        unsafe {
            std::env::set_var("PATH", dir.path.as_os_str());
        }

        let err =
            which_in_path("anything").expect_err("chmod 000 PATH entry must surface a stat error");
        assert_eq!(
            err.kind(),
            std::io::ErrorKind::PermissionDenied,
            "expected PermissionDenied to bubble out of the walk, got {:?}: {}",
            err.kind(),
            err,
        );
    }

    #[test]
    fn is_executable_file_propagates_non_not_found() {
        if running_as_root() {
            return;
        }
        let dir = UnreadableDir::new("isexec-perm");
        // Probe a file *inside* the chmod-000 dir: stat on the child
        // returns PermissionDenied (search permission missing on parent).
        let probe = dir.path.join("x");
        let err = is_executable_file(&probe).expect_err("stat through chmod 000 parent must error");
        assert_eq!(
            err.kind(),
            std::io::ErrorKind::PermissionDenied,
            "is_executable_file must bubble non-NotFound errors verbatim, got {:?}: {}",
            err.kind(),
            err,
        );
    }

    // ---- is_executable_file direct contract tests ------------------------

    #[test]
    fn is_executable_file_missing_path_is_ok_false() {
        // A path that is guaranteed not to exist must return Ok(false), not
        // an error — NotFound is a normal outcome of a $PATH walk.
        let missing = std::env::temp_dir().join(format!(
            "niri-activities-nonexistent-{}-{}",
            std::process::id(),
            "isexec-missing",
        ));
        // Ensure the path truly does not exist before probing.
        let _ = std::fs::remove_file(&missing);
        assert!(
            !is_executable_file(&missing).expect("NotFound must collapse to Ok(false)"),
            "missing path must return Ok(false)",
        );
    }

    #[test]
    fn is_executable_file_non_executable_is_ok_false() {
        use std::os::unix::fs::PermissionsExt;
        // A regular file with no execute bits must return Ok(false).
        let path = std::env::temp_dir().join(format!(
            "niri-activities-noexec-{}-{}",
            std::process::id(),
            "isexec-noexec",
        ));
        std::fs::write(&path, b"").expect("create noexec test file");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))
            .expect("chmod 644 noexec test file");
        let result = is_executable_file(&path).expect("noexec file must not error");
        let _ = std::fs::remove_file(&path);
        assert!(!result, "non-executable file must return Ok(false)");
    }

    // ---- ensure_available direct contract tests --------------------------

    #[test]
    fn ensure_available_not_found_synthesises_canonical_message() {
        // When $PATH contains no entry with `fuzzel`, ensure_available must
        // return PickerUnavailable whose io::Error message equals PICKER_MISSING_MESSAGE.
        // This pins the NotFound arm of the two-arm contract.
        let _lock = PATH_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        // Use a tempdir that has no fuzzel binary so which_in_path returns NotFound.
        let empty_dir = std::env::temp_dir().join(format!(
            "niri-activities-ea-empty-{}-{}",
            std::process::id(),
            "ensure-notfound",
        ));
        std::fs::create_dir_all(&empty_dir).expect("create empty dir for ensure_available test");

        struct PathGuard(Option<std::ffi::OsString>);
        impl Drop for PathGuard {
            fn drop(&mut self) {
                unsafe {
                    match self.0.take() {
                        Some(v) => std::env::set_var("PATH", v),
                        None => std::env::remove_var("PATH"),
                    }
                }
            }
        }
        let _guard = PathGuard(std::env::var_os("PATH"));
        unsafe {
            std::env::set_var("PATH", &empty_dir);
        }

        let err =
            ensure_available().expect_err("ensure_available must fail when fuzzel is missing");
        let _ = std::fs::remove_dir_all(&empty_dir);

        match err {
            CliError::PickerUnavailable(io_err) => {
                assert_eq!(
                    io_err.kind(),
                    std::io::ErrorKind::NotFound,
                    "NotFound arm must produce a NotFound io::Error",
                );
                assert_eq!(
                    io_err.to_string(),
                    PICKER_MISSING_MESSAGE,
                    "NotFound arm must synthesize PICKER_MISSING_MESSAGE verbatim",
                );
            }
            other => panic!("expected PickerUnavailable, got {other:?}"),
        }
    }

    #[test]
    fn ensure_available_preserves_non_not_found_verbatim() {
        // When a $PATH entry produces a non-NotFound error (PermissionDenied),
        // ensure_available must preserve the original io::Error verbatim —
        // it must NOT wrap it in PICKER_MISSING_MESSAGE.
        if running_as_root() {
            return;
        }
        let _lock = PATH_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let dir = UnreadableDir::new("ensure-perm");

        struct PathGuard(Option<std::ffi::OsString>);
        impl Drop for PathGuard {
            fn drop(&mut self) {
                unsafe {
                    match self.0.take() {
                        Some(v) => std::env::set_var("PATH", v),
                        None => std::env::remove_var("PATH"),
                    }
                }
            }
        }
        let _guard = PathGuard(std::env::var_os("PATH"));
        unsafe {
            std::env::set_var("PATH", dir.path.as_os_str());
        }

        let err =
            ensure_available().expect_err("ensure_available must fail on chmod 000 PATH entry");

        match err {
            CliError::PickerUnavailable(io_err) => {
                assert_eq!(
                    io_err.kind(),
                    std::io::ErrorKind::PermissionDenied,
                    "non-NotFound arm must preserve the original ErrorKind",
                );
                assert!(
                    !io_err.to_string().contains("not on $PATH"),
                    "non-NotFound arm must NOT wrap error in PICKER_MISSING_MESSAGE; \
                     got: {io_err}",
                );
            }
            other => panic!("expected PickerUnavailable, got {other:?}"),
        }
    }

    // ---- which_in_path short-circuit ordering test -----------------------

    #[test]
    fn which_in_path_short_circuits_before_later_match() {
        // Pins the docstring claim: "Errors short-circuit the walk."
        // PATH = [chmod-000-dir, good-dir-with-real-executable].
        // which_in_path must return Err(PermissionDenied) — not Ok(()),
        // which would mean it skipped past the bad entry to find the binary.
        if running_as_root() {
            return;
        }
        let _lock = PATH_MUTEX.lock().unwrap_or_else(|e| e.into_inner());

        let bad_dir = UnreadableDir::new("short-circuit-bad");

        // Create a good dir containing a unique executable so we can tell
        // if which_in_path accidentally walks past the bad dir.
        let good_dir = std::env::temp_dir().join(format!(
            "niri-activities-sc-good-{}-{}",
            std::process::id(),
            "short-circuit-good",
        ));
        std::fs::create_dir_all(&good_dir).expect("create good dir");
        let bin_name = format!("uniqbin-{}", std::process::id());
        let bin_path = good_dir.join(&bin_name);
        std::fs::write(&bin_path, b"#!/bin/sh\n").expect("write unique bin");
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bin_path, std::fs::Permissions::from_mode(0o755))
                .expect("chmod +x unique bin");
        }

        struct PathGuard(Option<std::ffi::OsString>);
        impl Drop for PathGuard {
            fn drop(&mut self) {
                unsafe {
                    match self.0.take() {
                        Some(v) => std::env::set_var("PATH", v),
                        None => std::env::remove_var("PATH"),
                    }
                }
            }
        }
        let _guard = PathGuard(std::env::var_os("PATH"));
        let combined = format!("{}:{}", bad_dir.path.display(), good_dir.display());
        unsafe {
            std::env::set_var("PATH", &combined);
        }

        let result = which_in_path(&bin_name);
        let _ = std::fs::remove_dir_all(&good_dir);

        let err = result.expect_err(
            "which_in_path must short-circuit on PermissionDenied, not walk past to find the bin",
        );
        assert_eq!(
            err.kind(),
            std::io::ErrorKind::PermissionDenied,
            "short-circuit must surface PermissionDenied, not find the binary in the later dir; \
             got {:?}: {}",
            err.kind(),
            err,
        );
    }
}
