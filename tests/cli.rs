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
fn switch_stub_exits_70() {
    Command::cargo_bin(BIN)
        .unwrap()
        .args(["switch", "foo"])
        .assert()
        .code(70)
        .stderr(contains("subcommand not yet implemented: switch"));
}

#[test]
fn no_args_exits_64() {
    Command::cargo_bin(BIN).unwrap().assert().code(64);
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
