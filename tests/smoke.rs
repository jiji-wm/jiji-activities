//! `--ignored`-gated end-to-end smoke tests against a live niri compositor.
//!
//! These tests are **not** run by `cargo test`. They require:
//!
//! - A running niri compositor exposing its IPC socket at `$NIRI_SOCKET`.
//! - `niri` on `$PATH` (used as a side-effect verifier via `niri msg --json`).
//!
//! Run cadence (operator):
//!
//! ```sh
//! cargo test --test smoke -- --ignored --test-threads=1
//! ```
//!
//! Each test asserts a *side effect* (post-IPC state observable via a
//! follow-up `niri msg --json` round-trip), not just a process exit code —
//! the default `tests/cli.rs` lane already covers exit-code-only assertions.
//!
//! Tests create runtime activities under a `__nact_smoke_<test>_<pid>_<nanos>`
//! prefix so they are isolated against existing user state and against each
//! other when re-run. Tests that create activities install a best-effort
//! `RuntimeActivityGuard` so an assertion-panic mid-test still triggers a
//! remove on unwind; if cleanup fails (compositor unreachable, etc.), a stderr
//! breadcrumb is logged but the test does not fail.
//!
//! **Stranded activity recovery.** If a test panics before its cleanup guard
//! runs (or cleanup itself fails), runtime activities with the
//! `__nact_smoke_` prefix remain in the compositor. Inspect via
//! `jiji-activities list | grep __nact_smoke` and remove manually with
//! `jiji-activities remove <name>`. They do not persist across a compositor
//! restart (runtime activities by construction).

use std::process::Command as StdCommand;
use std::time::{SystemTime, UNIX_EPOCH};

use assert_cmd::Command;
use serde_json::Value;

const BIN: &str = "jiji-activities";
const SMOKE_PREFIX: &str = "__nact_smoke_";

/// Returns `Some(socket)` if the live-niri precondition holds, `None`
/// otherwise. Callers should early-return on `None` after logging a
/// skip breadcrumb — `#[ignore]`-gated tests do not have a native
/// skip mechanism, so we simulate one by returning a passing test.
fn require_live_niri() -> Option<String> {
    let socket = match std::env::var("NIRI_SOCKET") {
        Ok(s) if !s.is_empty() => s,
        _ => {
            eprintln!(
                "smoke: SKIP — $NIRI_SOCKET unset; smoke tests require a running niri compositor",
            );
            return None;
        }
    };
    // Probe with the `activities` subcommand specifically — this both
    // verifies socket reachability AND that the `niri` binary on `$PATH`
    // is fork-side (carries the activities IPC surface). An upstream
    // `niri msg` binary lacks the `activities` subcommand and reports
    // "unrecognized subcommand 'activities'" — surfacing this as a SKIP
    // (rather than a hard failure inside an individual test) gives the
    // operator a single clear diagnostic.
    let probe = StdCommand::new("niri")
        .args(["msg", "--json", "activities"])
        .env("NIRI_SOCKET", &socket)
        .output();
    match probe {
        Ok(out) if out.status.success() => Some(socket),
        Ok(out) => {
            eprintln!(
                "smoke: SKIP — `niri msg activities` failed ({}); compositor not reachable, or the `niri` binary on $PATH is not the fork with activities support. Stderr: {}",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim(),
            );
            None
        }
        Err(e) => {
            eprintln!("smoke: SKIP — could not exec `niri`: {e}");
            None
        }
    }
}

/// Shell out to `niri msg --json <args>` and decode stdout as JSON.
///
/// Panics with a clear message naming the failing command and stderr on
/// any failure — these are smoke tests, so a broken precondition is
/// a hard fail (after `require_live_niri` has gated the test).
fn niri_msg(socket: &str, args: &[&str]) -> Value {
    let out = StdCommand::new("niri")
        .arg("msg")
        .arg("--json")
        .args(args)
        .env("NIRI_SOCKET", socket)
        .output()
        .unwrap_or_else(|e| panic!("exec `niri msg --json {}` failed: {e}", args.join(" ")));
    if !out.status.success() {
        panic!(
            "`niri msg --json {}` exited {}; stderr: {}",
            args.join(" "),
            out.status,
            String::from_utf8_lossy(&out.stderr),
        );
    }
    if out.stdout.is_empty() {
        panic!(
            "`niri msg --json {}` stdout was empty (expected JSON)",
            args.join(" "),
        );
    }
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "`niri msg --json {}` stdout was not JSON ({e}); raw: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stdout),
        )
    })
}

