//! Integration tests for the `fuzzel`-backed **single-select** picker via
//! a fuzzel shim binary.
//!
//! These tests prove the spawn-and-pipe flow end-to-end without
//! depending on a real `fuzzel` install on the test host. The strategy:
//!
//! 1. Create a per-test tempdir.
//! 2. Write a `bash` script named `fuzzel` inside it that does whatever
//!    behaviour the test wants (cancel, select, etc.).
//! 3. Spawn `jiji-activities` with `$PATH` set to *only* that tempdir
//!    (via `env_clear` + explicit `env("PATH", ...)`) so the shim is the
//!    only `fuzzel` the binary can resolve.
//! 4. For tests that need the IPC `Request::Activities` round-trip to
//!    succeed (so the single-select picker is actually reached), bind a
//!    one-shot Unix listener and point `$JIJI_SOCKET` at it. The listener
//!    replies with a fixed `Response::Activities` payload then exits.
//!
//! `env_clear` is load-bearing — leaving the parent's `$PATH` in place
//! would let the real `fuzzel` on the developer's machine shadow the
//! shim and turn these into integration tests against a live `fuzzel`.
//! See `rofi_shim.rs` for the parallel multi-select picker tests.

use std::fs;
use std::io::{BufRead, BufReader, Write as _};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;

use assert_cmd::Command;
use predicates::prelude::PredicateBooleanExt;
use predicates::str::contains;

const BIN: &str = "jiji-activities";

/// Per-test unique tempdir under `/tmp`. PID + counter keeps concurrent
/// `cargo test` jobs disjoint. Avoids pulling in `tempfile` as a
/// dev-dep — the directory is created and removed on `Drop`.
struct ShimDir {
    path: PathBuf,
}

impl ShimDir {
    fn new(tag: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "jiji-activities-shim-{}-{}-{}",
            std::process::id(),
            n,
            tag,
        ));
        fs::create_dir_all(&path).expect("create shim tempdir");
        ShimDir { path }
    }

    /// Writes an executable `sh` script named `fuzzel` inside this
    /// tempdir. `body` is the script body *without* the shebang.
    ///
    /// Uses `#!/bin/sh` directly (absolute path) so the kernel can
    /// resolve the interpreter without consulting `$PATH`. The test
    /// harness deliberately constrains `$PATH` to the shim tempdir
    /// (so the real `fuzzel` can't be picked up by `which_in_path`),
    /// which would break `#!/usr/bin/env bash` shebangs — `env`
    /// resolves the interpreter by walking `$PATH`. POSIX sh builtins
    /// (`exit`, `printf`) are sufficient for the shim bodies we need.
    fn install_fuzzel(&self, body: &str) {
        let script = self.path.join("fuzzel");
        let mut f = fs::File::create(&script).expect("create shim script");
        writeln!(f, "#!/bin/sh").expect("write shebang");
        f.write_all(body.as_bytes()).expect("write body");
        let mut perms = f.metadata().expect("script metadata").permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&script, perms).expect("chmod +x shim");
    }

    fn as_path(&self) -> &Path {
        &self.path
    }
}

impl Drop for ShimDir {
    fn drop(&mut self) {
        // Best-effort cleanup; don't fail the test if the directory was
        // already pruned by a sibling.
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// One-shot Unix-socket listener that answers exactly one
/// `Request::Activities` with a hard-coded `Response::Activities` reply
/// containing two activities, then exits. Used by picker shim tests
/// that need the `run_picker` IPC round-trip to succeed so the picker
/// is actually invoked.
///
/// The path is returned so the caller can point `$JIJI_SOCKET` at it.
fn spawn_one_shot_activities_listener(tag: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "jiji-activities-shim-sock-{}-{}-{}.sock",
        std::process::id(),
        n,
        tag,
    ));
    // Best-effort prune so a re-run after a crashed previous job
    // doesn't trip the bind.
    let _ = fs::remove_file(&path);
    let listener = UnixListener::bind(&path).expect("bind one-shot socket");
    let path_clone = path.clone();
    thread::spawn(move || {
        // Accept one connection, read one request line, reply with a
        // fixed Activities payload. Best-effort: don't panic the test
        // thread if the client hangs up early.
        if let Ok((mut sock, _)) = listener.accept() {
            let read_clone = sock.try_clone().expect("clone socket");
            let mut reader = BufReader::new(read_clone);
            let mut req = String::new();
            let _ = reader.read_line(&mut req);
            // Two activities: "Work" (focused) and "Personal".
            // Field ordering follows the niri-ipc `Activity` struct.
            let reply = "{\"Ok\":{\"Activities\":[\
                 {\"id\":1,\"name\":\"Work\",\"is_config_declared\":true,\"is_active\":true,\"is_urgent\":false,\"last_active_seq\":2},\
                 {\"id\":2,\"name\":\"Personal\",\"is_config_declared\":true,\"is_active\":false,\"is_urgent\":false,\"last_active_seq\":1}\
                 ]}}\n";
            let _ = sock.write_all(reply.as_bytes());
        }
        let _ = fs::remove_file(&path_clone);
    });
    // Spin up to ~500 ms waiting for the bind to be observable. In
    // practice this returns on the first poll.
    for _ in 0..50 {
        if path.exists() {
            break;
        }
        thread::sleep(std::time::Duration::from_millis(10));
    }
    path
}

