//! `save` subcommand: append `activity "<name>"` to the user's niri
//! config file, then trigger a config reload over IPC.
//!
//! ## Why an external config edit
//!
//! Activities the compositor knows about at runtime are not persisted
//! by niri itself: a fresh compositor start re-reads the config file
//! and only the `activity` nodes declared there are seeded. The `save`
//! subcommand bridges that gap by editing the user's KDL config in
//! place and asking the compositor to reload.
//!
//! ## Why the `kdl` crate
//!
//! User configs are hand-maintained, often with extensive comments and
//! idiosyncratic formatting. A string-append heuristic would have to
//! cope with file-not-exists / file-exists-empty / trailing-newline
//! variation / quoted-name detection / block-form `activity "x" {}`
//! nodes — each a place where a careless implementation silently
//! corrupts the user's config. The `kdl` crate's document-oriented
//! parser preserves formatting, whitespace, and comments through a
//! round-trip; we touch only the newly appended node.
//!
//! ## KDL spec version
//!
//! niri's parser (`knuffel` 3.2.0) speaks KDL v1; the kdl-rs default
//! dialect is KDL v2. The `v1` feature on the `kdl` dep selects the
//! matching parser. Failing to do this would let `save` write nodes
//! the compositor can't read back.
//!
//! ## Idempotency
//!
//! A case-insensitive collision with any existing top-level
//! `activity "..."` node short-circuits before the filesystem is
//! touched, prints an informational stderr line, and exits 0 without
//! issuing the reload IPC. Matches niri's own duplicate-name handling
//! which uses `eq_ignore_ascii_case` for activity names.
//!
//! ## Failure mode: file written, reload failed
//!
//! The atomic write-then-rename happens BEFORE the `LoadConfigFile`
//! IPC call. If the reload fails (compositor parse error, dead socket,
//! etc.) the new `activity` node is still on disk and will take effect
//! at the next compositor start or manual reload. Rolling back the
//! file is deliberately NOT attempted — the user can re-edit by hand
//! or rerun `save` after fixing the upstream issue, and we avoid the
//! risk of losing the just-written declaration to a buggy rollback.
//!
//! ## Error matrix
//!
//! - Empty-after-trim `<name>` → `CliError::Usage` (exit 64). Pre-validated
//!   BEFORE any filesystem op; the compositor's own empty-name check
//!   never runs.
//! - Path resolution failure (platform config-dir unavailable,
//!   `ProjectDirs::from` returns `None`) → `CliError::ConfigEdit` (exit 73).
//! - File read / write / mkdir / rename / parent-create failure →
//!   `CliError::ConfigEdit` (exit 73).
//! - KDL parse error on existing config → `CliError::ConfigEdit` (exit
//!   73). The user must repair the file by hand; we refuse to overwrite
//!   a broken file.
//! - `LoadConfigFile` transport failure → `CliError::SocketUnavailable`
//!   (exit 69).
//! - `LoadConfigFile` returned `Reply::Err(_)` or a non-`Handled`
//!   `Response` → `CliError::MalformedResponse` (exit 65).

use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use directories::ProjectDirs;
use kdl::{KdlDocument, KdlEntry, KdlEntryFormat, KdlNode, KdlValue};
use niri_ipc::{Action, Request, Response};

use crate::error::{CliError, MalformedResponseSource};
use crate::ipc::{IpcError, NiriClient};

const JIJI_CONFIG_ENV: &str = "JIJI_CONFIG";
const TMP_SUFFIX: &str = ".jiji-activities.tmp";

/// Outcome of [`append_activity_if_absent`].
///
/// `Added` carries the new document content the caller should write
/// to disk; `AlreadyDeclared` signals the idempotent short-circuit and
/// indicates no filesystem write or IPC dispatch should happen.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum AppendOutcome {
    /// The existing config did not contain a matching `activity` node;
    /// the wrapped `String` is the serialised document with the new
    /// node appended, ready to write atomically.
    Added(String),
    /// A case-insensitive match for `name` is already present at the
    /// top level. The caller should print an informational message and
    /// exit 0 without writing or dispatching a reload.
    AlreadyDeclared,
}

/// Resolves the niri config-file path the CLI should edit.
///
/// Production code resolves via [`RealConfigPaths`]; tests inject a
/// fixed path. Kept as a trait so an in-process `run()` test can drive
/// the full filesystem path without touching real environment state.
pub(crate) trait ConfigPathResolver {
    /// Returns the absolute path to the niri user config file the CLI
    /// should edit. Mirrors jiji's own resolution: `$JIJI_CONFIG` first,
    /// then `ProjectDirs::from("", "", "jiji").config_dir() / config.kdl`.
    fn resolve(&self) -> std::result::Result<PathBuf, CliError>;
}

