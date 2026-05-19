//! Integration tests for the `rofi`-backed multi-select picker via a
//! shim binary.
//!
//! Mirror of `picker_shim.rs` but exercises the `assign-workspace`
//! subcommand and its three-IPC-call flow. The shim strategy is
//! identical: per-test tempdir, `sh` script named `rofi` (and
//! optionally `fuzzel` for the chain leg), `$PATH` restricted to the
//! tempdir under `env_clear` so the host's real `rofi` / `fuzzel` can't
//! shadow the shim.
//!
//! The IPC listener accepts **three** sequential connections — one per
//! `SocketClient::send` round-trip (Activities, Workspaces,
//! SetWorkspaceActivities). Collapsing all three into one connection
//! would be wrong: `SocketClient` opens a fresh connection per call.

use std::fs;
use std::io::{BufRead, BufReader, Write as _};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use assert_cmd::Command;
use predicates::prelude::PredicateBooleanExt;
use predicates::str::{self, contains};

const BIN: &str = "jiji-activities";

/// Per-test unique tempdir under `/tmp`. PID + counter keeps concurrent
/// `cargo test` jobs disjoint. Mirror of `ShimDir` in
/// `picker_shim.rs`; the spec calls for a clean duplicate rather than a
/// shared infra extraction.
struct RofiShimDir {
    path: PathBuf,
}