#[test]
fn fuzzel_cancel_exits_0() {
    // Full pipe-and-read flow: socket listener answers Activities, then
    // the single-select fuzzel shim simulates a user dismissal (exit 1
    // with empty stdout). The CLI must classify that as cancellation and
    // exit 0 silently.
    let shim = ShimDir::new("cancel");
    // Drain stdin to EOF before exiting so the binary's stdin write
    // does not race with the shim's exit and produce EPIPE.
    shim.install_fuzzel("while IFS= read -r _line; do :; done\nexit 1\n");
    let sock = spawn_one_shot_activities_listener("cancel");

    Command::cargo_bin(BIN)
        .unwrap()
        .arg("switch")
        .env_clear()
        .env("PATH", shim.as_path())
        .env("JIJI_SOCKET", &sock)
        .assert()
        .code(0);
}

#[test]
fn fuzzel_select_then_second_ipc_fails_exits_69() {
    // Full pipe-and-read flow: socket listener answers Activities, the
    // picker spawns the shim, which prints "Work" and exits 0 — picker
    // returns Selected("Work"). `run_picker` then dispatches a SECOND
    // IPC call (`SwitchActivity`); the one-shot listener already
    // closed, so that second call hits a dead socket and exit 69
    // surfaces. `$JIJI_SOCKET` IS set, but the post-pick socket is
    // dead. The exit-code contract is what's pinned: select + dead
    // second IPC → 69.
    let shim = ShimDir::new("select");
    shim.install_fuzzel("printf 'Work\\n'\nexit 0\n");
    let sock = spawn_one_shot_activities_listener("select");

    Command::cargo_bin(BIN)
        .unwrap()
        .arg("switch")
        .env_clear()
        .env("PATH", shim.as_path())
        .env("JIJI_SOCKET", &sock)
        .assert()
        .code(69);
}

#[test]
fn fuzzel_stdin_payload_reaches_shim() {
    // Pins that `pick_one` actually writes the activity-name payload to
    // fuzzel's stdin. A regression that dropped `stdin.write_all(...)` or
    // replaced stdin with `Stdio::null()` would produce an empty capture
    // file and fail this test.
    //
    // Strategy: the shim reads its stdin and writes it to
    // `$SHIM_STDIN_CAPTURE` (a sidecar file the test provides via env).
    // The shim then cancels (exit 1 + no stdout) so the CLI exits 0
    // cleanly without needing a second IPC call. The test reads the
    // capture file and asserts the payload matches the expected format:
    // one name per line, trailing newline, focused activity first.
    let shim = ShimDir::new("stdin-capture");
    let capture = shim.as_path().join("stdin.cap");
    // Write stdin to $SHIM_STDIN_CAPTURE using a shell read-loop so the
    // shim needs no external commands (PATH is restricted to the shim dir,
    // so `cat` and friends are unavailable). Then cancel (exit 1).
    shim.install_fuzzel(
        "while IFS= read -r line; do printf '%s\\n' \"$line\"; done > \"$SHIM_STDIN_CAPTURE\"\nexit 1\n",
    );
    let sock = spawn_one_shot_activities_listener("stdin-capture");

    Command::cargo_bin(BIN)
        .unwrap()
        .arg("switch")
        .env_clear()
        .env("PATH", shim.as_path())
        .env("JIJI_SOCKET", &sock)
        .env("SHIM_STDIN_CAPTURE", &capture)
        .assert()
        .code(0);

    // The one-shot listener sends Work (active, seq=2) + Personal (seq=1).
    // `names_for_switch` (MRU default) pulls the active "Work" into
    // `current`; "Personal" leads the rows (preselected) and the current
    // activity rides along as a marked last row on its own line.
    let payload = fs::read_to_string(&capture)
        .expect("shim must have written stdin capture file (file missing)");
    assert_eq!(
        payload, "Personal\nWork (current)\n",
        "stdin payload must list non-active names first and the marked current activity last, one per line with trailing newline; got: {payload:?}",
    );
}

