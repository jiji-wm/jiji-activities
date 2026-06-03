//! Smoke integration tests pinning the CLI surface.
//!
//! These tests exercise the binary through `assert_cmd` so any
//! regression in the clap-derive surface (dropped subcommand,
//! changed exit code, broken `--version`) surfaces immediately.

use std::io::{BufRead, BufReader, Write as _};
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;

use assert_cmd::Command;
use predicates::str::contains;

const BIN: &str = "jiji-activities";

/// Multi-reply Unix-socket listener that accepts one connection per reply
/// and sends a fixed reply to each, in order.  `SocketClient` opens a new
/// Unix-socket connection for every `send` call, so each IPC request from
/// the binary arrives as an independent `accept()`.  Used for subcommands
/// that issue more than one IPC request per invocation.
fn spawn_scripted_listener(tag: &str, replies: Vec<&'static str>) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let sock_path = std::env::temp_dir().join(format!(
        "jiji-activities-cli-test-scripted-{}-{}-{}.sock",
        std::process::id(),
        n,
        tag,
    ));
    let _ = std::fs::remove_file(&sock_path);
    let listener = UnixListener::bind(&sock_path).expect("bind scripted socket");
    let sock_path_clone = sock_path.clone();
    thread::spawn(move || {
        for reply in replies {
            // Accept one connection per queued reply.
            if let Ok((mut sock, _)) = listener.accept() {
                let read_clone = sock.try_clone().expect("clone socket");
                let mut reader = BufReader::new(read_clone);
                let mut _req_line = String::new();
                let _ = reader.read_line(&mut _req_line);
                let _ = sock.write_all(reply.as_bytes());
                let _ = sock.write_all(b"\n");
            }
        }
        let _ = std::fs::remove_file(&sock_path_clone);
    });
    // Spin until the socket file appears (up to ~500 ms).
    for _ in 0..50 {
        if sock_path.exists() {
            break;
        }
        thread::sleep(std::time::Duration::from_millis(10));
    }
    sock_path
}

/// One-shot Unix-socket listener that accepts one connection, reads one
/// request line, writes a `Reply::Ok(Response::Handled)` reply, and
/// captures the raw request JSON to `capture_path`. Used to pin the
/// IPC request shape emitted by a subcommand without a full compositor.
fn spawn_one_shot_handled_listener(tag: &str, capture_path: PathBuf) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let sock_path = std::env::temp_dir().join(format!(
        "jiji-activities-cli-test-{}-{}-{}.sock",
        std::process::id(),
        n,
        tag,
    ));
    let _ = std::fs::remove_file(&sock_path);
    let listener = UnixListener::bind(&sock_path).expect("bind one-shot socket");
    let sock_path_clone = sock_path.clone();
    thread::spawn(move || {
        if let Ok((mut sock, _)) = listener.accept() {
            let read_clone = sock.try_clone().expect("clone socket");
            let mut reader = BufReader::new(read_clone);
            let mut req_line = String::new();
            let _ = reader.read_line(&mut req_line);
            // Capture the raw request JSON so the test can inspect it.
            let _ = std::fs::write(&capture_path, req_line.as_bytes());
            // Reply Handled unconditionally.
            let _ = sock.write_all(b"{\"Ok\":\"Handled\"}\n");
        }
        let _ = std::fs::remove_file(&sock_path_clone);
    });
    // Spin until the socket file appears (up to ~500 ms).
    for _ in 0..50 {
        if sock_path.exists() {
            break;
        }
        thread::sleep(std::time::Duration::from_millis(10));
    }
    sock_path
}

#[test]
fn version_prints_pkg_version() {
    Command::cargo_bin(BIN)
        .unwrap()
        .arg("--version")
        .assert()
        .success()
        .stdout(contains(env!("CARGO_PKG_VERSION")));
}

#[test]
fn help_lists_all_subcommands() {
    let assert = Command::cargo_bin(BIN)
        .unwrap()
        .arg("--help")
        .assert()
        .success();
    let out = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    // Match `  <name> ` (two leading spaces + trailing space) so that
    // e.g. "switch" does not accidentally match "switch-previous".
    for sub in [
        "switch-previous",
        "move-window-here",
        "move-workspace",
        "assign-workspace",
        "create",
        "remove",
        "rename",
        "save",
        "list",
        "completions",
        // "move-window" checked after "move-window-here" so a plain
        // `contains("move-window")` does not accidentally match the longer
        // name; the two-space-and-trailing-space pattern also disambiguates.
        "move-window",
        // "switch" checked last and with delimiter so it doesn't hit "switch-previous"
        "switch",
    ] {
        assert!(
            out.contains(&format!("  {sub} ")),
            "--help output missing subcommand `{sub}`:\n{out}",
        );
    }
}