/// Build an `assert_cmd::Command` for the test-compiled `jiji-activities`
/// binary with `$NIRI_SOCKET` pre-populated.
///
/// `assert_cmd::Command` does not strip env by default, but inheriting
/// `$NIRI_SOCKET` explicitly keeps the binary-under-test connecting to
/// the same compositor the test helper queries — even if future
/// versions of `assert_cmd` change defaults.
fn nact(socket: &str) -> Command {
    let mut c = Command::cargo_bin(BIN).expect("locate jiji-activities binary");
    c.env("NIRI_SOCKET", socket);
    c
}

/// Generate a unique, per-test runtime-activity name.
fn unique_name(test: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("SystemTime before UNIX_EPOCH — clock not set")
        .as_nanos();
    format!("{SMOKE_PREFIX}{test}_{}_{}", std::process::id(), nanos)
}

/// Look up an activity by name in `niri msg --json activities` output.
fn find_activity<'a>(activities: &'a Value, name: &str) -> Option<&'a Value> {
    activities
        .as_array()?
        .iter()
        .find(|a| a.get("name").and_then(Value::as_str) == Some(name))
}

/// Best-effort cleanup RAII guard. Removes the named activity in `Drop`.
///
/// Spawn failures (binary not found) log a breadcrumb to stderr. Non-zero
/// subprocess exit codes (e.g. exit 66 when the activity was already removed
/// by the test's explicit `remove` call) are silently accepted — best-effort
/// cleanup does not distinguish "already gone" from success. The test's own
/// assertion is the source of truth; a `Drop` that panics during unwind would
/// mask the original failure.
struct RuntimeActivityGuard {
    name: String,
    socket: String,
}

impl RuntimeActivityGuard {
    fn new(socket: &str, name: String) -> Self {
        Self {
            name,
            socket: socket.to_string(),
        }
    }
}

impl Drop for RuntimeActivityGuard {
    fn drop(&mut self) {
        let mut cmd = match Command::cargo_bin(BIN) {
            Ok(c) => c,
            Err(e) => {
                eprintln!(
                    "smoke cleanup: could not locate jiji-activities binary for cleanup of `{}`: {e}",
                    self.name,
                );
                return;
            }
        };
        if let Err(e) = cmd
            .env("NIRI_SOCKET", &self.socket)
            .args(["remove", &self.name])
            .output()
        {
            eprintln!(
                "smoke cleanup: could not remove activity `{}`: {e}",
                self.name,
            );
        }
    }
}

#[test]
#[ignore]
fn smoke_list_succeeds() {
    let Some(socket) = require_live_niri() else {
        return;
    };
    let assert = nact(&socket).arg("list").assert().success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).expect("utf-8 stdout");
    assert!(
        !stdout.trim().is_empty(),
        "`jiji-activities list` stdout was empty; live compositor must have at least one activity",
    );
}

#[test]
#[ignore]
fn smoke_list_json_parses() {
    let Some(socket) = require_live_niri() else {
        return;
    };
    let assert = nact(&socket).args(["list", "--json"]).assert().success();
    let stdout = assert.get_output().stdout.clone();
    let v: Value = serde_json::from_slice(&stdout).expect("--json output parses as JSON");
    let envelope = v.as_object().expect("--json envelope is a JSON object");
    let schema_version = envelope
        .get("schema_version")
        .expect("envelope.schema_version present")
        .as_u64()
        .expect("schema_version is u64");
    assert_eq!(
        schema_version, 1,
        "schema_version must be 1 (envelope stability contract)",
    );
    envelope
        .get("activities")
        .expect("envelope.activities present")
        .as_array()
        .expect("envelope.activities is array");
}

#[test]
#[ignore]
fn smoke_create_then_remove() {
    let Some(socket) = require_live_niri() else {
        return;
    };
    let name = unique_name("create_remove");
    // Panic-path backstop: if the post-create assertion or the explicit remove
    // below fails, Drop still issues the remove so the activity is not
    // stranded. The explicit `nact … remove` at L244 runs first (drop order)
    // and is what exercises the subcommand.
    let _guard = RuntimeActivityGuard::new(&socket, name.clone());

    // Create the activity.
    nact(&socket).args(["create", &name]).assert().success();

    // Side-effect assertion: the activity now appears in `niri msg activities`.
    let activities = niri_msg(&socket, &["activities"]);
    assert!(
        find_activity(&activities, &name).is_some(),
        "after `create`, activity `{name}` must appear in niri msg output; got: {activities}",
    );

    // Remove the activity.
    nact(&socket).args(["remove", &name]).assert().success();

    // Side-effect assertion: the activity is gone.
    let activities_after = niri_msg(&socket, &["activities"]);
    assert!(
        find_activity(&activities_after, &name).is_none(),
        "after `remove`, activity `{name}` must NOT appear in niri msg output; got: {activities_after}",
    );
}