#[test]
fn fuzzel_prompt_arg_is_switch_prompt() {
    // Pins that `pick_one` passes `--prompt "Switch to activity:"` to
    // fuzzel. A regression that dropped or misspelled the prompt would
    // not be caught by the cancel/select tests (which ignore args). The
    // shim writes `$@` to `$SHIM_ARGS_CAPTURE`, then cancels.
    let shim = ShimDir::new("args-capture");
    let capture = shim.as_path().join("args.cap");
    // Drain stdin before capturing args so the binary's stdin write does
    // not race with the shim's exit and produce EPIPE.
    shim.install_fuzzel("while IFS= read -r _line; do :; done\nprintf '%s\\n' \"$@\" > \"$SHIM_ARGS_CAPTURE\"\nexit 1\n");
    let sock = spawn_one_shot_activities_listener("args-capture");

    Command::cargo_bin(BIN)
        .unwrap()
        .arg("switch")
        .env_clear()
        .env("PATH", shim.as_path())
        .env("JIJI_SOCKET", &sock)
        .env("SHIM_ARGS_CAPTURE", &capture)
        .assert()
        .code(0);

    let args = fs::read_to_string(&capture).expect("shim must have written args capture file");
    // fuzzel is called as: fuzzel --dmenu --prompt "Switch to activity:"
    // The shim writes each arg on its own line via `printf '%s\n' "$@"`.
    assert!(
        args.contains("--dmenu"),
        "--dmenu flag must be present in fuzzel args: {args:?}",
    );
    assert!(
        args.contains("--prompt"),
        "--prompt flag must be present in fuzzel args: {args:?}",
    );
    // The current activity is surfaced as a marked row, not in the prompt
    // (fuzzel's prompt and input share one fixed-width line), so the prompt
    // stays short.
    assert!(
        args.contains("Switch to activity:"),
        "prompt value must be 'Switch to activity:' in fuzzel args: {args:?}",
    );
    // Dynamic width: pick_one passes --width so context-bearing rows are
    // never truncated by fuzzel's 30-character default.
    assert!(
        args.contains("--width"),
        "--width flag must be present in fuzzel args: {args:?}",
    );
}

#[test]
fn fuzzel_missing_from_path_exits_69() {
    // `$PATH` points at an empty tempdir — no `fuzzel` binary there.
    // `ensure_available()` (called from `cmd_switch` BEFORE any IPC
    // round-trip) returns PickerUnavailable with the canonical
    // missing-fuzzel message. Exit code 69 with stderr naming `fuzzel`.
    //
    // `$JIJI_SOCKET` is unset deliberately: even with a missing socket,
    // the missing-fuzzel error must surface first because the
    // availability check runs before any IPC.
    let shim = ShimDir::new("missing");
    // Deliberately NOT installing the fuzzel script — the dir is empty.

    Command::cargo_bin(BIN)
        .unwrap()
        .arg("switch")
        .env_clear()
        .env("PATH", shim.as_path())
        .env_remove("JIJI_SOCKET")
        .assert()
        .code(69)
        .stderr(contains("picker unavailable").and(contains("fuzzel")));
}

#[test]
fn run_picker_empty_activities_warns_and_exits_zero() {
    // End-to-end pin of the empty-list UX: the one-shot listener replies
    // with an empty `Activities` payload; `run_picker` must short-circuit
    // before spawning the single-select picker (the fuzzel shim touches a
    // sentinel file on entry, and the test asserts the sentinel does NOT
    // exist afterward). Exit 0 + stderr diagnostic naming the empty-list
    // cause.
    let shim = ShimDir::new("empty-activities");
    let sentinel = shim.as_path().join("shim-invoked.sentinel");
    // Sentinel-file strategy: if the shim is ever reached it creates
    // `$SHIM_INVOKED` before doing anything else. The post-run assertion
    // that the sentinel is absent fires before any stderr/exit analysis,
    // so a regression in the short-circuit is caught even if the shim
    // later exits non-zero with empty stdout (which classify_output would
    // fold into Cancelled, masking the bug).
    shim.install_fuzzel(
        ": > \"$SHIM_INVOKED\"\nprintf 'jiji-activities BUG: picker spawned for empty list\\n' >&2\nexit 99\n",
    );
    let sock = spawn_one_shot_activities_listener_empty("empty-activities");

    Command::cargo_bin(BIN)
        .unwrap()
        .arg("switch")
        .env_clear()
        .env("PATH", shim.as_path())
        .env("JIJI_SOCKET", &sock)
        .env("SHIM_INVOKED", &sentinel)
        .assert()
        .code(0)
        .stderr(contains("no activities configured"));

    assert!(
        !sentinel.exists(),
        "fuzzel shim must NOT be invoked when the activity list is empty \
         (sentinel file {:?} was created — the empty-list short-circuit regressed)",
        sentinel,
    );
}

#[test]
fn run_picker_single_activity_warns_and_exits_zero() {
    // End-to-end pin of the single-activity UX: the one-shot listener
    // replies with a single active activity; `run_picker` must
    // short-circuit before spawning the picker (sentinel file asserts
    // fuzzel is never invoked). Exit 0 + stderr naming the single-activity
    // cause — distinct from the empty-list message.
    let shim = ShimDir::new("single-activity");
    let sentinel = shim.as_path().join("shim-invoked.sentinel");
    // Sentinel + drain discipline: the shim creates `$SHIM_INVOKED` on
    // entry and drains stdin with `cat >/dev/null` before exiting so the
    // binary's stdin write cannot produce EPIPE even if the shim is
    // unexpectedly reached.
    shim.install_fuzzel(
        ": > \"$SHIM_INVOKED\"\ncat >/dev/null\nprintf 'jiji-activities BUG: picker spawned for single-activity\\n' >&2\nexit 99\n",
    );
    let sock = spawn_one_shot_activities_listener_single("single-activity");

    Command::cargo_bin(BIN)
        .unwrap()
        .arg("switch")
        .env_clear()
        .env("PATH", shim.as_path())
        .env("JIJI_SOCKET", &sock)
        .env("SHIM_INVOKED", &sentinel)
        .assert()
        .code(0)
        .stderr(contains("only the active activity exists"));

    assert!(
        !sentinel.exists(),
        "fuzzel shim must NOT be invoked when only one (active) activity exists \
         (sentinel file {:?} was created — the single-activity short-circuit regressed)",
        sentinel,
    );
}