#[test]
fn unknown_subcommand_exits_64() {
    Command::cargo_bin(BIN)
        .unwrap()
        .arg("bogus-subcommand")
        .assert()
        .code(64);
}

#[test]
fn switch_named_no_socket_exits_69() {
    // Pins the binary-boundary wiring: `switch <name>` dispatches
    // through switch::run (not the NotImplemented stub), which hits the
    // IPC factory. With $JIJI_SOCKET unset the factory returns
    // SocketUnavailable (exit 69) — proving the named path is wired end-
    // to-end rather than returning NotImplemented (exit 70).
    Command::cargo_bin(BIN)
        .unwrap()
        .args(["switch", "Work"])
        .env_remove("JIJI_SOCKET")
        .assert()
        .code(69);
}

#[test]
fn switch_no_arg_no_socket_exits_69() {
    // `switch` with no arg now opens the fuzzel picker. With
    // `$JIJI_SOCKET` unset, two distinct paths can produce exit 69:
    //
    // 1. If `fuzzel` IS installed on this host (the common case on dev
    //    machines), `which fuzzel` succeeds but `Request::Activities`
    //    fails on the missing socket → `SocketUnavailable` with the IPC
    //    stderr message.
    // 2. If `fuzzel` is NOT installed, `which fuzzel` short-circuits to
    //    `PickerUnavailable` with the missing-fuzzel stderr message.
    //
    // Both routes share exit code 69. We deliberately do not assert the
    // stderr text — the assertion target is the wire-up (no-arg branch
    // is routed through the picker/IPC path, not the `NotImplemented`
    // stub it used to fall through to). A regression to exit 70 would
    // mean the no-arg branch silently fell back to the stub.
    Command::cargo_bin(BIN)
        .unwrap()
        .args(["switch"])
        .env_remove("JIJI_SOCKET")
        .assert()
        .code(69);
}

#[test]
fn no_args_exits_64() {
    Command::cargo_bin(BIN).unwrap().assert().code(64);
}

#[test]
fn assign_workspace_no_socket_exits_69() {
    // Pins the wiring: `assign-workspace` dispatches through the picker
    // availability check + IPC factory rather than the `NotImplemented`
    // stub it used to fall back to. With `$JIJI_SOCKET` unset, two
    // distinct routes can produce exit 69:
    //
    // 1. If `rofi` IS installed on this host, the availability check
    //    passes and the first IPC call fails on the missing socket →
    //    `SocketUnavailable`.
    // 2. If `rofi` is NOT installed, the availability check
    //    short-circuits to `PickerUnavailable`.
    //
    // Both share exit code 69. A regression to exit 70 would mean
    // assign-workspace silently fell back to the NotImplemented stub.
    Command::cargo_bin(BIN)
        .unwrap()
        .arg("assign-workspace")
        .env_remove("JIJI_SOCKET")
        .assert()
        .code(69);
}

#[test]
fn switch_previous_no_socket_exits_69() {
    // Pins the binary-boundary wiring: `switch-previous` dispatches
    // through switch_previous::run (not the NotImplemented stub), which
    // hits the IPC factory. With $JIJI_SOCKET unset the factory returns
    // SocketUnavailable (exit 69) — proving the wired path replaced the
    // stub. A regression to exit 70 would mean it fell back to
    // NotImplemented.
    Command::cargo_bin(BIN)
        .unwrap()
        .args(["switch-previous"])
        .env_remove("JIJI_SOCKET")
        .assert()
        .code(69);
}

#[test]
fn toggle_alias_no_socket_exits_69() {
    // The `toggle` alias must reach the same wired switch-previous
    // path. Exit 69 (not 70) proves the alias is not silently falling
    // through to the NotImplemented stub it used to.
    Command::cargo_bin(BIN)
        .unwrap()
        .arg("toggle")
        .env_remove("JIJI_SOCKET")
        .assert()
        .code(69);
}

