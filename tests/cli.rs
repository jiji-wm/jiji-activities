//! Smoke integration tests pinning the CLI surface.
//!
//! These tests exercise the binary through `assert_cmd` so any
//! regression in the clap-derive surface (dropped subcommand,
//! changed exit code, broken `--version`) surfaces immediately.

use assert_cmd::Command;
use predicates::str::contains;

const BIN: &str = "niri-activities";

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
        "move-window",
        "move-workspace",
        "assign-workspace",
        "create",
        "remove",
        "save",
        "list",
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
    // IPC factory. With $NIRI_SOCKET unset the factory returns
    // SocketUnavailable (exit 69) — proving the named path is wired end-
    // to-end rather than returning NotImplemented (exit 70).
    Command::cargo_bin(BIN)
        .unwrap()
        .args(["switch", "Work"])
        .env_remove("NIRI_SOCKET")
        .assert()
        .code(69);
}

#[test]
fn switch_no_arg_no_socket_exits_69() {
    // `switch` with no arg now opens the fuzzel picker. With
    // `$NIRI_SOCKET` unset, two distinct paths can produce exit 69:
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
        .env_remove("NIRI_SOCKET")
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
    // stub it used to fall back to. With `$NIRI_SOCKET` unset, two
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
        .env_remove("NIRI_SOCKET")
        .assert()
        .code(69);
}

#[test]
fn toggle_alias_routes_to_switch_previous() {
    Command::cargo_bin(BIN)
        .unwrap()
        .arg("toggle")
        .assert()
        .code(70)
        .stderr(contains("subcommand not yet implemented: switch-previous"));
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
    // whether `$NIRI_SOCKET` is set or reachable. Pin that end-to-end.
    Command::cargo_bin(BIN)
        .unwrap()
        .args(["list", "--format", "bogus"])
        .assert()
        .code(64)
        .stderr(contains("unknown field: bogus"));
}