impl RofiShimDir {
    fn new(tag: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "jiji-activities-rofi-shim-{}-{}-{}",
            std::process::id(),
            n,
            tag,
        ));
        fs::create_dir_all(&path).expect("create rofi shim tempdir");
        RofiShimDir { path }
    }

    /// Writes an executable `sh` script named `bin` inside this
    /// tempdir. `body` is the script body *without* the shebang.
    fn install_script(&self, bin: &str, body: &str) {
        let script = self.path.join(bin);
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

impl Drop for RofiShimDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// One-shot Unix-socket listener that accepts **three** sequential
/// connections and replies in order:
///
/// 1. `Response::Activities` — two activities (`Work` focused-irrelevant
///    here, `Personal`).
/// 2. `Response::Workspaces` — one focused workspace (id 42, currently
///    belonging to activity id 1).
/// 3. `Response::Handled` — the `SetWorkspaceActivities` ack.
///
/// `SocketClient::send` opens a fresh connection per call, so we must
/// `accept()` three times — not read three lines off one connection.
/// Collapsing them would mean the shim listener races against the
/// CLI's second connect and fails non-deterministically.
fn spawn_one_shot_assign_listener(tag: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "jiji-activities-rofi-shim-sock-{}-{}-{}.sock",
        std::process::id(),
        n,
        tag,
    ));
    let _ = fs::remove_file(&path);
    let listener = UnixListener::bind(&path).expect("bind one-shot socket");
    let path_clone = path.clone();
    thread::spawn(move || {
        // Reply #1 — Activities.
        let activities_reply = "{\"Ok\":{\"Activities\":[\
             {\"id\":1,\"name\":\"Work\",\"is_config_declared\":true,\"is_active\":true,\"is_urgent\":false},\
             {\"id\":2,\"name\":\"Personal\",\"is_config_declared\":true,\"is_active\":false,\"is_urgent\":false}\
             ]}}\n";
        // Reply #2 — Workspaces with one focused workspace (id 42,
        // currently in activity 1).
        let workspaces_reply = "{\"Ok\":{\"Workspaces\":[\
             {\"id\":42,\"idx\":1,\"name\":null,\"output\":\"DP-1\",\"is_urgent\":false,\
              \"is_active\":true,\"is_focused\":true,\"active_window_id\":null,\
              \"activities\":[1],\"is_sticky\":false,\"is_in_active_activity\":true}\
             ]}}\n";
        // Reply #3 — Handled.
        let handled_reply = "{\"Ok\":\"Handled\"}\n";
        let replies = [activities_reply, workspaces_reply, handled_reply];
        for reply in replies.iter() {
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

/// Like [`spawn_one_shot_assign_listener`], but also captures the raw
/// JSON request lines from each of the three connections into the
/// returned `Arc<Mutex<Vec<String>>>`. Callers can inspect the third
/// entry to assert the wire-level `SetWorkspaceActivities` body.
fn spawn_one_shot_assign_listener_capturing(tag: &str) -> (PathBuf, Arc<Mutex<Vec<String>>>) {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "jiji-activities-rofi-shim-cap-{}-{}-{}.sock",
        std::process::id(),
        n,
        tag,
    ));
    let _ = fs::remove_file(&path);
    let listener = UnixListener::bind(&path).expect("bind capturing socket");
    let path_clone = path.clone();
    let captured: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let captured_clone = Arc::clone(&captured);
    thread::spawn(move || {
        let activities_reply = "{\"Ok\":{\"Activities\":[\
             {\"id\":1,\"name\":\"Work\",\"is_config_declared\":true,\"is_active\":true,\"is_urgent\":false},\
             {\"id\":2,\"name\":\"Personal\",\"is_config_declared\":true,\"is_active\":false,\"is_urgent\":false}\
             ]}}\n";
        let workspaces_reply = "{\"Ok\":{\"Workspaces\":[\
             {\"id\":42,\"idx\":1,\"name\":null,\"output\":\"DP-1\",\"is_urgent\":false,\
              \"is_active\":true,\"is_focused\":true,\"active_window_id\":null,\
              \"activities\":[1],\"is_sticky\":false,\"is_in_active_activity\":true}\
             ]}}\n";
        let handled_reply = "{\"Ok\":\"Handled\"}\n";
        let replies = [activities_reply, workspaces_reply, handled_reply];
        for reply in replies.iter() {
            match listener.accept() {
                Ok((mut sock, _)) => {
                    let read_clone = sock.try_clone().expect("clone socket");
                    let mut reader = BufReader::new(read_clone);
                    let mut req = String::new();
                    let _ = reader.read_line(&mut req);
                    captured_clone
                        .lock()
                        .unwrap()
                        .push(req.trim_end().to_owned());
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
    (path, captured)
}

#[test]
fn rofi_cancel_exits_0() {
    // User dismisses rofi (non-zero exit + empty stdout). The CLI must
    // classify that as cancellation and exit 0 silently — no
    // SetWorkspaceActivities dispatch, so the listener's third accept
    // never resolves (the listener thread just dies on its remove_file
    // cleanup).
    let shim = RofiShimDir::new("cancel");
    shim.install_script("rofi", "exit 1\n");
    let sock = spawn_one_shot_assign_listener("cancel");

    Command::cargo_bin(BIN)
        .unwrap()
        .arg("assign-workspace")
        .env_clear()
        .env("PATH", shim.as_path())
        .env("NIRI_SOCKET", &sock)
        .assert()
        .code(0)
        .stderr(str::is_empty());
}

#[test]
fn rofi_select_dispatches_set_and_exits_0() {
    // Rofi prints "[x] Personal\n" and exits 0 — picker returns a
    // literal `Selected(["Personal"])`. The CLI must dispatch
    // SetWorkspaceActivities with workspace Id(42) and activity Name
    // "Personal"; the listener's third connection receives it and
    // replies Handled → exit 0.
    let shim = RofiShimDir::new("select");
    shim.install_script("rofi", "printf '[x] Personal\\n'\nexit 0\n");
    let (sock, captured) = spawn_one_shot_assign_listener_capturing("select");

    Command::cargo_bin(BIN)
        .unwrap()
        .arg("assign-workspace")
        .env_clear()
        .env("PATH", shim.as_path())
        .env("NIRI_SOCKET", &sock)
        .assert()
        .code(0)
        .stderr(str::is_empty());

    // Assert the wire body of the third request (SetWorkspaceActivities).
    let reqs = captured.lock().unwrap();
    assert_eq!(reqs.len(), 3, "expected exactly 3 IPC requests");
    let set_req = &reqs[2];
    assert!(
        set_req.contains("\"Id\":42"),
        "SetWorkspaceActivities must pin the focused workspace id 42; got: {set_req}",
    );
    assert!(
        set_req.contains("\"Name\":\"Personal\""),
        "SetWorkspaceActivities must carry the selected activity name; got: {set_req}",
    );
}

#[test]
fn rofi_only_one_chains_and_dispatches() {
    // Rofi prints the « Only one… » sentinel → MultiPickerOutcome
    // resolves to ChainSingle. The CLI then spawns `fuzzel`; the fuzzel
    // shim prints "Personal" and exits 0 → PickerOutcome::Selected.
    // SetWorkspaceActivities is dispatched with workspace Id(42) and
    // activity Name "Personal" against the listener's third connection
    // → exit 0.
    let shim = RofiShimDir::new("chain");
    shim.install_script("rofi", "printf '« Only one… »\\n'\nexit 0\n");
    shim.install_script("fuzzel", "printf 'Personal\\n'\nexit 0\n");
    let (sock, captured) = spawn_one_shot_assign_listener_capturing("chain");

    Command::cargo_bin(BIN)
        .unwrap()
        .arg("assign-workspace")
        .env_clear()
        .env("PATH", shim.as_path())
        .env("NIRI_SOCKET", &sock)
        .assert()
        .code(0)
        .stderr(contains("error").not());

    // Assert the wire body of the third request (SetWorkspaceActivities).
    let reqs = captured.lock().unwrap();
    assert_eq!(reqs.len(), 3, "expected exactly 3 IPC requests");
    let set_req = &reqs[2];
    assert!(
        set_req.contains("\"Id\":42"),
        "SetWorkspaceActivities must pin the focused workspace id 42; got: {set_req}",
    );
    assert!(
        set_req.contains("\"Name\":\"Personal\""),
        "SetWorkspaceActivities must carry the chained-picker selection; got: {set_req}",
    );
}

#[test]
fn rofi_missing_from_path_exits_69() {
    // `$PATH` points at an empty tempdir — no `rofi` binary there.
    // `multi_select::ensure_available()` returns PickerUnavailable
    // with the canonical missing-rofi message BEFORE any IPC
    // round-trip. Exit code 69 with stderr naming `rofi`.
    let shim = RofiShimDir::new("missing");
    // Deliberately NOT installing the rofi script — the dir is empty.

    Command::cargo_bin(BIN)
        .unwrap()
        .arg("assign-workspace")
        .env_clear()
        .env("PATH", shim.as_path())
        .env_remove("NIRI_SOCKET")
        .assert()
        .code(69)
        .stderr(contains("picker unavailable").and(contains("rofi")));
}
