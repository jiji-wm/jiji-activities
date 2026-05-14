//! `rofi`-backed multi-select picker.
//!
//! `rofi -dmenu -multi-select` is the multi-select dmenu we standardise
//! on. The CLI wraps it behind a typed API ([`pick_many`]) that returns
//! one of three [`MultiPickerOutcome`]s: a literal selection, a
//! chain-into-single-select signal, or cancellation. Two sentinel rows
//! (`« Select all »`, `« Only one… »`) sit above the real activity
//! rows; the sentinel-resolution logic lives in pure helpers
//! ([`parse_selection`], [`resolve_outcome`]) so it can be unit-tested
//! without spawning `rofi`.
//!
//! ## Why two sentinels, not three
//!
//! The earlier design carried a third sentinel (`« Select none »`) to
//! let the user clear all activity assignments from a workspace. That
//! was dropped because the compositor requires every workspace to
//! belong to at least one activity — `Action::SetWorkspaceActivities`
//! with an empty list is rejected at the IPC boundary. A sentinel that
//! always produces a server-side error is worse UX than no sentinel at
//! all.
//!
//! ## Failure modes
//!
//! Every infrastructural failure (`rofi` not on `PATH`, spawn failure,
//! write-to-stdin failure, wait failure) routes to
//! [`CliError::PickerUnavailable`] — the same typed carrier the
//! `fuzzel`-backed picker uses. Exit code 69 (`EX_UNAVAILABLE`); the
//! stderr `Display` prefix `picker unavailable:` plus the
//! [`ROFI_MISSING_MESSAGE`] body name `rofi` so the user can tell which
//! external dep is the problem.

use std::collections::HashSet;
use std::io::Write;
use std::process::{Command, Stdio};

use crate::error::CliError;
use crate::picker::PickerOutcome;

/// Stderr message attached to the missing-`rofi` failure.
///
/// Mirrors [`crate::picker::PICKER_MISSING_MESSAGE`] but names the
/// multi-select binary. Exposed `pub(crate)` so tests have a single
/// canonical source for the literal.
pub(crate) const ROFI_MISSING_MESSAGE: &str =
    "rofi: not on $PATH (required for multi-select picker)";

/// The two canonical sentinel strings, with their underscore-fallback
/// alternates folded in when a real activity name would clash with the
/// unicode form.
///
/// Held in a struct rather than two `&str` parameters so the caller
/// always uses a matched pair — passing the unicode `select_all` next
/// to the underscore `only_one` would split-brain the parser.
#[derive(Debug, Clone)]
pub(crate) struct SentinelNames {
    pub(crate) select_all: String,
    pub(crate) only_one: String,
}

impl SentinelNames {
    const UNICODE_SELECT_ALL: &'static str = "« Select all »";
    const UNICODE_ONLY_ONE: &'static str = "« Only one… »";
    const FALLBACK_SELECT_ALL: &'static str = "__niri_activities_select_all__";
    const FALLBACK_ONLY_ONE: &'static str = "__niri_activities_only_one__";
}

/// Outcome of a multi-select invocation, post-sentinel-resolution.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum MultiPickerOutcome {
    /// User confirmed a literal multi-selection. The carried `Vec`
    /// is the activity-name list to dispatch (sentinels already
    /// resolved — `Selected(all_activity_names)` for `« Select all »`,
    /// the user's literal selection otherwise).
    ///
    /// Construct via [`MultiPickerOutcome::selected`] to enforce the
    /// non-empty invariant.
    Selected(Vec<String>),
    /// User picked `« Only one… »`. The caller must invoke the
    /// chained single-select picker (`pick_one_chained`) and dispatch
    /// against its result.
    ChainSingle,
    /// User dismissed the multi-select picker without saving (rofi exited
    /// non-zero with empty stdout, or any defensive fallback shape).
    Cancelled,
}