/// Production [`ConfigPathResolver`]: `$JIJI_CONFIG` → `ProjectDirs`
/// fallback. Mirrors niri's own config-path resolution order: env
/// override first, platform config dir second.
pub(crate) struct RealConfigPaths;

impl ConfigPathResolver for RealConfigPaths {
    fn resolve(&self) -> std::result::Result<PathBuf, CliError> {
        if let Some(env) = std::env::var_os(JIJI_CONFIG_ENV)
            && !env.is_empty()
        {
            return Ok(PathBuf::from(env));
        }
        let Some(dirs) = ProjectDirs::from("", "", "jiji") else {
            return Err(CliError::ConfigEdit(io::Error::new(
                io::ErrorKind::NotFound,
                "could not determine jiji config path: $HOME is unset and $JIJI_CONFIG is not set. Set either to proceed.",
            )));
        };
        Ok(dirs.config_dir().join("config.kdl"))
    }
}

/// Appends an `activity "<name>"` node to `existing` if no
/// case-insensitive match is already present at the top level.
///
/// **Contract:**
/// - Parses `existing` as KDL v1; returns
///   `CliError::ConfigEdit(io::Error)` with kind `InvalidData` on parse
///   failure (the caller must surface this verbatim — we refuse to
///   write into a config the compositor can't read back).
/// - A top-level `activity` node whose first argument is a **string**
///   equal to `name` under [`str::eq_ignore_ascii_case`] (matching
///   niri's own duplicate-name check) yields
///   `AppendOutcome::AlreadyDeclared`. Block-form `activity "x" {}`
///   matches just as well as bare `activity "x"` — the entry list is
///   independent of the children block.
/// - A top-level `activity` node whose first argument is a **non-string**
///   (e.g. `activity 42`, `activity true`) causes an `InvalidData`
///   error; the user must repair the config before `save` appends into
///   it.
/// - Otherwise yields `AppendOutcome::Added(new_content)` — the new
///   node is appended via `KdlDocument::nodes_mut().push(...)`; the
///   existing document layout (whitespace, comments, ordering) is
///   preserved by the kdl crate's document-oriented model.
///
/// **Note:** the appended node has `leading: "\n"` when `existing` is
/// non-empty and does not end with a newline, to guarantee the new
/// node starts on a fresh line even when the user's file is missing
/// the customary trailing newline.
pub(crate) fn append_activity_if_absent(
    existing: &str,
    name: &str,
) -> std::result::Result<AppendOutcome, CliError> {
    // Empty / whitespace-only existing content: parse_v1 on "" may
    // succeed or fail depending on the version; bypass and start with
    // a fresh document so the behaviour is stable.
    let mut document = if existing.trim().is_empty() {
        KdlDocument::new()
    } else {
        KdlDocument::parse_v1(existing).map_err(|e| {
            CliError::ConfigEdit(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("niri config is not valid KDL: {e:?}"),
            ))
        })?
    };

    // Case-insensitive collision check against every top-level
    // `activity` node's first argument. Matches niri's own
    // duplicate-name handling for activities (`eq_ignore_ascii_case`).
    for node in document.nodes() {
        if node.name().value() != "activity" {
            continue;
        }
        let Some(first) = node.entries().first() else {
            continue;
        };
        match first.value() {
            KdlValue::String(s) => {
                if s.eq_ignore_ascii_case(name) {
                    return Ok(AppendOutcome::AlreadyDeclared);
                }
                // Non-ASCII names: `eq_ignore_ascii_case` only folds
                // ASCII letters. Emit a one-time informational note
                // rather than silently missing a Unicode duplicate.
                if !name.is_ascii() || !s.is_ascii() {
                    eprintln!(
                        "jiji-activities: note: activity name contains non-ASCII characters; \
                         collision detection is ASCII-only"
                    );
                }
            }
            other => {
                // A non-String first entry (e.g. `activity 42` or
                // `activity true`) is malformed. Reject so the user
                // repairs the config before we append into it.
                return Err(CliError::ConfigEdit(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "niri config contains an `activity` node whose first entry is not a \
                         string (got {other:?}); repair the config file before running `save`"
                    ),
                )));
            }
        }
    }

    let mut node = KdlNode::new("activity");
    node.entries_mut().push(quoted_string_entry(name));

    // If the existing file lacked a trailing newline (or last node's
    // trailing was stripped), force a newline before the new node so
    // it does not glue onto the previous node's terminator.
    if !existing.is_empty()
        && !existing.ends_with('\n')
        && let Some(fmt) = node.format_mut()
    {
        fmt.leading = "\n".into();
    }
    document.nodes_mut().push(node);

    Ok(AppendOutcome::Added(document.to_string()))
}