// ---- move-workspace picker tests -------------------------------------------

#[test]
fn move_workspace_picker_cancel_exits_zero() {
    // Sibling of `fuzzel_cancel_exits_0` for the `move-workspace`
    // subcommand. The fuzzel shim simulates a user dismissal (exit 1,
    // empty stdout). The CLI must classify as cancellation and exit 0
    // silently.
    let shim = ShimDir::new("mw-cancel");
    // Drain stdin to EOF before exiting so the binary's stdin write
    // does not race with the shim's exit and produce EPIPE.
    shim.install_fuzzel("while IFS= read -r _line; do :; done\nexit 1\n");
    let sock = spawn_one_shot_activities_listener("mw-cancel");

    Command::cargo_bin(BIN)
        .unwrap()
        .arg("move-workspace")
        .env_clear()
        .env("PATH", shim.as_path())
        .env("JIJI_SOCKET", &sock)
        .assert()
        .code(0);
}

#[test]
fn move_workspace_picker_select_then_second_ipc_fails_exits_69() {
    // Sibling of `fuzzel_select_then_second_ipc_fails_exits_69`. The
    // one-shot listener answers Activities; the picker spawns the
    // shim, which prints "Work" and exits 0 → picker returns
    // Selected("Work"). `run_picker` then dispatches a SECOND IPC call
    // (MoveWorkspaceToActivity); the one-shot listener already closed,
    // so that second call hits a dead socket and exit 69 surfaces.
    // The exit-code contract is what's pinned: select + dead second
    // IPC → 69.
    let shim = ShimDir::new("mw-select");
    shim.install_fuzzel("printf 'Work\\n'\nexit 0\n");
    let sock = spawn_one_shot_activities_listener("mw-select");

    Command::cargo_bin(BIN)
        .unwrap()
        .arg("move-workspace")
        .env_clear()
        .env("PATH", shim.as_path())
        .env("JIJI_SOCKET", &sock)
        .assert()
        .code(69);
}

#[test]
fn move_workspace_picker_prompt_arg_is_move_prompt() {
    // Pins that `move_workspace::run_picker` passes
    // `--prompt "Move workspace to activity:"` to fuzzel. A regression
    // that re-used `switch`'s prompt would not be caught by the
    // cancel/select tests (which ignore args). The shim writes `$@` to
    // `$SHIM_ARGS_CAPTURE`, then cancels.
    let shim = ShimDir::new("mw-args");
    let capture = shim.as_path().join("args.cap");
    // Drain stdin before capturing args so the binary's stdin write does
    // not race with the shim's exit and produce EPIPE.
    shim.install_fuzzel("while IFS= read -r _line; do :; done\nprintf '%s\\n' \"$@\" > \"$SHIM_ARGS_CAPTURE\"\nexit 1\n");
    let sock = spawn_one_shot_activities_listener("mw-args");

    Command::cargo_bin(BIN)
        .unwrap()
        .arg("move-workspace")
        .env_clear()
        .env("PATH", shim.as_path())
        .env("JIJI_SOCKET", &sock)
        .env("SHIM_ARGS_CAPTURE", &capture)
        .assert()
        .code(0);

    let args = fs::read_to_string(&capture).expect("shim must have written args capture file");
    assert!(
        args.contains("--dmenu"),
        "--dmenu flag must be present in fuzzel args: {args:?}",
    );
    assert!(
        args.contains("--prompt"),
        "--prompt flag must be present in fuzzel args: {args:?}",
    );
    assert!(
        args.contains("Move workspace to activity:"),
        "prompt value must be 'Move workspace to activity:' in fuzzel args: {args:?}",
    );
}

// ---- move-window picker tests ----------------------------------------------

/// Two-stage listener helper for `move-window`. Accepts **two**
/// connections (one per IPC call — the CLI's `SocketClient` opens a
/// fresh connection per `send()`), answering one `(Request, Reply)`
/// round-trip on each. The first reply is the activities payload, the
/// second is the workspaces payload. Both are written verbatim (caller
/// supplies trailing newline). Used by the `move-window` two-stage
/// picker tests where the first IPC call (Activities) must succeed so
/// stage 1 fires, and the second IPC call (Workspaces) must succeed so
/// stage 2 fires.
fn spawn_two_shot_listener_for_move_window(
    tag: &str,
    activities_reply: &'static str,
    workspaces_reply: &'static str,
) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "jiji-activities-shim-sock-mw-{}-{}-{}.sock",
        std::process::id(),
        n,
        tag,
    ));
    let _ = fs::remove_file(&path);
    let listener = UnixListener::bind(&path).expect("bind two-shot socket");
    let path_clone = path.clone();
    thread::spawn(move || {
        // The CLI's `SocketClient` opens a fresh connection per send().
        // Accept once per request (max two), reply once each, exit.
        for reply in [activities_reply, workspaces_reply] {
            match listener.accept() {
                Ok((mut sock, _)) => {
                    let read_clone = sock.try_clone().expect("clone socket");
                    let mut reader = BufReader::new(read_clone);
                    let mut req = String::new();
                    let _ = reader.read_line(&mut req);
                    let _ = sock.write_all(reply.as_bytes());
                }
                Err(_) => break,
            }
        }
        let _ = fs::remove_file(&path_clone);
    });
    for _ in 0..50 {
        if path.exists() {
            break;
        }
        thread::sleep(std::time::Duration::from_millis(10));
    }
    path
}