impl MultiPickerOutcome {
    /// Constructs `Selected(names)`. Asserts `names` is non-empty in
    /// debug builds — `SetWorkspaceActivities` with an empty list is
    /// rejected by the compositor, so an empty `Selected` is a caller
    /// bug. Production callers always guard with `if names.is_empty()`
    /// before reaching this path.
    fn selected(names: Vec<String>) -> Self {
        debug_assert!(
            !names.is_empty(),
            "MultiPickerOutcome::Selected must be non-empty"
        );
        MultiPickerOutcome::Selected(names)
    }
}

/// Internal result of `parse_selection` — distinguishes "all activities"
/// (which the caller must expand to the literal name list) from a
/// literal user selection.
#[derive(Debug, PartialEq, Eq)]
enum ResolvedSelection {
    ChainSingle,
    SelectAll,
    Literal(Vec<String>),
}

/// Returns the sentinel strings to use given `activity_names`, falling
/// back to underscore-form alternates if any unicode sentinel would
/// collide with a real activity name.
///
/// The collision check is deliberately conservative: if either unicode
/// sentinel clashes, both flip to the underscore form. Mixing the two
/// (e.g. unicode `select_all` + underscore `only_one`) would be
/// surprising for users skimming the menu.
pub(crate) fn sentinel_names(activity_names: &[String]) -> SentinelNames {
    let names: HashSet<&str> = activity_names.iter().map(String::as_str).collect();
    let unicode_collides = names.contains(SentinelNames::UNICODE_SELECT_ALL)
        || names.contains(SentinelNames::UNICODE_ONLY_ONE);
    if unicode_collides {
        SentinelNames {
            select_all: SentinelNames::FALLBACK_SELECT_ALL.to_owned(),
            only_one: SentinelNames::FALLBACK_ONLY_ONE.to_owned(),
        }
    } else {
        SentinelNames {
            select_all: SentinelNames::UNICODE_SELECT_ALL.to_owned(),
            only_one: SentinelNames::UNICODE_ONLY_ONE.to_owned(),
        }
    }
}

/// Composes the stdin payload for `rofi`.
///
/// Layout:
/// ```text
/// « Select all »
/// « Only one… »
/// [x] Work
/// [ ] Personal
/// [x] Gaming
/// ```
///
/// Sentinels appear first with **no** prefix; activity rows are
/// pre-marked with `[x] ` for current members and `[ ] ` otherwise so
/// the user sees the existing assignment state on entry. Each row is
/// newline-terminated including the last.
pub(crate) fn build_input_payload(
    activity_names: &[String],
    current_membership: &HashSet<String>,
    sentinels: &SentinelNames,
) -> String {
    let mut out = String::new();
    out.push_str(&sentinels.select_all);
    out.push('\n');
    out.push_str(&sentinels.only_one);
    out.push('\n');
    for name in activity_names {
        if current_membership.contains(name) {
            out.push_str("[x] ");
        } else {
            out.push_str("[ ] ");
        }
        out.push_str(name);
        out.push('\n');
    }
    out
}

/// Strips the `[x] ` / `[ ] ` prefix from a returned rofi line (when
/// present) and recognises sentinel rows. Returns one of three resolved
/// states. Precedence: `ChainSingle` beats `SelectAll`; `SelectAll`
/// beats any literal mixing.
///
/// The strip-or-not logic is forgiving: rofi's exact output shape for
/// `-multi-select` may include or omit the original prefix depending on
/// version, so we accept both. Anything that doesn't start with a known
/// prefix is taken as-is.
fn parse_selection(returned_lines: &[&str], sentinels: &SentinelNames) -> ResolvedSelection {
    let mut chain_single = false;
    let mut select_all = false;
    let mut literal: Vec<String> = Vec::new();

    for raw in returned_lines {
        let line = raw.trim_end();
        if line.is_empty() {
            continue;
        }
        // Strip the membership prefix if rofi echoed it back. We accept
        // both `[x] ` and `[ ] ` (4 bytes each).
        let stripped = line
            .strip_prefix("[x] ")
            .or_else(|| line.strip_prefix("[ ] "))
            .unwrap_or(line);
        if stripped == sentinels.only_one {
            chain_single = true;
        } else if stripped == sentinels.select_all {
            select_all = true;
        } else {
            literal.push(stripped.to_owned());
        }
    }

    // Precedence: `« Only one… »` always wins. Once the user picks the
    // chain sentinel, anything else they marked is a confused click and
    // should be ignored — running the chained single-select picker is
    // unambiguous.
    if chain_single {
        ResolvedSelection::ChainSingle
    } else if select_all {
        ResolvedSelection::SelectAll
    } else {
        ResolvedSelection::Literal(literal)
    }
}