/// Builds a positional [`KdlEntry`] whose value is a string AND whose
/// serialised form is always a KDL quoted string (never a bare
/// identifier).
///
/// The default `KdlValue::String` display picks bareword form when the
/// string is a valid plain identifier (kdl-rs v6 is KDL v2-first; v2
/// allows bareword string values). niri's parser speaks KDL v1, in
/// which `activity Work` parses `Work` as an identifier value rather
/// than a string — guaranteed to break niri's `knuffel`-derived
/// `Activity` decoder that expects `Literal::String`. We pin the
/// serialised form to the quoted-string shape so the round-trip
/// matches niri's expectations regardless of the name's contents.
fn quoted_string_entry(name: &str) -> KdlEntry {
    let mut entry = KdlEntry::new(KdlValue::String(name.to_owned()));
    let mut quoted = String::with_capacity(name.len() + 2);
    quoted.push('"');
    for c in name.chars() {
        match c {
            '\\' | '"' => {
                quoted.push('\\');
                quoted.push(c);
            }
            '\n' => quoted.push_str("\\n"),
            '\r' => quoted.push_str("\\r"),
            '\t' => quoted.push_str("\\t"),
            _ => quoted.push(c),
        }
    }
    quoted.push('"');
    entry.set_format(KdlEntryFormat {
        value_repr: quoted,
        leading: " ".into(),
        ..Default::default()
    });
    entry
}

/// Wires the `save <name>` subcommand: validate the name, resolve the
/// config path, append the `activity` node if absent, write atomically,
/// then trigger a compositor reload via `Action::LoadConfigFile { path: None }`.
///
/// **Contract:** the file write happens BEFORE the reload IPC call.
/// A reload failure does NOT roll back the on-disk edit; the new
/// `activity` node remains and will take effect at next compositor
/// start or manual reload. See module docs for rationale.
pub(crate) fn run(
    client: &mut dyn NiriClient,
    name: &str,
    paths: &dyn ConfigPathResolver,
) -> Result<()> {
    if name.trim().is_empty() {
        return Err(CliError::Usage(format!(
            "activity name must not be empty or whitespace-only (got {name:?})"
        ))
        .into());
    }

    // -- Filesystem phase ----------------------------------------------
    let written_path = (|| -> std::result::Result<Option<std::path::PathBuf>, CliError> {
        let path = paths.resolve()?;
        let existing = read_or_empty(&path)?;
        match append_activity_if_absent(&existing, name)? {
            AppendOutcome::AlreadyDeclared => {
                eprintln!(
                    "jiji-activities: activity \"{name}\" is already declared in config; nothing to write"
                );
                Ok(None)
            }
            AppendOutcome::Added(new_content) => {
                ensure_parent_dir(&path)?;
                atomic_write(&path, &new_content)?;
                Ok(Some(path))
            }
        }
    })()
    .context("writing config file")?;

    let Some(written_path) = written_path else {
        return Ok(());
    };

    // -- IPC phase -----------------------------------------------------
    // The file has been written. If the reload fails, inform the user
    // so they know the on-disk edit succeeded and can reload manually.
    dispatch_reload(client, &written_path).context("reloading niri config")
}

/// Reads `path` to a `String`. Treats `NotFound` as empty content
/// (niri runs from internal defaults when no user config file exists;
/// this CLI may be invoked before the user has written one). All
/// other `io::Error` kinds map to `CliError::ConfigEdit`.
fn read_or_empty(path: &Path) -> std::result::Result<String, CliError> {
    match std::fs::read_to_string(path) {
        Ok(s) => Ok(s),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(CliError::ConfigEdit(e)),
    }
}

/// Creates the parent directory of `path` if it does not already
/// exist. Maps any failure (including `path` having no parent) to
/// `CliError::ConfigEdit`.
fn ensure_parent_dir(path: &Path) -> std::result::Result<(), CliError> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    if parent.as_os_str().is_empty() {
        return Ok(());
    }
    std::fs::create_dir_all(parent).map_err(CliError::ConfigEdit)
}