#[test]
fn move_window_no_socket_exits_69() {
    // `move-window` with no arg opens the fuzzel picker. With
    // `$JIJI_SOCKET` unset, two distinct paths can produce exit 69:
    //
    // 1. If `fuzzel` IS installed on this host, `which fuzzel` succeeds
    //    but `Request::Activities` fails on the missing socket →
    //    `SocketUnavailable`.
    // 2. If `fuzzel` is NOT installed, `which fuzzel` short-circuits to
    //    `PickerUnavailable`.
    //
    // Both routes share exit code 69. The assertion target is the
    // wire-up (no-arg branch routed through the picker/IPC path, not
    // the `NotImplemented` stub it used to fall through to). A
    // regression to exit 70 would mean the no-arg branch silently fell
    // back to the stub.
    Command::cargo_bin(BIN)
        .unwrap()
        .args(["move-window"])
        .env_remove("JIJI_SOCKET")
        .assert()
        .code(69);
}

#[test]
fn move_window_here_no_socket_exits_69() {
    // `move-window-here` is always picker-driven (no named-arg form).
    // Same exit-code matrix as `move-window` no-arg. A regression to
    // exit 70 would mean the verb wasn't wired up.
    Command::cargo_bin(BIN)
        .unwrap()
        .args(["move-window-here"])
        .env_remove("JIJI_SOCKET")
        .assert()
        .code(69);
}

#[test]
fn move_workspace_named_no_socket_exits_69() {
    // Pins the binary-boundary wiring: `move-workspace <name>`
    // dispatches through move_workspace::run (not NotImplemented),
    // hitting the IPC factory. With $JIJI_SOCKET unset the factory
    // returns SocketUnavailable (exit 69).
    Command::cargo_bin(BIN)
        .unwrap()
        .args(["move-workspace", "Work"])
        .env_remove("JIJI_SOCKET")
        .assert()
        .code(69);
}

#[test]
fn move_workspace_no_arg_no_socket_exits_69() {
    // `move-workspace` with no arg now opens the fuzzel picker. With
    // `$JIJI_SOCKET` unset, two distinct paths can produce exit 69:
    //
    // 1. If `fuzzel` IS installed on this host, `which fuzzel` succeeds
    //    but `Request::Activities` fails on the missing socket →
    //    `SocketUnavailable`.
    // 2. If `fuzzel` is NOT installed, `which fuzzel` short-circuits to
    //    `PickerUnavailable`.
    //
    // Both routes share exit code 69. We deliberately do not assert the
    // stderr text — the assertion target is the wire-up (no-arg branch
    // routed through the picker/IPC path, not NotImplemented). A
    // regression to exit 70 would mean the no-arg branch silently fell
    // back to the stub.
    Command::cargo_bin(BIN)
        .unwrap()
        .args(["move-workspace"])
        .env_remove("JIJI_SOCKET")
        .assert()
        .code(69);
}

#[test]
fn create_no_socket_exits_69() {
    // Pins the binary-boundary wiring: `create <name>` dispatches
    // through create::run (not the NotImplemented stub), which hits the
    // IPC factory. With $JIJI_SOCKET unset the factory returns
    // SocketUnavailable (exit 69) — proving the wired path replaced the
    // stub. A regression to exit 70 would mean it fell back to
    // NotImplemented.
    Command::cargo_bin(BIN)
        .unwrap()
        .args(["create", "Foo"])
        .env_remove("JIJI_SOCKET")
        .assert()
        .code(69);
}

#[test]
fn remove_no_socket_exits_69() {
    // Pins the binary-boundary wiring: `remove <name>` dispatches
    // through remove::run (not the NotImplemented stub), which hits the
    // IPC factory. With $JIJI_SOCKET unset the factory returns
    // SocketUnavailable (exit 69) — proving the wired path replaced the
    // stub. A regression to exit 70 would mean it fell back to
    // NotImplemented.
    Command::cargo_bin(BIN)
        .unwrap()
        .args(["remove", "Foo"])
        .env_remove("JIJI_SOCKET")
        .assert()
        .code(69);
}

#[test]
fn save_no_socket_exits_69() {
    // Pins the binary-boundary wiring: `save <name>` dispatches through
    // save::run (not the NotImplemented stub), which performs a
    // filesystem write FIRST, then crosses the IPC factory for the
    // LoadConfigFile reload. With $NIRI_CONFIG pointing at a writable
    // tempdir and $JIJI_SOCKET unset, the fs-edit step succeeds and
    // the reload IPC call fails on the dead socket → exit 69. A
    // regression to exit 70 would mean save fell back to
    // NotImplemented; a regression to exit 73 would mean the fs-edit
    // erroneously preceded the empty-socket short-circuit (or the IPC
    // step never ran).
    let tmp = tempfile::tempdir().expect("tempdir");
    let cfg = tmp.path().join("config.kdl");
    std::fs::write(&cfg, "// seed\n").expect("seed config");
    Command::cargo_bin(BIN)
        .unwrap()
        .args(["save", "Foo"])
        .env("NIRI_CONFIG", &cfg)
        .env_remove("JIJI_SOCKET")
        .assert()
        .code(69)
        // C1: the recovery breadcrumb must always appear on stderr when
        // the fs-write succeeded but the reload IPC failed — including
        // the Transport (dead-socket) path, which is the primary documented
        // failure mode.
        .stderr(contains("jiji-activities: note: activity was written to"))
        .stderr(contains("load-config-file"));
    // The fs-edit phase must have completed: the new activity is on
    // disk even though the reload failed.
    let after = std::fs::read_to_string(&cfg).expect("config readable after");
    assert!(
        after.contains("activity") && after.contains("Foo"),
        "save must write the activity to config before the reload IPC; got: {after:?}",
    );
}

