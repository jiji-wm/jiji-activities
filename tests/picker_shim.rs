//! Integration tests for the `fuzzel`-backed picker via a shim binary.
//!
//! These tests prove the spawn-and-pipe flow end-to-end without
//! depending on a real `fuzzel` install on the test host. The strategy:
//!
//! 1. Create a per-test tempdir.
//! 2. Write a `bash` script named `fuzzel` inside it that does whatever
//!    behaviour the test wants (cancel, select, etc.).
//! 3. Spawn `niri-activities` with `$PATH` set to *only* that tempdir
//!    (via `env_clear` + explicit `env("PATH", ...)`) so the shim is the
//!    only `fuzzel` the binary can resolve.
//! 4. For tests that need the IPC `Request::Activities` round-trip to
//!    succeed (so the picker is actually reached), bind a one-shot
//!    Unix listener and point `$NIRI_SOCKET` at it. The listener
//!    replies with a fixed `Response::Activities` payload then exits.
//!
//! `env_clear` is load-bearing — leaving the parent's `$PATH` in place
//! would let the real `fuzzel` on the developer's machine shadow the
//! shim and turn these into integration tests against a live `fuzzel`.

use std::fs;
use std::io::{BufRead, BufReader, Write as _};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;

use assert_cmd::Command;
use predicates::str::contains;

const BIN: &str = "niri-activities";

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
            "niri-activities-shim-{}-{}-{}",
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
/// The path is returned so the caller can point `$NIRI_SOCKET` at it.
fn spawn_one_shot_activities_listener(tag: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "niri-activities-shim-sock-{}-{}-{}.sock",
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
                 {\"id\":1,\"name\":\"Work\",\"is_config_declared\":true,\"is_active\":true,\"is_urgent\":false},\
                 {\"id\":2,\"name\":\"Personal\",\"is_config_declared\":true,\"is_active\":false,\"is_urgent\":false}\
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
    // the picker spawns the shim, which simulates a user dismissal
    // (exit 1 with empty stdout). The CLI must classify that as
    // cancellation and exit 0 silently.
    let shim = ShimDir::new("cancel");
    shim.install_fuzzel("exit 1\n");
    let sock = spawn_one_shot_activities_listener("cancel");

    Command::cargo_bin(BIN)
        .unwrap()
        .arg("switch")
        .env_clear()
        .env("PATH", shim.as_path())
        .env("NIRI_SOCKET", &sock)
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
    // surfaces. `$NIRI_SOCKET` IS set, but the post-pick socket is
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
        .env("NIRI_SOCKET", &sock)
        .assert()
        .code(69);
}

#[test]
fn fuzzel_missing_from_path_exits_69() {
    // `$PATH` points at an empty tempdir — no `fuzzel` binary there.
    // `ensure_available()` (called from `cmd_switch` BEFORE any IPC
    // round-trip) returns SocketUnavailable with the canonical
    // missing-fuzzel message. Exit code 69 with stderr naming `fuzzel`.
    //
    // `$NIRI_SOCKET` is unset deliberately: even with a missing socket,
    // the missing-fuzzel error must surface first because the
    // availability check runs before any IPC.
    let shim = ShimDir::new("missing");
    // Deliberately NOT installing the fuzzel script — the dir is empty.

    Command::cargo_bin(BIN)
        .unwrap()
        .arg("switch")
        .env_clear()
        .env("PATH", shim.as_path())
        .env_remove("NIRI_SOCKET")
        .assert()
        .code(69)
        .stderr(contains("fuzzel"));
}