/// Writes `content` to `path` via a same-directory tmp file plus
/// `rename`. `rename` on Unix is atomic within a single filesystem;
/// placing the tmp file in the same directory as the target ensures
/// they are on the same filesystem under ordinary mounts. Note: this
/// guarantees all-or-nothing visibility, but does NOT guarantee
/// durability across a crash (no `fsync` is performed).
fn atomic_write(path: &Path, content: &str) -> std::result::Result<(), CliError> {
    let tmp = tmp_path_for(path)?;
    std::fs::write(&tmp, content).map_err(CliError::ConfigEdit)?;
    std::fs::rename(&tmp, path).map_err(|e| {
        // Best-effort cleanup: a stale tmp file would shadow future
        // atomic writes and confuse users grepping their config dir.
        if let Err(cleanup_err) = std::fs::remove_file(&tmp)
            && cleanup_err.kind() != io::ErrorKind::NotFound
        {
            eprintln!(
                "jiji-activities: warning: could not remove tmp file {}: {cleanup_err}",
                tmp.display(),
            );
        }
        CliError::ConfigEdit(e)
    })
}

/// Returns the temp path used for the atomic write. Sibling of `path`
/// so `rename` stays on the same filesystem. Returns a `CliError` if
/// `path` has no filename component (e.g. it is `/` or empty).
fn tmp_path_for(path: &Path) -> std::result::Result<PathBuf, CliError> {
    let file_name = path.file_name().ok_or_else(|| {
        CliError::ConfigEdit(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("config path has no filename component: {}", path.display()),
        ))
    })?;
    let mut tmp_name = file_name.to_os_string();
    tmp_name.push(TMP_SUFFIX);
    Ok(match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.join(tmp_name),
        _ => PathBuf::from(tmp_name),
    })
}

/// Issues `Action::LoadConfigFile { path: None }` and maps the reply.
///
/// `path: None` reloads the compositor's currently-active config,
/// bypassing path-validation that the compositor applies to explicit
/// paths. This avoids false errors when the compositor's path
/// resolution diverges from ours.
///
/// On any error (transport failure, server error, wrong response
/// variant), a recovery breadcrumb is printed to stderr informing the
/// user that the file was written and a manual reload command is
/// available. This covers the primary documented failure mode
/// (`IpcError::Transport` — dead socket, exit 69) as well as
/// `IpcError::Server` and `IpcError::Decode`.
fn dispatch_reload(client: &mut dyn NiriClient, written_path: &Path) -> Result<()> {
    let req = Request::Action(Action::LoadConfigFile { path: None });
    match client.send(req) {
        Ok(Response::Handled) => Ok(()),
        Ok(other) => {
            emit_written_breadcrumb(written_path);
            Err(
                CliError::MalformedResponse(MalformedResponseSource::WrongVariant {
                    expected: "Response::Handled",
                    got: format!("{:?}", other),
                })
                .into(),
            )
        }
        Err(IpcError::Server(msg)) => {
            emit_written_breadcrumb(written_path);
            // The compositor's reload-error wire format is opaque to
            // us; surface verbatim under MalformedResponse(Server).
            Err(CliError::MalformedResponse(MalformedResponseSource::Server(msg)).into())
        }
        Err(other) => {
            emit_written_breadcrumb(written_path);
            Err(CliError::from(other).into())
        }
    }
}