/// Hard-coded `Activities` JSON reply with two activities (Work
/// focused, Personal). Used by the two-stage move-window shim tests.
const MOVE_WINDOW_ACTIVITIES_REPLY: &str = "{\"Ok\":{\"Activities\":[\
    {\"id\":1,\"name\":\"Work\",\"is_config_declared\":true,\"is_active\":true,\"is_urgent\":false,\"last_active_seq\":2},\
    {\"id\":2,\"name\":\"Personal\",\"is_config_declared\":true,\"is_active\":false,\"is_urgent\":false,\"last_active_seq\":1}\
    ]}}\n";

/// Hard-coded `Workspaces` JSON reply: one focused workspace on DP-1 in
/// activity 1, one trailing-empty workspace on DP-1 in activity 1.
const MOVE_WINDOW_WORKSPACES_REPLY: &str = "{\"Ok\":{\"Workspaces\":[\
    {\"id\":10,\"idx\":0,\"name\":null,\"output\":\"DP-1\",\"is_urgent\":false,\"is_active\":true,\"is_focused\":true,\"active_window_id\":42,\"activities\":[1],\"is_sticky\":false,\"is_in_active_activity\":true},\
    {\"id\":20,\"idx\":1,\"name\":null,\"output\":\"DP-1\",\"is_urgent\":false,\"is_active\":false,\"is_focused\":false,\"active_window_id\":null,\"activities\":[1],\"is_sticky\":false,\"is_in_active_activity\":true}\
    ]}}\n";

/// Counter-file two-stage shim body. The shim reads the counter file
/// (initial value `0` written by the test), increments it, and selects
/// its response based on the counter value: stage 1 (counter==0) uses
/// `$SHIM_STAGE1_RESPONSE`, stage 2 (counter==1) uses
/// `$SHIM_STAGE2_RESPONSE`. POSIX shell builtins only — no `cat`,
/// `touch`, `expr` — the shim's `$PATH` is restricted to the shim dir,
/// so external binaries are unavailable.
///
/// Each stage's behaviour is controlled by the corresponding env var:
/// - `select:<name>` — prints `<name>` and exits 0 (selection).
/// - `cancel` — exits 1 with empty stdout (user dismissal).
/// - `args:<path>` — writes `$@` (all fuzzel args) to `<path>`, then
///   exits 1 (cancellation); used by prompt-capture tests.
const TWO_STAGE_SHIM_BODY: &str = r#"
read counter < "$SHIM_COUNTER_FILE"
new=$(( counter + 1 ))
printf '%s' "$new" > "$SHIM_COUNTER_FILE"
if [ "$counter" = "0" ]; then
    behaviour="$SHIM_STAGE1_RESPONSE"
else
    behaviour="$SHIM_STAGE2_RESPONSE"
fi
case "$behaviour" in
    cancel)
        exit 1
        ;;
    select:*)
        printf '%s\n' "${behaviour#select:}"
        exit 0
        ;;
    args:*)
        # `args:<path>` — write args to <path>, then cancel.
        capture="${behaviour#args:}"
        printf '%s\n' "$@" > "$capture"
        exit 1
        ;;
    *)
        printf 'shim: unknown behaviour %s\n' "$behaviour" >&2
        exit 99
        ;;
esac
"#;

/// Writes the counter file initialised to `0` inside `dir`.
fn init_counter_file(dir: &Path) -> PathBuf {
    let p = dir.join("counter");
    fs::write(&p, "0").expect("init counter file");
    p
}

#[test]
fn move_window_two_stage_cancel_at_stage1_exits_zero() {
    // Stage 1 cancellation: the shim is invoked once (stage 1), exits
    // 1 (user dismiss). No stage 2 happens, no second IPC call fires.
    // CLI must classify as cancellation and exit 0.
    let shim = ShimDir::new("mw-cancel-stage1");
    shim.install_fuzzel(TWO_STAGE_SHIM_BODY);
    let counter = init_counter_file(shim.as_path());
    let sock = spawn_two_shot_listener_for_move_window(
        "mw-cancel-stage1",
        MOVE_WINDOW_ACTIVITIES_REPLY,
        MOVE_WINDOW_WORKSPACES_REPLY,
    );

    Command::cargo_bin(BIN)
        .unwrap()
        .arg("move-window")
        .env_clear()
        .env("PATH", shim.as_path())
        .env("JIJI_SOCKET", &sock)
        .env("SHIM_COUNTER_FILE", &counter)
        .env("SHIM_STAGE1_RESPONSE", "cancel")
        .env("SHIM_STAGE2_RESPONSE", "cancel")
        .assert()
        .code(0);
}