/// Glues `parse_selection` into a [`MultiPickerOutcome`], expanding
/// `SelectAll` into the literal activity-name list. Kept as a separate
/// helper so the expansion is unit-testable without re-deriving the
/// sentinel names.
fn resolve_outcome(
    returned_lines: &[&str],
    activity_names: &[String],
    sentinels: &SentinelNames,
) -> MultiPickerOutcome {
    match parse_selection(returned_lines, sentinels) {
        ResolvedSelection::ChainSingle => MultiPickerOutcome::ChainSingle,
        ResolvedSelection::SelectAll => MultiPickerOutcome::selected(activity_names.to_vec()),
        ResolvedSelection::Literal(names) => {
            if names.is_empty() {
                // Empty literal selection (user confirmed an empty
                // result without picking a sentinel) is treated as
                // cancellation — `SetWorkspaceActivities` with `[]` is
                // rejected by the compositor.
                MultiPickerOutcome::Cancelled
            } else {
                MultiPickerOutcome::selected(names)
            }
        }
    }
}

/// Checks that `rofi` is on `$PATH` and returns
/// [`CliError::PickerUnavailable`] when it isn't.
///
/// **Two-arm contract**, copy-paste-parallel with
/// [`crate::picker::ensure_available`]:
/// - `NotFound` from the walker is replaced with a synthetic
///   `NotFound` `io::Error` whose message is [`ROFI_MISSING_MESSAGE`]
///   so the stderr line frames the failure as "install rofi" rather
///   than a per-directory walker complaint.
/// - Any other `io::ErrorKind` (e.g. `PermissionDenied`) flows through
///   verbatim so the actionable detail is preserved.
pub(crate) fn ensure_available() -> Result<(), CliError> {
    match which_in_path("rofi") {
        Ok(()) => Ok(()),
        Err(inner) if inner.kind() == std::io::ErrorKind::NotFound => {
            Err(CliError::PickerUnavailable(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                ROFI_MISSING_MESSAGE,
            )))
        }
        Err(inner) => Err(CliError::PickerUnavailable(inner)),
    }
}