/// Prints a one-line stderr note that the config file was written and
/// includes the manual-reload command. Called from every error arm of
/// [`dispatch_reload`] so no failure mode silently drops the breadcrumb.
fn emit_written_breadcrumb(written_path: &Path) {
    eprintln!(
        "jiji-activities: note: activity was written to {}; \
         run `niri msg action load-config-file` to reload manually",
        written_path.display()
    );
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    use niri_ipc::{Action, Reply, Request, Response};
    use tempfile::TempDir;

    use super::*;
    use crate::error::{CliError, MalformedResponseSource};
    use crate::ipc::MockClient;

    // `$JIJI_CONFIG` is process-global; tests that mutate it must run
    // serialized.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Test-only [`ConfigPathResolver`] returning a fixed path.
    struct FixedPath(PathBuf);

    impl ConfigPathResolver for FixedPath {
        fn resolve(&self) -> std::result::Result<PathBuf, CliError> {
            Ok(self.0.clone())
        }
    }

    fn load_config_req() -> Request {
        Request::Action(Action::LoadConfigFile { path: None })
    }

    fn write_config(dir: &Path, contents: &str) -> PathBuf {
        let path = dir.join("config.kdl");
        std::fs::write(&path, contents).expect("write seed config");
        path
    }

    // ---- append_activity_if_absent ----

    #[test]
    fn append_to_empty_document_adds_activity() {
        let outcome = append_activity_if_absent("", "Work").expect("parse + append succeeds");
        match outcome {
            AppendOutcome::Added(s) => {
                assert!(
                    s.contains("activity") && s.contains("Work"),
                    "appended content must contain the new node: {s:?}",
                );
                assert!(
                    s.trim_end().ends_with("\"Work\"") || s.contains("activity \"Work\""),
                    "new node must use string-literal name form: {s:?}",
                );
            }
            other => panic!("expected Added, got {other:?}"),
        }
    }

    #[test]
    fn append_to_document_with_one_unrelated_node_keeps_it() {
        let existing = "input {\n    keyboard\n}\n";
        let outcome = append_activity_if_absent(existing, "Work").expect("append succeeds");
        match outcome {
            AppendOutcome::Added(s) => {
                assert!(s.contains("input"), "unrelated node must survive: {s:?}");
                assert!(
                    s.contains("activity") && s.contains("\"Work\""),
                    "new node must be present: {s:?}",
                );
            }
            other => panic!("expected Added, got {other:?}"),
        }
    }

    #[test]
    fn append_preserves_comments() {
        let existing = "// Keep me.\ninput {\n    // and me\n}\n";
        let outcome = append_activity_if_absent(existing, "Work").expect("append succeeds");
        match outcome {
            AppendOutcome::Added(s) => {
                assert!(
                    s.contains("// Keep me."),
                    "leading comment must survive: {s:?}",
                );
                assert!(s.contains("// and me"), "inner comment must survive: {s:?}");
            }
            other => panic!("expected Added, got {other:?}"),
        }
    }

    #[test]
    fn append_preserves_trailing_blank_line() {
        // Trailing whitespace after the last node should round-trip; we
        // don't care about exact whitespace, only that the new node ends
        // with a terminator so the next append wouldn't collide.
        let existing = "input {\n}\n\n";
        let outcome = append_activity_if_absent(existing, "Work").expect("append succeeds");
        match outcome {
            AppendOutcome::Added(s) => {
                assert!(
                    s.ends_with('\n'),
                    "output must end with newline, got: {s:?}",
                );
            }
            other => panic!("expected Added, got {other:?}"),
        }
    }

    #[test]
    fn existing_activity_exact_match_is_already_declared() {
        let existing = "activity \"Work\"\n";
        let outcome = append_activity_if_absent(existing, "Work").expect("parse succeeds");
        assert_eq!(outcome, AppendOutcome::AlreadyDeclared);
    }

    #[test]
    fn existing_activity_case_insensitive_match_is_already_declared() {
        // Mirrors niri-config's eq_ignore_ascii_case rule.
        let existing = "activity \"WORK\"\n";
        let outcome = append_activity_if_absent(existing, "work").expect("parse succeeds");
        assert_eq!(outcome, AppendOutcome::AlreadyDeclared);
    }

    #[test]
    fn existing_activity_substring_does_not_match() {
        // "Workshop" is not "Work" — substring matches must NOT
        // short-circuit the append.
        let existing = "activity \"Workshop\"\n";
        let outcome = append_activity_if_absent(existing, "Work").expect("parse succeeds");
        match outcome {
            AppendOutcome::Added(s) => assert!(s.contains("\"Work\"") && s.contains("Workshop")),
            other => panic!("expected Added, got {other:?}"),
        }
    }

    #[test]
    fn existing_activity_block_form_still_matches() {
        // The block-form `activity "Work" {}` still has "Work" as
        // entries()[0]; case-insensitive match must fire regardless of
        // whether a children block follows.
        let existing = "activity \"Work\" {\n}\n";
        let outcome = append_activity_if_absent(existing, "Work").expect("parse succeeds");
        assert_eq!(outcome, AppendOutcome::AlreadyDeclared);
    }

    #[test]
    fn parse_error_returns_config_edit() {
        // Invalid KDL: bracket left open. parse_v1 must fail; we must
        // refuse to write rather than overwriting a broken file.
        let existing = "input {\n";
        let err = append_activity_if_absent(existing, "Work").expect_err("parse must fail");
        match err {
            CliError::ConfigEdit(io) => {
                assert_eq!(io.kind(), io::ErrorKind::InvalidData);
            }
            other => panic!("expected ConfigEdit(InvalidData), got {other:?}"),
        }
    }

    #[test]
    fn append_when_existing_has_no_trailing_newline() {
        // Exercises the leading-`\n` injection branch: when the existing
        // content does not end with '\n', the new node must get a leading
        // newline to avoid gluing onto the previous node's terminator.
        let existing = "input {}"; // deliberately no trailing newline
        let outcome = append_activity_if_absent(existing, "Work").expect("append succeeds");
        match outcome {
            AppendOutcome::Added(s) => {
                // The new node must be on its own line.
                assert!(
                    s.contains("\nactivity"),
                    "new node must start on a fresh line when existing has no trailing newline: {s:?}",
                );
                // The round-trip must still be valid KDL.
                KdlDocument::parse_v1(&s).expect("output must be valid KDL v1");
            }
            other => panic!("expected Added, got {other:?}"),
        }
    }

    #[test]
    fn append_after_multiple_activities_appends_when_no_match() {
        // Exercises iteration through all top-level `activity` nodes:
        // when none match, we must still append, not short-circuit.
        let existing = "activity \"Home\"\nactivity \"Office\"\n";
        let outcome = append_activity_if_absent(existing, "Work").expect("append succeeds");
        match outcome {
            AppendOutcome::Added(s) => {
                assert!(s.contains("\"Home\""), "Home must survive: {s:?}");
                assert!(s.contains("\"Office\""), "Office must survive: {s:?}");
                assert!(s.contains("\"Work\""), "Work must be appended: {s:?}");
            }
            other => panic!("expected Added, got {other:?}"),
        }
    }

    #[test]
    fn non_string_first_entry_returns_invalid_data() {
        // C2: `activity 42` has a non-String first entry; we must
        // reject with InvalidData rather than silently skipping it.
        let existing = "activity 42\n";
        let err =
            append_activity_if_absent(existing, "Work").expect_err("must fail on non-String entry");
        match err {
            CliError::ConfigEdit(io) => {
                assert_eq!(io.kind(), io::ErrorKind::InvalidData);
                assert!(
                    io.to_string().contains("not a string"),
                    "error message must describe non-String entry: {io}",
                );
            }
            other => panic!("expected ConfigEdit(InvalidData), got {other:?}"),
        }
    }

    #[test]
    fn append_with_special_chars_in_name_quotes_correctly() {
        // KDL quotes string entries — names containing spaces or quotes
        // must round-trip through the serializer without breaking the
        // surrounding document.
        let outcome = append_activity_if_absent("", "Hello \"World\"").expect("append succeeds");
        match outcome {
            AppendOutcome::Added(s) => {
                let reparsed = KdlDocument::parse_v1(&s).expect("serialized output reparses");
                let last = reparsed
                    .nodes()
                    .last()
                    .expect("at least one node after append");
                assert_eq!(last.name().value(), "activity");
                let entry = last.entries().first().expect("name entry present");
                match entry.value() {
                    KdlValue::String(s) => assert_eq!(s, "Hello \"World\""),
                    other => panic!("expected String entry, got {other:?}"),
                }
            }
            other => panic!("expected Added, got {other:?}"),
        }
    }

    #[test]
    fn special_char_newline_in_name_round_trips() {
        let name = "Work\nHome";
        let outcome = append_activity_if_absent("", name).expect("append succeeds");
        match outcome {
            AppendOutcome::Added(s) => {
                let reparsed = KdlDocument::parse_v1(&s).expect("must reparse as KDL v1");
                let entry = reparsed
                    .nodes()
                    .last()
                    .expect("node present")
                    .entries()
                    .first()
                    .expect("entry present");
                match entry.value() {
                    KdlValue::String(v) => assert_eq!(v, name),
                    other => panic!("expected String, got {other:?}"),
                }
            }
            other => panic!("expected Added, got {other:?}"),
        }
    }

    #[test]
    fn special_char_tab_in_name_round_trips() {
        let name = "Work\tHome";
        let outcome = append_activity_if_absent("", name).expect("append succeeds");
        match outcome {
            AppendOutcome::Added(s) => {
                let reparsed = KdlDocument::parse_v1(&s).expect("must reparse as KDL v1");
                let entry = reparsed
                    .nodes()
                    .last()
                    .expect("node present")
                    .entries()
                    .first()
                    .expect("entry present");
                match entry.value() {
                    KdlValue::String(v) => assert_eq!(v, name),
                    other => panic!("expected String, got {other:?}"),
                }
            }
            other => panic!("expected Added, got {other:?}"),
        }
    }

    #[test]
    fn special_char_backslash_in_name_round_trips() {
        let name = "Work\\Home";
        let outcome = append_activity_if_absent("", name).expect("append succeeds");
        match outcome {
            AppendOutcome::Added(s) => {
                let reparsed = KdlDocument::parse_v1(&s).expect("must reparse as KDL v1");
                let entry = reparsed
                    .nodes()
                    .last()
                    .expect("node present")
                    .entries()
                    .first()
                    .expect("entry present");
                match entry.value() {
                    KdlValue::String(v) => assert_eq!(v, name),
                    other => panic!("expected String, got {other:?}"),
                }
            }
            other => panic!("expected Added, got {other:?}"),
        }
    }

    #[test]
    fn special_char_carriage_return_in_name_round_trips() {
        let name = "Work\rHome";
        let outcome = append_activity_if_absent("", name).expect("append succeeds");
        match outcome {
            AppendOutcome::Added(s) => {
                let reparsed = KdlDocument::parse_v1(&s).expect("must reparse as KDL v1");
                let entry = reparsed
                    .nodes()
                    .last()
                    .expect("node present")
                    .entries()
                    .first()
                    .expect("entry present");
                match entry.value() {
                    KdlValue::String(v) => assert_eq!(v, name),
                    other => panic!("expected String, got {other:?}"),
                }
            }
            other => panic!("expected Added, got {other:?}"),
        }
    }

    // ---- run ----

    #[test]
    fn save_already_declared_short_circuits_no_ipc() {
        // If the name is already in config, run() must NOT issue any
        // IPC call. The real guard here is the MockClient having no
        // expectations queued: a stray send() would panic with
        // "unexpected IPC request". The `remaining_count == 0`
        // assertion is an additional explicit statement of intent —
        // it catches a scenario where send() was called but consumed
        // an expectation it shouldn't have had.
        let dir = TempDir::new().expect("tempdir");
        let path = write_config(dir.path(), "activity \"Work\"\n");
        let mut client = MockClient::new();
        let resolver = FixedPath(path.clone());
        run(&mut client, "Work", &resolver).expect("idempotent path returns Ok");
        assert_eq!(
            client.remaining_count(),
            0,
            "already-declared path must leave the IPC queue empty",
        );
        // The file content must be unchanged.
        let after = std::fs::read_to_string(&path).expect("read after");
        assert_eq!(after, "activity \"Work\"\n");
    }

    #[test]
    fn save_writes_file_then_dispatches_load_config_file() {
        let dir = TempDir::new().expect("tempdir");
        let path = write_config(dir.path(), "");
        let mut client = MockClient::new();
        client.expect(load_config_req(), Reply::Ok(Response::Handled));
        let resolver = FixedPath(path.clone());
        run(&mut client, "Work", &resolver).expect("happy path succeeds");
        client.assert_consumed_in_order();
        let after = std::fs::read_to_string(&path).expect("read after");
        assert!(
            after.contains("activity") && after.contains("\"Work\""),
            "config must contain the new activity declaration: {after:?}",
        );
        // Round-trip: the written content must be valid KDL v1 and
        // the new node must carry the exact string value.
        let reparsed = KdlDocument::parse_v1(&after).expect("written content must be valid KDL v1");
        let activity_node = reparsed
            .nodes()
            .iter()
            .find(|n| n.name().value() == "activity")
            .expect("parsed document must contain an `activity` node");
        let first_entry = activity_node
            .entries()
            .first()
            .expect("activity node must have a name entry");
        match first_entry.value() {
            KdlValue::String(s) => assert_eq!(s, "Work", "activity name must round-trip exactly"),
            other => panic!("expected String entry, got {other:?}"),
        }
    }

    #[test]
    fn save_load_config_server_error_is_malformed() {
        let dir = TempDir::new().expect("tempdir");
        let path = write_config(dir.path(), "");
        let mut client = MockClient::new();
        client.expect(
            load_config_req(),
            Reply::Err("compositor reload failed: parse error".to_owned()),
        );
        let resolver = FixedPath(path);
        let err = run(&mut client, "Work", &resolver).expect_err("server error must fail");
        let cli_err = err
            .chain()
            .find_map(|e| e.downcast_ref::<CliError>())
            .expect("CliError must be in chain");
        match cli_err {
            CliError::MalformedResponse(MalformedResponseSource::Server(msg)) => {
                assert!(
                    msg.contains("compositor reload failed"),
                    "server message must be surfaced verbatim: {msg:?}",
                );
            }
            other => panic!("expected MalformedResponse(Server), got {other:?}"),
        }
        client.assert_consumed_in_order();
    }

    #[test]
    fn save_load_config_wrong_variant_is_malformed() {
        let dir = TempDir::new().expect("tempdir");
        let path = write_config(dir.path(), "");
        let mut client = MockClient::new();
        client.expect(load_config_req(), Reply::Ok(Response::Version("v".into())));
        let resolver = FixedPath(path);
        let err = run(&mut client, "Work", &resolver).expect_err("wrong variant must fail");
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
                assert!(
                    got.contains("Version"),
                    "got must name the Response::Version variant: {got:?}",
                );
            }
            other => panic!("expected WrongVariant, got {other:?}"),
        }
        client.assert_consumed_in_order();
    }

    #[test]
    fn save_empty_name_is_usage_64() {
        // Pre-validation: empty / whitespace-only name must short-circuit
        // BEFORE any filesystem access. The resolver is given a path that
        // does not exist; if pre-validation works, run() never touches it.
        let mut client = MockClient::new();
        let resolver = FixedPath(PathBuf::from("/this/path/must/not/be/touched/by/save"));
        let err = run(&mut client, "   ", &resolver).expect_err("empty-after-trim name must fail");
        let cli_err = err
            .chain()
            .find_map(|e| e.downcast_ref::<CliError>())
            .expect("CliError must be in chain");
        match cli_err {
            CliError::Usage(msg) => {
                assert!(
                    msg.contains("activity name must not be empty or whitespace-only"),
                    "Usage message must describe the constraint: {msg:?}",
                );
                assert!(
                    msg.contains("   "),
                    "Usage message must echo what was typed: {msg:?}",
                );
            }
            other => panic!("expected Usage, got {other:?}"),
        }
        assert_eq!(cli_err.exit_code(), 64);
        assert_eq!(client.remaining_count(), 0);
    }

    // ---- RealConfigPaths::resolve ----

    #[test]
    fn real_config_paths_niri_config_set_returns_that_path() {
        let _lock = ENV_LOCK.lock().expect("env lock not poisoned");
        // SAFETY: ENV_LOCK serializes all JIJI_CONFIG mutations in
        // this test module.
        unsafe { std::env::set_var(JIJI_CONFIG_ENV, "/tmp/my-custom.kdl") };
        let result = RealConfigPaths.resolve();
        unsafe { std::env::remove_var(JIJI_CONFIG_ENV) };
        let path = result.expect("JIJI_CONFIG set must resolve to that path");
        assert_eq!(path, PathBuf::from("/tmp/my-custom.kdl"));
    }

    #[test]
    fn real_config_paths_niri_config_empty_falls_through_to_project_dirs() {
        // (T3a) Empty `$JIJI_CONFIG` must be treated as unset and
        // fall through to the ProjectDirs branch.
        let _lock = ENV_LOCK.lock().expect("env lock not poisoned");
        // SAFETY: ENV_LOCK serializes JIJI_CONFIG mutations.
        unsafe { std::env::set_var(JIJI_CONFIG_ENV, "") };
        let result = RealConfigPaths.resolve();
        unsafe { std::env::remove_var(JIJI_CONFIG_ENV) };
        // Whether ProjectDirs succeeds or not depends on the host's
        // $HOME. Either outcome is valid here; what matters is that
        // we did NOT return "/this/path" (i.e. the empty string was
        // not used verbatim as a path).
        match result {
            Ok(path) => {
                assert_ne!(
                    path,
                    PathBuf::from(""),
                    "empty JIJI_CONFIG must not produce an empty path",
                );
                assert!(
                    path.ends_with("config.kdl"),
                    "ProjectDirs fallback must end with config.kdl: {path:?}",
                );
            }
            Err(CliError::ConfigEdit(_)) => {
                // ProjectDirs returned None; acceptable on a headless CI host.
            }
            Err(other) => panic!("unexpected error variant: {other:?}"),
        }
    }

    #[test]
    fn real_config_paths_niri_config_unset_uses_project_dirs() {
        // (T3b) When `$JIJI_CONFIG` is unset, resolve must try
        // ProjectDirs and either succeed with a `config.kdl` path or
        // return ConfigEdit(NotFound) on a host without $HOME.
        let _lock = ENV_LOCK.lock().expect("env lock not poisoned");
        // SAFETY: ENV_LOCK serializes JIJI_CONFIG mutations.
        let had_env = std::env::var_os(JIJI_CONFIG_ENV);
        unsafe { std::env::remove_var(JIJI_CONFIG_ENV) };
        let result = RealConfigPaths.resolve();
        // Restore if it was set.
        if let Some(v) = had_env {
            unsafe { std::env::set_var(JIJI_CONFIG_ENV, v) };
        }
        match result {
            Ok(path) => {
                assert!(
                    path.ends_with("config.kdl"),
                    "ProjectDirs fallback must end with config.kdl: {path:?}",
                );
            }
            Err(CliError::ConfigEdit(io)) => {
                assert_eq!(
                    io.kind(),
                    io::ErrorKind::NotFound,
                    "ProjectDirs failure must map to NotFound: {io}",
                );
            }
            Err(other) => panic!("unexpected error variant: {other:?}"),
        }
    }
}