#[test]
fn move_window_two_stage_select_then_dead_socket_exits_69() {
    // Stage 1 selects "Work", stage 2 selects the workspace label. The
    // two-shot listener already served Activities + Workspaces; the
    // third IPC call (MoveWindowToWorkspace) hits a dead socket and
    // exit 69 surfaces. Pins that the dispatch IPC call actually fires
    // (regression to exit 0 would mean stage 2 silently no-op'd).
    let shim = ShimDir::new("mw-select");
    shim.install_fuzzel(TWO_STAGE_SHIM_BODY);
    let counter = init_counter_file(shim.as_path());
    let sock = spawn_two_shot_listener_for_move_window(
        "mw-select",
        MOVE_WINDOW_ACTIVITIES_REPLY,
        MOVE_WINDOW_WORKSPACES_REPLY,
    );

    Command::cargo_bin(BIN)
        .unwrap()
        .arg("move-window")
        .env_clear()
        .env("PATH", shim.as_path())
        .env("JIJI_SOCKET", &sock)
        .env("SHIM_COUNTER_FILE", &counter)
        // Stage 1: pick the active activity "Work".
        .env("SHIM_STAGE1_RESPONSE", "select:Work")
        // Stage 2: pick the trailing-empty workspace's label.
        // (label is "idx 1" since the workspace has no name.)
        .env("SHIM_STAGE2_RESPONSE", "select:idx 1")
        .assert()
        .code(69);
}

#[test]
fn move_window_stage2_non_active_no_workspaces_exits_zero_with_eprintln() {
    // Binary-boundary pin of the stage-2 zero-case: stage 1 selects
    // "Personal" (non-active, no workspaces on focused output in the
    // fixture), stage 2 short-circuits to eprintln+exit-0 without
    // spawning the picker.
    //
    // The shim counter must equal "1" after the run (stage 1 fired;
    // stage 2 was never spawned). stderr must name the activity.
    let shim = ShimDir::new("mw-stage2-zero");
    shim.install_fuzzel(TWO_STAGE_SHIM_BODY);
    let counter = init_counter_file(shim.as_path());
    // MOVE_WINDOW_WORKSPACES_REPLY has workspaces only in activity 1;
    // "Personal" (id 2) has none on DP-1 — triggers the zero-case.
    let sock = spawn_two_shot_listener_for_move_window(
        "mw-stage2-zero",
        MOVE_WINDOW_ACTIVITIES_REPLY,
        MOVE_WINDOW_WORKSPACES_REPLY,
    );

    Command::cargo_bin(BIN)
        .unwrap()
        .arg("move-window")
        .env_clear()
        .env("PATH", shim.as_path())
        .env("JIJI_SOCKET", &sock)
        .env("SHIM_COUNTER_FILE", &counter)
        .env("SHIM_STAGE1_RESPONSE", "select:Personal")
        .env("SHIM_STAGE2_RESPONSE", "cancel")
        .assert()
        .code(0)
        .stderr(contains("Personal").and(contains("nothing to move window to")));

    let counter_val = std::fs::read_to_string(&counter).expect("counter file readable");
    assert_eq!(
        counter_val.trim(),
        "1",
        "shim must be invoked exactly once (stage 1 only); got counter={counter_val:?}",
    );
}

#[test]
fn move_window_named_arg_skips_picker() {
    // `move-window Personal` is fully non-interactive — even with a
    // shim installed, it must NOT be invoked. The shim creates a
    // sentinel file on entry; the test asserts the file is absent
    // after the run. Exit 0 is the zero-case (no workspaces in
    // 'Personal' on focused output — the fixture has none).
    let shim = ShimDir::new("mw-named");
    let sentinel = shim.as_path().join("shim-invoked.sentinel");
    shim.install_fuzzel(
        ": > \"$SHIM_INVOKED\"\nprintf 'jiji-activities BUG: picker spawned for named arg\\n' >&2\nexit 99\n",
    );
    let sock = spawn_two_shot_listener_for_move_window(
        "mw-named",
        MOVE_WINDOW_ACTIVITIES_REPLY,
        MOVE_WINDOW_WORKSPACES_REPLY,
    );

    Command::cargo_bin(BIN)
        .unwrap()
        .args(["move-window", "Personal"])
        .env_clear()
        .env("PATH", shim.as_path())
        .env("JIJI_SOCKET", &sock)
        .env("SHIM_INVOKED", &sentinel)
        .assert()
        // 'Personal' is non-active and has no workspaces on DP-1 in
        // the fixture → zero-case eprintln + exit 0. Assert the
        // activity name is interpolated so a regression that drops the
        // name from the message surfaces immediately.
        .code(0)
        .stderr(contains("activity 'Personal' has no workspaces"));

    assert!(
        !sentinel.exists(),
        "fuzzel shim must NOT be invoked for `move-window <name>` (named-arg form is fully non-interactive)",
    );
}