#[test]
#[ignore]
fn smoke_create_collision_exits_73() {
    let Some(socket) = require_live_niri() else {
        return;
    };
    let name = unique_name("collision");
    let _guard = RuntimeActivityGuard::new(&socket, name.clone());

    // First create succeeds.
    nact(&socket).args(["create", &name]).assert().success();

    // Second create with the same name collides with exit 73 + stderr prefix
    // anchor. The README documents the prefix `cannot create activity:` as
    // the stable disambiguator for exit-code 73 in the CantCreate path; this
    // test pins that anchor at the binary boundary against a live niri.
    let assert = nact(&socket).args(["create", &name]).assert().code(73);
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).expect("utf-8 stderr");
    assert!(
        stderr.contains("cannot create activity:"),
        "collision stderr must contain `cannot create activity:` prefix anchor; got: {stderr:?}",
    );
}

#[test]
#[ignore]
fn smoke_switch_round_trip() {
    let Some(socket) = require_live_niri() else {
        return;
    };

    // Read pre-state: which activity is currently focused.
    let activities = niri_msg(&socket, &["activities"]);
    let arr = activities
        .as_array()
        .unwrap_or_else(|| panic!("activities response is array; got: {activities}"));
    if arr.len() < 2 {
        eprintln!(
            "smoke_switch_round_trip: SKIP — need >=2 activities for a switch round-trip, got {}",
            arr.len(),
        );
        return;
    }
    let current = arr
        .iter()
        .find(|a| a.get("is_active").and_then(Value::as_bool) == Some(true))
        .unwrap_or_else(|| panic!("exactly one activity must be active; got: {activities}"));
    let current_name = current
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("active activity has a name; got: {current}"))
        .to_string();
    let other = arr
        .iter()
        .find(|a| {
            a.get("is_active").and_then(Value::as_bool) == Some(false)
                && a.get("name").and_then(Value::as_str).is_some()
        })
        .unwrap_or_else(|| {
            panic!("must have a non-active activity for round-trip; got: {activities}")
        });
    let other_name = other
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("non-active activity has a name; got: {other}"))
        .to_string();

    // Switch to the other.
    nact(&socket)
        .args(["switch", &other_name])
        .assert()
        .success();

    // Side-effect assertion: focus moved.
    let after = niri_msg(&socket, &["activities"]);
    let now_active = after
        .as_array()
        .and_then(|arr| {
            arr.iter()
                .find(|a| a.get("is_active").and_then(Value::as_bool) == Some(true))
        })
        .and_then(|a| a.get("name").and_then(Value::as_str))
        .map(str::to_string);
    assert_eq!(
        now_active.as_deref(),
        Some(other_name.as_str()),
        "after switch, `{other_name}` must be active; got {now_active:?}",
    );

    // Restore.
    nact(&socket)
        .args(["switch", &current_name])
        .assert()
        .success();
}

#[test]
#[ignore]
fn smoke_switch_previous() {
    let Some(socket) = require_live_niri() else {
        return;
    };

    let output = nact(&socket)
        .arg("switch-previous")
        .output()
        .expect("spawn jiji-activities");

    // Two valid outcomes:
    // 1. Exit 0 with an observable focus change (a previous activity existed).
    // 2. A non-zero exit code because no previous activity exists (e.g. since
    //    the compositor session start). We accept *one* of these, not both.
    if output.status.success() {
        let after = niri_msg(&socket, &["activities"]);
        let after_active = after
            .as_array()
            .and_then(|arr| {
                arr.iter()
                    .find(|a| a.get("is_active").and_then(Value::as_bool) == Some(true))
            })
            .and_then(|a| a.get("name").and_then(Value::as_str))
            .map(str::to_string);
        // If success, either focus changed, OR (rare but valid: the previous
        // activity *is* the current one — silent no-op semantics). Accept
        // either; pin only that the success branch produced a coherent state.
        assert!(
            after_active.is_some(),
            "after switch-previous (exit 0), some activity must be active; got {after_active:?}",
        );
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // We don't pin the exact exit code — `switch-previous` against a
        // session with no previous activity may surface as exit 0 (silent
        // no-op) or as a non-zero error depending on the compositor's pointer
        // state. Pin only that some non-zero exit produces a
        // `jiji-activities:`-prefixed stderr breadcrumb.
        assert!(
            stderr.contains("jiji-activities:"),
            "switch-previous non-zero exit must produce a jiji-activities stderr breadcrumb; got: {stderr:?}",
        );
    }
}