#[test]
fn list_json_and_format_conflict_exits_64() {
    Command::cargo_bin(BIN)
        .unwrap()
        .args(["list", "--json", "--format", "custom"])
        .assert()
        .code(64);
}

#[test]
fn list_unknown_format_field_exits_64() {
    // The format-spec parser runs *before* any IPC connect attempt, so
    // a bogus field name short-circuits to exit 64 regardless of
    // whether `$JIJI_SOCKET` is set or reachable. Pin that end-to-end.
    Command::cargo_bin(BIN)
        .unwrap()
        .args(["list", "--format", "bogus"])
        .assert()
        .code(64)
        .stderr(contains("unknown field: bogus"));
}

#[test]
fn toggle_alias_routes_to_switch_activity_previous() {
    // Pins that `toggle` dispatches `SwitchActivityPrevious`, not
    // `Switch { name: None }` (which would issue `Activities` first).
    // A clap regression that mis-routed `toggle` to `Cmd::Switch { .. }`
    // would still exit 69 on a dead socket, making exit-code tests
    // unable to detect it. This test inspects the actual request JSON.
    let capture = std::env::temp_dir().join(format!(
        "jiji-activities-cli-toggle-req-{}.json",
        std::process::id(),
    ));
    let sock = spawn_one_shot_handled_listener("toggle-shape", capture.clone());

    Command::cargo_bin(BIN)
        .unwrap()
        .arg("toggle")
        .env("JIJI_SOCKET", &sock)
        .assert()
        .success();

    let req_json = std::fs::read_to_string(&capture)
        .expect("one-shot listener must have written the request capture file");
    let _ = std::fs::remove_file(&capture);

    assert!(
        req_json.contains("SwitchActivityPrevious"),
        "toggle must emit SwitchActivityPrevious, not Switch or Activities; got: {req_json:?}",
    );
    assert!(
        req_json.contains("\"depth\":1"),
        "toggle must emit depth:1 in SwitchActivityPrevious; got: {req_json:?}",
    );
    assert!(
        !req_json.contains("Activities"),
        "toggle must NOT emit Activities (picker path); got: {req_json:?}",
    );
}

#[test]
fn completions_fish_emits_clap_complete_base_and_dynamic_lines() {
    // Pins both halves of the fish completion output:
    //   1. clap_complete base (anchored by `complete -c jiji-activities`)
    //   2. dynamic activity-name augmentation (anchored by the comment
    //      header, the `__jiji_activities_no_positional_yet` helper
    //      definition, and the position-aware condition for `switch`).
    // A regression in either half — clap_complete dropped, or the fish
    // branch in completions::run lost the augmentation — fails here.
    let assert = Command::cargo_bin(BIN)
        .unwrap()
        .args(["completions", "fish"])
        .assert()
        .success();
    let out = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(
        out.contains("complete -c jiji-activities"),
        "fish completion output missing clap_complete base:\n{out}",
    );
    assert!(
        out.contains("# Dynamic activity-name completion"),
        "fish completion output missing dynamic-section header:\n{out}",
    );
    assert!(
        out.contains("function __jiji_activities_no_positional_yet"),
        "fish completion output missing position-guard helper:\n{out}",
    );
    assert!(
        out.contains(
            "__fish_jiji_activities_using_subcommand switch; \
             and __jiji_activities_no_positional_yet"
        ),
        "fish completion output missing position-aware condition for `switch`:\n{out}",
    );
    assert!(
        out.contains("(jiji-activities list --format=name 2>/dev/null)"),
        "fish completion output missing live-candidate source:\n{out}",
    );
}