#[test]
fn move_window_stage1_prompt_arg_is_activity_prompt() {
    // Pins that stage 1 passes `--prompt "Move window to activity:"`.
    let shim = ShimDir::new("mw-args-stage1");
    let capture = shim.as_path().join("stage1.args");
    shim.install_fuzzel(TWO_STAGE_SHIM_BODY);
    let counter = init_counter_file(shim.as_path());
    let sock = spawn_two_shot_listener_for_move_window(
        "mw-args-stage1",
        MOVE_WINDOW_ACTIVITIES_REPLY,
        MOVE_WINDOW_WORKSPACES_REPLY,
    );

    Command::cargo_bin(BIN)
        .unwrap()
        .arg("move-window")
        .env_clear()
        .env("PATH", shim.as_path())
        .env("JIJI_SOCKET", &sock)
        .env("SHIM_COUNTER_FILE", &counter)
        // Stage 1: write args, then cancel.
        .env(
            "SHIM_STAGE1_RESPONSE",
            format!("args:{}", capture.display()),
        )
        .env("SHIM_STAGE2_RESPONSE", "cancel")
        .assert()
        .code(0);

    let args = fs::read_to_string(&capture).expect("shim must capture stage1 args");
    assert!(
        args.contains("--prompt"),
        "--prompt flag must be present in stage-1 fuzzel args: {args:?}",
    );
    assert!(
        args.contains("Move window to activity:"),
        "stage-1 prompt must be 'Move window to activity:': {args:?}",
    );
}

#[test]
fn move_window_stage2_prompt_arg_is_workspace_prompt() {
    // Pins that stage 2 passes `--prompt "Move window to workspace:"`.
    // Stage 1 selects "Work" (the active activity) so stage 2 fires.
    let shim = ShimDir::new("mw-args-stage2");
    let capture = shim.as_path().join("stage2.args");
    shim.install_fuzzel(TWO_STAGE_SHIM_BODY);
    let counter = init_counter_file(shim.as_path());
    let sock = spawn_two_shot_listener_for_move_window(
        "mw-args-stage2",
        MOVE_WINDOW_ACTIVITIES_REPLY,
        MOVE_WINDOW_WORKSPACES_REPLY,
    );

    Command::cargo_bin(BIN)
        .unwrap()
        .arg("move-window")
        .env_clear()
        .env("PATH", shim.as_path())
        .env("JIJI_SOCKET", &sock)
        .env("SHIM_COUNTER_FILE", &counter)
        // Stage 1: select Work so stage 2 fires.
        .env("SHIM_STAGE1_RESPONSE", "select:Work")
        // Stage 2: write args, then cancel.
        .env(
            "SHIM_STAGE2_RESPONSE",
            format!("args:{}", capture.display()),
        )
        .assert()
        .code(0);

    let args = fs::read_to_string(&capture).expect("shim must capture stage2 args");
    assert!(
        args.contains("--prompt"),
        "--prompt flag must be present in stage-2 fuzzel args: {args:?}",
    );
    assert!(
        args.contains("Move window to workspace:"),
        "stage-2 prompt must be 'Move window to workspace:': {args:?}",
    );
}

#[test]
fn move_window_here_picker_cancel_exits_zero() {
    // `move-window-here` cancellation. The verb opens a single
    // workspace picker (no preceding activity picker). The counter-shim
    // is invoked once; cancellation (counter==0 → SHIM_STAGE1_RESPONSE)
    // routes to exit 0 silently.
    let shim = ShimDir::new("mwh-cancel");
    shim.install_fuzzel(TWO_STAGE_SHIM_BODY);
    let counter = init_counter_file(shim.as_path());
    let sock = spawn_two_shot_listener_for_move_window(
        "mwh-cancel",
        MOVE_WINDOW_ACTIVITIES_REPLY,
        MOVE_WINDOW_WORKSPACES_REPLY,
    );

    Command::cargo_bin(BIN)
        .unwrap()
        .arg("move-window-here")
        .env_clear()
        .env("PATH", shim.as_path())
        .env("JIJI_SOCKET", &sock)
        .env("SHIM_COUNTER_FILE", &counter)
        .env("SHIM_STAGE1_RESPONSE", "cancel")
        .env("SHIM_STAGE2_RESPONSE", "cancel")
        .assert()
        .code(0);
}

// ---- rename picker tests ---------------------------------------------------

#[test]
fn rename_picker_cancel_exits_zero() {
    // Full pipe-and-read flow for `rename`: socket listener answers
    // Activities, the single-select fuzzel shim simulates a user dismissal
    // (exit 1, empty stdout). The CLI must classify that as cancellation and
    // exit 0 silently.
    let shim = ShimDir::new("rename-cancel");
    // Drain stdin to EOF before exiting so the binary's stdin write
    // does not race with the shim's exit and produce EPIPE.
    shim.install_fuzzel("while IFS= read -r _line; do :; done\nexit 1\n");
    let sock = spawn_one_shot_activities_listener("rename-cancel");

    Command::cargo_bin(BIN)
        .unwrap()
        .args(["rename", "NewName"])
        .env_clear()
        .env("PATH", shim.as_path())
        .env("JIJI_SOCKET", &sock)
        .assert()
        .code(0);
}