/// Walks `$PATH` looking for an executable named `bin`. Inlined from the
/// same logic in [`crate::picker::single_select`] (~20 lines) rather than
/// shared, to avoid widening `single_select`'s public surface for a single
/// re-use. The duplicate is intentional and small.
///
/// Contract: `Ok(())` on first hit, `NotFound` when no entry resolves,
/// other `io::ErrorKind` variants bubbled out verbatim (so
/// `ensure_available` can preserve the actionable detail when a
/// stat-through-chmod-000 fails).
fn which_in_path(bin: &str) -> Result<(), std::io::Error> {
    let path = std::env::var_os("PATH")
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "$PATH is unset"))?;
    for dir in std::env::split_paths(&path) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        let candidate = dir.join(bin);
        match is_executable_file(&candidate) {
            Ok(true) => return Ok(()),
            Ok(false) => continue,
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
    match std::fs::metadata(path) {
        Ok(md) => Ok(md.is_file() && md.permissions().mode() & 0o111 != 0),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

#[cfg(not(unix))]
fn is_executable_file(path: &std::path::Path) -> Result<bool, std::io::Error> {
    Ok(path.is_file())
}

/// Runs `rofi -dmenu -multi-select -prompt <prompt>` with the input
/// payload piped to stdin and returns a resolved [`MultiPickerOutcome`].
///
/// **Contract:**
/// - Returns `Ok(MultiPickerOutcome::Selected(names))` on a literal
///   confirmed selection (after `« Select all »` expansion).
/// - Returns `Ok(MultiPickerOutcome::ChainSingle)` when the user
///   picked `« Only one… »` (precedence: chain-single beats select-all
///   beats literal mixing).
/// - Returns `Ok(MultiPickerOutcome::Cancelled)` on rofi non-zero exit
///   with empty stdout, on any defensive fallback shape, and on a
///   literal-empty confirmed selection (no sentinel + no rows).
/// - Returns `Err(CliError::PickerUnavailable)` for any infrastructure
///   failure (spawn, stdin write, wait).
pub(crate) fn pick_many(
    activity_names: &[String],
    current_membership: &HashSet<String>,
) -> Result<MultiPickerOutcome, CliError> {
    ensure_available()?;
    let sentinels = sentinel_names(activity_names);

    let mut child = Command::new("rofi")
        .args(["-dmenu", "-multi-select", "-prompt", "Assign workspace to:"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(CliError::PickerUnavailable)?;

    {
        let mut stdin = child.stdin.take().ok_or_else(|| {
            CliError::PickerUnavailable(std::io::Error::other(
                "rofi: stdin pipe missing after spawn",
            ))
        })?;
        let payload = build_input_payload(activity_names, current_membership, &sentinels);
        if let Err(e) = stdin.write_all(payload.as_bytes()) {
            // Best-effort cleanup, matches the fuzzel-side
            // single_select precedent: drop stdin to EOF, then kill +
            // wait to bound the orphan.
            drop(stdin);
            let _ = child.kill();
            let _ = child.wait();
            return Err(CliError::PickerUnavailable(e));
        }
    }

    let output = child
        .wait_with_output()
        .map_err(CliError::PickerUnavailable)?;
    if !output.stderr.is_empty() {
        let stderr_text = String::from_utf8_lossy(&output.stderr);
        for line in stderr_text.lines() {
            eprintln!("rofi: {line}");
        }
    }
    Ok(classify_output(
        output.status.success(),
        &output.stdout,
        activity_names,
        &sentinels,
    ))
}

/// Classifies the rofi child's exit status + stdout into a
/// [`MultiPickerOutcome`].
///
/// Mirrors the four-case logic of the fuzzel-side `classify_output`:
/// - success + non-empty stdout → resolve sentinels and return.
/// - success + empty stdout → `Cancelled` (defensive; log).
/// - failure + empty stdout → `Cancelled` (canonical dismiss).
/// - failure + non-empty stdout → `Cancelled` (defensive; log).
fn classify_output(
    success: bool,
    stdout: &[u8],
    activity_names: &[String],
    sentinels: &SentinelNames,
) -> MultiPickerOutcome {
    let text = String::from_utf8_lossy(stdout);
    let lines: Vec<&str> = text.lines().collect();
    match (success, lines.iter().all(|l| l.trim().is_empty())) {
        (true, false) => resolve_outcome(&lines, activity_names, sentinels),
        (true, true) => {
            eprintln!(
                "niri-activities: rofi exited 0 with empty stdout (treating as cancellation)"
            );
            MultiPickerOutcome::Cancelled
        }
        (false, false) => {
            eprintln!(
                "niri-activities: rofi exited non-zero with stdout {:?} (treating as cancellation)",
                text.as_ref()
            );
            MultiPickerOutcome::Cancelled
        }
        (false, true) => MultiPickerOutcome::Cancelled,
    }
}

/// Runs `fuzzel --dmenu --prompt 'Assign workspace to (one):'` as the
/// chained single-select picker invoked when the multi-select returns
/// [`MultiPickerOutcome::ChainSingle`].
///
/// Delegates to the fuzzel-side [`crate::picker::pick_one`] so the
/// stdin payload shape, cancellation contract, and infra-failure
/// classification all stay in lockstep with the standalone `switch`
/// picker. The prompt is distinct from `switch`'s so the user knows
/// they're in the chained leg of `assign-workspace`.
pub(crate) fn pick_one_chained(activity_names: &[String]) -> Result<PickerOutcome, CliError> {
    crate::picker::pick_one("Assign workspace to (one):", activity_names)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names_for(strs: &[&str]) -> Vec<String> {
        strs.iter().map(|s| (*s).to_owned()).collect()
    }

    fn membership(strs: &[&str]) -> HashSet<String> {
        strs.iter().map(|s| (*s).to_owned()).collect()
    }

    // ---- sentinel_names -------------------------------------------------

    #[test]
    fn sentinel_names_uses_unicode_when_no_collision() {
        let names = names_for(&["Work", "Personal", "Gaming"]);
        let sentinels = sentinel_names(&names);
        assert_eq!(sentinels.select_all, "« Select all »");
        assert_eq!(sentinels.only_one, "« Only one… »");
    }

    #[test]
    fn sentinel_names_falls_back_to_underscore_on_collision() {
        // A user with a pathological activity named "« Select all »"
        // forces the underscore-fallback alternates. Both flip
        // together — mixing forms would split-brain the parser.
        let names = names_for(&["Work", "« Select all »", "Gaming"]);
        let sentinels = sentinel_names(&names);
        assert_eq!(sentinels.select_all, "__niri_activities_select_all__");
        assert_eq!(sentinels.only_one, "__niri_activities_only_one__");
    }

    // ---- build_input_payload --------------------------------------------

    #[test]
    fn build_input_payload_pre_marks_current_membership() {
        let names = names_for(&["Work", "Personal", "Gaming"]);
        let current = membership(&["Work", "Gaming"]);
        let sentinels = sentinel_names(&names);
        let payload = build_input_payload(&names, &current, &sentinels);
        let expected = "« Select all »\n« Only one… »\n[x] Work\n[ ] Personal\n[x] Gaming\n";
        assert_eq!(payload, expected);
    }

    // ---- parse_selection ------------------------------------------------

    #[test]
    fn parse_selection_strips_prefix_and_recognizes_sentinels() {
        let names = names_for(&["Work", "Personal", "Gaming"]);
        let sentinels = sentinel_names(&names);
        // rofi may echo back the prefix; parser must strip it.
        let lines = vec!["[x] Work", "[ ] Personal"];
        match parse_selection(&lines, &sentinels) {
            ResolvedSelection::Literal(v) => assert_eq!(v, vec!["Work", "Personal"]),
            other => panic!("expected Literal, got {other:?}"),
        }
    }

    #[test]
    fn parse_selection_precedence_only_one_beats_select_all() {
        let names = names_for(&["Work", "Personal"]);
        let sentinels = sentinel_names(&names);
        // User marked both sentinels + a literal. `Only one…` wins.
        let lines = vec!["« Select all »", "« Only one… »", "[x] Work"];
        assert_eq!(
            parse_selection(&lines, &sentinels),
            ResolvedSelection::ChainSingle,
        );
    }

    // ---- resolve_outcome ------------------------------------------------

    #[test]
    fn resolve_outcome_select_all_expands_to_full_name_list() {
        let names = names_for(&["Work", "Personal", "Gaming"]);
        let sentinels = sentinel_names(&names);
        let lines = vec!["« Select all »"];
        match resolve_outcome(&lines, &names, &sentinels) {
            MultiPickerOutcome::Selected(v) => {
                assert_eq!(v, names);
            }
            other => panic!("expected Selected(all), got {other:?}"),
        }
    }

    #[test]
    fn resolve_outcome_empty_literal_is_cancelled() {
        // Confirming nothing (no sentinel + no rows) is treated as
        // cancellation — the compositor rejects empty activity lists.
        let names = names_for(&["Work"]);
        let sentinels = sentinel_names(&names);
        let lines: Vec<&str> = vec![];
        assert_eq!(
            resolve_outcome(&lines, &names, &sentinels),
            MultiPickerOutcome::Cancelled,
        );
    }

    // ---- ensure_available -----------------------------------------------
    //
    // Pins the two-arm contract for the rofi side. Copy-paste-parallel
    // with the fuzzel-side `ensure_available` tests in
    // `single_select::tests` — same invariants, different binary name.

    /// PATH mutex that serialises the rofi-side ensure_available tests
    /// with each other. Distinct from the fuzzel-side mutex because the
    /// two modules don't share the helper; collision is avoided by
    /// keeping the mutexes per-module.
    static PATH_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn running_as_root() -> bool {
        match std::fs::read_to_string("/proc/self/status") {
            Ok(s) => s.lines().any(|line| {
                line.strip_prefix("Uid:")
                    .map(|rest| rest.split_whitespace().take(2).any(|uid| uid == "0"))
                    .unwrap_or(false)
            }),
            Err(_) => false,
        }
    }

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
                "niri-activities-rofi-{}-{}-{}",
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
            let _ = std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o700));
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    struct PathGuard(Option<std::ffi::OsString>);
    impl Drop for PathGuard {
        fn drop(&mut self) {
            // SAFETY: env mutation serialised via PATH_MUTEX in callers.
            unsafe {
                match self.0.take() {
                    Some(v) => std::env::set_var("PATH", v),
                    None => std::env::remove_var("PATH"),
                }
            }
        }
    }

    #[test]
    fn ensure_rofi_available_not_found_synthesises_canonical_message() {
        let _lock = PATH_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let empty_dir = std::env::temp_dir().join(format!(
            "niri-activities-rofi-ea-empty-{}",
            std::process::id(),
        ));
        std::fs::create_dir_all(&empty_dir).expect("create empty dir");
        let _guard = PathGuard(std::env::var_os("PATH"));
        // SAFETY: PATH_MUTEX serialises with sibling PATH-mutating tests.
        unsafe {
            std::env::set_var("PATH", &empty_dir);
        }

        let err = ensure_available().expect_err("ensure_available must fail when rofi is missing");
        let _ = std::fs::remove_dir_all(&empty_dir);

        match err {
            CliError::PickerUnavailable(io_err) => {
                assert_eq!(io_err.kind(), std::io::ErrorKind::NotFound);
                assert_eq!(io_err.to_string(), ROFI_MISSING_MESSAGE);
            }
            other => panic!("expected PickerUnavailable, got {other:?}"),
        }
    }

    #[test]
    fn ensure_rofi_available_preserves_non_not_found_verbatim() {
        if running_as_root() {
            return;
        }
        let _lock = PATH_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let dir = UnreadableDir::new("rofi-ensure-perm");
        let _guard = PathGuard(std::env::var_os("PATH"));
        // SAFETY: PATH_MUTEX serialises with sibling PATH-mutating tests.
        unsafe {
            std::env::set_var("PATH", dir.path.as_os_str());
        }

        let err =
            ensure_available().expect_err("ensure_available must fail on chmod 000 PATH entry");
        match err {
            CliError::PickerUnavailable(io_err) => {
                assert_eq!(io_err.kind(), std::io::ErrorKind::PermissionDenied);
                assert!(
                    !io_err.to_string().contains("not on $PATH"),
                    "non-NotFound arm must NOT wrap error in ROFI_MISSING_MESSAGE; got: {io_err}",
                );
            }
            other => panic!("expected PickerUnavailable, got {other:?}"),
        }
    }
}