#[test]
fn completions_bash_emits_non_empty_output() {
    // Smoke check that non-fish shells still produce a clap_complete
    // base. We do not assert content beyond non-emptiness — bash output
    // shape is owned by clap_complete and changing across minor bumps is
    // expected; this test guards only that the dispatch path is wired.
    let assert = Command::cargo_bin(BIN)
        .unwrap()
        .args(["completions", "bash"])
        .assert()
        .success();
    let out = assert.get_output().stdout.clone();
    assert!(!out.is_empty(), "bash completions stdout must be non-empty");
}

#[test]
fn completions_unknown_shell_exits_64() {
    // Unknown shell values are rejected by clap (ValueEnum). Pins the
    // exit-64 contract for clap parse errors.
    Command::cargo_bin(BIN)
        .unwrap()
        .args(["completions", "bogus-shell"])
        .assert()
        .code(64);
}

#[test]
fn move_window_named_prints_confirmation_to_stderr_on_success() {
    // Pins that `move-window <activity>` writes the post-move confirmation
    // line to stderr on success.  A regression that deletes the
    // `print_move_confirmation` call would produce exit 0 with no stderr
    // output, which this test would catch.
    //
    // IPC script (3 requests):
    //   1. Activities  → [Work(active, id=1), Personal(inactive, id=2)]
    //   2. Workspaces  → focused ws id=10 (activity 1, output DP-1,
    //                    active_window_id=42), trailing-empty ws id=20
    //                    (activity 2, output DP-1).
    //   3. Action::MoveWindowToWorkspace → Handled.
    //
    // The compositor JSON for Activities and Workspaces is inline — it
    // matches the niri-ipc wire format.
    let activities_reply = r#"{"Ok":{"Activities":[{"id":1,"name":"Work","is_active":true,"is_config_declared":true,"is_urgent":false,"last_active_seq":2},{"id":2,"name":"Personal","is_active":false,"is_config_declared":true,"is_urgent":false,"last_active_seq":1}]}}"#;
    let workspaces_reply = r#"{"Ok":{"Workspaces":[{"id":10,"idx":0,"name":null,"output":"DP-1","is_urgent":false,"is_active":false,"is_focused":true,"active_window_id":42,"activities":[1],"is_sticky":false,"is_in_active_activity":true},{"id":20,"idx":0,"name":null,"output":"DP-1","is_urgent":false,"is_active":false,"is_focused":false,"active_window_id":null,"activities":[2],"is_sticky":false,"is_in_active_activity":false}]}}"#;
    let handled_reply = r#"{"Ok":"Handled"}"#;

    let sock = spawn_scripted_listener(
        "move-window-confirm",
        vec![activities_reply, workspaces_reply, handled_reply],
    );

    Command::cargo_bin(BIN)
        .unwrap()
        .args(["move-window", "Personal"])
        .env("JIJI_SOCKET", &sock)
        .assert()
        .success()
        .stderr(contains("moved focused window"))
        .stderr(contains("'Personal'"));
}

#[test]
fn rename_named_target_no_socket_exits_69() {
    // Pins the binary-boundary wiring: `rename <new-name> --activity <old>`
    // dispatches through rename::run (not a NotImplemented stub), which hits
    // the IPC factory. With $JIJI_SOCKET unset the factory returns
    // SocketUnavailable (exit 69) — proving the named path is wired end-to-
    // end. A regression to exit 70 would mean rename fell back to a stub.
    Command::cargo_bin(BIN)
        .unwrap()
        .args(["rename", "newname", "--activity", "old"])
        .env_remove("JIJI_SOCKET")
        .assert()
        .code(69);
}

#[test]
fn move_and_assign_verbs_expose_follow_and_overview_flags() {
    // Pins the clap surface for the four mutating verbs that grew
    // `--follow` and `--overview` flags. Iterating over the verbs keeps
    // the assertion target uniform: a missing `#[arg(long)]` on any of
    // the eight flag fields would render that flag as a positional in
    // `--help` (or omit it entirely), and this test catches both shapes.
    //
    // The flags are surface-only in this commit — the runner functions
    // accept and discard them. Behavioral consumption lands in later
    // tasks; the help-text assertion is the contract-pin until then.
    for verb in [
        "move-window",
        "move-window-here",
        "move-workspace",
        "assign-workspace",
    ] {
        let assert = Command::cargo_bin(BIN)
            .unwrap()
            .args([verb, "--help"])
            .assert()
            .success();
        let out = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
        assert!(
            out.contains("--follow"),
            "`{verb} --help` missing `--follow` flag:\n{out}",
        );
        assert!(
            out.contains("--overview"),
            "`{verb} --help` missing `--overview` flag:\n{out}",
        );
    }
}