#[test]
fn rename_picker_select_then_second_ipc_fails_exits_69() {
    // Full pipe-and-read flow: socket listener answers Activities, the picker
    // spawns the shim, which prints "Work" and exits 0 — picker returns
    // Selected("Work"). `run_picker` then dispatches a SECOND IPC call
    // (RenameActivity); the one-shot listener already closed, so that second
    // call hits a dead socket and exit 69 surfaces. Pins that the picker path
    // reaches rename::run (regression to exit 0 would mean no second IPC
    // call fired).
    let shim = ShimDir::new("rename-select");
    shim.install_fuzzel("printf 'Work\\n'\nexit 0\n");
    let sock = spawn_one_shot_activities_listener("rename-select");

    Command::cargo_bin(BIN)
        .unwrap()
        .args(["rename", "NewName"])
        .env_clear()
        .env("PATH", shim.as_path())
        .env("JIJI_SOCKET", &sock)
        .assert()
        .code(69);
}

#[test]
fn rename_picker_prompt_arg_is_rename_prompt() {
    // Pins that the rename picker passes `--prompt "Rename activity:"` to
    // fuzzel. A regression that re-used another verb's prompt string would
    // not be caught by the cancel/select tests. The shim writes `$@` to
    // `$SHIM_ARGS_CAPTURE`, then cancels.
    let shim = ShimDir::new("rename-args");
    let capture = shim.as_path().join("args.cap");
    // Drain stdin before capturing args so the binary's stdin write does
    // not race with the shim's exit and produce EPIPE.
    shim.install_fuzzel("while IFS= read -r _line; do :; done\nprintf '%s\\n' \"$@\" > \"$SHIM_ARGS_CAPTURE\"\nexit 1\n");
    let sock = spawn_one_shot_activities_listener("rename-args");

    Command::cargo_bin(BIN)
        .unwrap()
        .args(["rename", "NewName"])
        .env_clear()
        .env("PATH", shim.as_path())
        .env("JIJI_SOCKET", &sock)
        .env("SHIM_ARGS_CAPTURE", &capture)
        .assert()
        .code(0);

    let args = fs::read_to_string(&capture).expect("shim must have written args capture file");
    assert!(
        args.contains("--prompt"),
        "--prompt flag must be present in rename fuzzel args: {args:?}",
    );
    assert!(
        args.contains("Rename activity:"),
        "prompt value must be 'Rename activity:' in rename fuzzel args: {args:?}",
    );
}

/// Sibling of [`spawn_one_shot_activities_listener`] that replies with a
/// single-activity `Activities` payload (one activity, marked active).
/// Used by the single-activity short-circuit integration test.
fn spawn_one_shot_activities_listener_single(tag: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "jiji-activities-shim-sock-single-{}-{}-{}.sock",
        std::process::id(),
        n,
        tag,
    ));
    let _ = fs::remove_file(&path);
    let listener = UnixListener::bind(&path).expect("bind one-shot socket");
    let path_clone = path.clone();
    thread::spawn(move || {
        if let Ok((mut sock, _)) = listener.accept() {
            let read_clone = sock.try_clone().expect("clone socket");
            let mut reader = BufReader::new(read_clone);
            let mut req = String::new();
            let _ = reader.read_line(&mut req);
            // One activity: "Work" (active).
            let reply = "{\"Ok\":{\"Activities\":[\
                 {\"id\":1,\"name\":\"Work\",\"is_config_declared\":true,\"is_active\":true,\"is_urgent\":false,\"last_active_seq\":1}\
                 ]}}\n";
            let _ = sock.write_all(reply.as_bytes());
        }
        let _ = fs::remove_file(&path_clone);
    });
    for _ in 0..50 {
        if path.exists() {
            break;
        }
        thread::sleep(std::time::Duration::from_millis(10));
    }
    path
}

/// Sibling of [`spawn_one_shot_activities_listener`] that replies with an
/// empty `Activities` payload. Used by the empty-list short-circuit
/// integration test.
fn spawn_one_shot_activities_listener_empty(tag: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "jiji-activities-shim-sock-empty-{}-{}-{}.sock",
        std::process::id(),
        n,
        tag,
    ));
    let _ = fs::remove_file(&path);
    let listener = UnixListener::bind(&path).expect("bind one-shot socket");
    let path_clone = path.clone();
    thread::spawn(move || {
        if let Ok((mut sock, _)) = listener.accept() {
            let read_clone = sock.try_clone().expect("clone socket");
            let mut reader = BufReader::new(read_clone);
            let mut req = String::new();
            let _ = reader.read_line(&mut req);
            let reply = "{\"Ok\":{\"Activities\":[]}}\n";
            let _ = sock.write_all(reply.as_bytes());
        }
        let _ = fs::remove_file(&path_clone);
    });
    for _ in 0..50 {
        if path.exists() {
            break;
        }
        thread::sleep(std::time::Duration::from_millis(10));
    }
    path
}
