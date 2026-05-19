//! Synchronous IPC adapter over the niri Unix socket.
//!
//! This module is the single seam between the CLI and `niri-ipc`:
//!
//! - [`NiriClient`] — the trait every subcommand consumes. One method,
//!   `send(Request) -> Result<Response, IpcError>`. Sync, blocking,
//!   one round-trip per call.
//! - [`SocketClient`] — production impl. Each call connects to
//!   `$NIRI_SOCKET`, writes a JSON-encoded `Request` line, reads a
//!   JSON-encoded `Reply` line, unwraps the outer `Result`. No
//!   persistent state, no connection pooling, no event-stream support.
//! - [`MockClient`] — test-only impl. Subcommand tests queue
//!   `(Request, Reply)` pairs via [`MockClient::expect`]; each
//!   `send()` pops the head and panics on any deviation from the
//!   queued order. [`MockClient::assert_consumed_in_order`] panics if
//!   the queue isn't empty at end-of-test, so a missed IPC call
//!   surfaces as a failure rather than a silent pass.
//! - [`IpcError`] — three carriers (`Transport`, `Decode`, `Server`)
//!   that map cleanly to [`CliError`] variants via `From`. The
//!   write/read loop is open-coded rather than delegated to
//!   `niri_ipc::socket::Socket::send` precisely so a JSON-decode
//!   failure surfaces as `Decode` instead of being flattened into
//!   `Transport` (the helper's `?` on `serde_json::from_str` converts
//!   the JSON error into `io::Error` and loses the type).
//!
//! ## Test injection
//!
//! Subcommands take `&mut dyn NiriClient`. Production wiring goes
//! through [`make_client`]; in non-test builds that returns a fresh
//! [`SocketClient`]. In test builds it first consults a thread-local
//! override populated by [`install_mock`], so a subcommand test can
//! drop in a `MockClient` without touching `$NIRI_SOCKET` or threading
//! the client through every call site.

use std::fmt;
use std::io::{self, BufRead, BufReader, Write};

use niri_ipc::socket::SOCKET_PATH_ENV;
use niri_ipc::{Reply, Request, Response};

use crate::error::{CliError, MalformedResponseSource};

/// Single round-trip over the niri socket: write a `Request`, read a
/// `Response`. Implementations are synchronous and consume `&mut
/// self` so a future stateful client (e.g. a connection pool) can
/// fit the same trait without changing call sites.
pub(crate) trait NiriClient {
    /// Sends `req` and returns the typed `Response`, or an
    /// [`IpcError`] classified by failure mode.
    ///
    /// Implementations guarantee: exactly one round-trip per call;
    /// the underlying connection is not reused across calls; cancel
    /// safety is not provided (callers must not interrupt).
    fn send(&mut self, req: Request) -> Result<Response, IpcError>;
}

/// Classified IPC failure.
///
/// | Variant     | Trigger                                                            | Maps to                              |
/// | ----------- | ------------------------------------------------------------------ | ------------------------------------ |
/// | `Transport` | `$NIRI_SOCKET` unset, connect refused, read/write IO failure       | [`CliError::SocketUnavailable`] (69) |
/// | `Decode`    | reply line did not deserialize as a `Reply`                        | [`CliError::MalformedResponse`] (65) |
/// | `Server`    | compositor responded with `Reply::Err(String)`                     | [`CliError::MalformedResponse`] (65) |
///
/// The `Server` variant routes to 65 (`EX_DATAERR`) rather than 66
/// (`EX_NOINPUT`) because the compositor's error string is opaque to
/// the CLI — we can't safely classify it as "input not found" without
/// inspecting the string contents, which would be brittle.
#[derive(Debug)]
pub(crate) enum IpcError {
    /// `$NIRI_SOCKET` unset, connect refused, or read/write IO failure
    /// during the round-trip.
    Transport(io::Error),
    /// The reply line on the wire was not valid JSON or did not
    /// deserialize to a `Reply`.
    Decode(serde_json::Error),
    /// The compositor returned `Reply::Err(String)` instead of
    /// `Ok(Response)`. The wrapped string is whatever the server sent.
    Server(String),
}

impl fmt::Display for IpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IpcError::Transport(io) => write!(f, "ipc transport: {io}"),
            IpcError::Decode(e) => write!(f, "ipc decode: {e}"),
            IpcError::Server(msg) => write!(f, "ipc server error: {msg}"),
        }
    }
}

impl std::error::Error for IpcError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            IpcError::Transport(io) => Some(io),
            IpcError::Decode(e) => Some(e),
            IpcError::Server(_) => None,
        }
    }
}

impl From<IpcError> for CliError {
    fn from(e: IpcError) -> CliError {
        match e {
            IpcError::Transport(io) => CliError::SocketUnavailable(io),
            IpcError::Decode(json) => {
                CliError::MalformedResponse(MalformedResponseSource::Decode(json))
            }
            IpcError::Server(msg) => {
                CliError::MalformedResponse(MalformedResponseSource::Server(msg))
            }
        }
    }
}

/// Production [`NiriClient`] that opens a fresh Unix-socket connection
/// per `send()` call.
///
/// Stateless on purpose: persisting a `BufReader<UnixStream>` would
/// force every error path to invalidate the connection, and v1 is
/// strictly request/reply (no event-stream support).
pub(crate) struct SocketClient {
    _private: (),
}

impl SocketClient {
    /// Constructs a new client. Cheap; defers all IO to [`Self::send`].
    pub(crate) fn new() -> Self {
        SocketClient { _private: () }
    }
}

impl NiriClient for SocketClient {
    fn send(&mut self, req: Request) -> Result<Response, IpcError> {
        // Resolve the socket path. `niri_ipc::socket::Socket::connect`
        // does the same env lookup but its `io::Result` would only
        // be folded into our `Transport` arm — we duplicate it here
        // anyway because the write/read loop below must be open-coded
        // (see module docs for the `Socket::send` collapsing problem).
        let path = std::env::var_os(SOCKET_PATH_ENV).ok_or_else(|| {
            IpcError::Transport(io::Error::new(
                io::ErrorKind::NotFound,
                format!("{SOCKET_PATH_ENV} is not set, are you running this within niri?"),
            ))
        })?;

        let stream = std::os::unix::net::UnixStream::connect(&path).map_err(IpcError::Transport)?;
        let mut reader = BufReader::new(stream);

        let mut buf = serde_json::to_string(&req)
            .expect("Request serialization is infallible: no map keys, no IO");
        buf.push('\n');
        reader
            .get_mut()
            .write_all(buf.as_bytes())
            .map_err(IpcError::Transport)?;

        buf.clear();
        reader.read_line(&mut buf).map_err(IpcError::Transport)?;

        // Decode the reply line. This is the step where we deliberately
        // do not delegate to `Socket::send`: it would convert this
        // `serde_json::Error` into `io::Error` and collapse it into our
        // `Transport` arm, costing us the ability to map back to
        // `MalformedResponse` (65) vs `SocketUnavailable` (69).
        let reply: Reply = serde_json::from_str(&buf).map_err(IpcError::Decode)?;

        match reply {
            Ok(response) => Ok(response),
            Err(msg) => Err(IpcError::Server(msg)),
        }
    }
}

// ----------------------------------------------------------------------
// Test injection — production builds get a zero-cost direct constructor;
// `cfg(test)` builds add a thread-local override seam for subcommand
// tests.
// ----------------------------------------------------------------------

#[cfg(not(test))]
pub(crate) fn make_client() -> Box<dyn NiriClient> {
    Box::new(SocketClient::new())
}

#[cfg(test)]
pub(crate) fn make_client() -> Box<dyn NiriClient> {
    if let Some(mock) = MOCK_OVERRIDE.with(|cell| cell.borrow_mut().take()) {
        mock
    } else {
        Box::new(SocketClient::new())
    }
}

#[cfg(test)]
thread_local! {
    static MOCK_OVERRIDE: std::cell::RefCell<Option<Box<dyn NiriClient>>> =
        const { std::cell::RefCell::new(None) };
}

/// RAII guard that clears the thread-local mock override on drop, so
/// a test panic mid-flight cannot leak the override into a sibling
/// test running on the same thread.
#[cfg(test)]
#[must_use = "the mock override is cleared when this guard is dropped"]
pub(crate) struct MockGuard;

#[cfg(test)]
impl Drop for MockGuard {
    fn drop(&mut self) {
        MOCK_OVERRIDE.with(|cell| {
            *cell.borrow_mut() = None;
        });
    }
}

/// Installs `mock` as the [`make_client`] override for the current
/// thread. The returned guard clears the override on drop, ensuring
/// sibling tests on the same thread see no leakage. Nested
/// `install_mock` calls are not supported.
#[cfg(test)]
pub(crate) fn install_mock(mock: MockClient) -> MockGuard {
    MOCK_OVERRIDE.with(|cell| {
        *cell.borrow_mut() = Some(Box::new(mock));
    });
    MockGuard
}

// ----------------------------------------------------------------------
// Mock client. Gated on `test` because the production binary must not
// link a queue-and-panic harness.
// ----------------------------------------------------------------------

/// Test double that replays a fixed `(Request, Reply)` script.
///
/// Subcommand tests build a `MockClient`, queue the exact requests
/// they expect the code-under-test to issue with
/// [`MockClient::expect`], install the client via [`install_mock`],
/// invoke the subcommand, then call
/// [`MockClient::assert_consumed_in_order`] to confirm every queued
/// pair was consumed.
///
/// Mismatches (unexpected request, wrong request shape, leftover
/// queue) all panic — silent passes are the failure mode this harness
/// exists to prevent.
///
/// If a test drops a `MockClient` with un-consumed entries (and isn't
/// already panicking), the drop itself panics —
/// `assert_consumed_in_order` becomes optional but still recommended
/// for explicit intent.
#[cfg(test)]
pub(crate) struct MockClient {
    queue: std::collections::VecDeque<(Request, Reply)>,
}

#[cfg(test)]
impl MockClient {
    pub(crate) fn new() -> Self {
        MockClient {
            queue: std::collections::VecDeque::new(),
        }
    }

    /// Queues a `(request, reply)` pair. The `request` must match
    /// (by JSON-round-trip equality) the next `send()` invocation, in
    /// the order they were queued.
    ///
    /// **Coverage limitation:** `reply` is `Reply`, which only carries
    /// `Ok(Response)` or `Err(String)` (the `IpcError::Server` leg).
    /// `IpcError::Transport` and `IpcError::Decode` are not injectable
    /// through this interface — those paths in `send_expect_handled`
    /// and related helpers are untested at the unit level.
    pub(crate) fn expect(&mut self, req: Request, reply: Reply) {
        self.queue.push_back((req, reply));
    }

    /// Returns the number of unconsumed `(request, reply)` pairs
    /// remaining in the queue. Useful in tests that need to verify
    /// exactly which IPC calls were issued without triggering a panic.
    pub(crate) fn remaining_count(&self) -> usize {
        self.queue.len()
    }

    /// Panics if the queue is non-empty. Call at the end of every
    /// test that uses a `MockClient` so a missed IPC call surfaces
    /// as a failure rather than a silent pass.
    pub(crate) fn assert_consumed_in_order(&self) {
        if !self.queue.is_empty() {
            let remaining: Vec<String> = self
                .queue
                .iter()
                .map(|(req, _)| format!("{req:?}"))
                .collect();
            panic!(
                "MockClient queue not fully consumed; remaining: [{}]",
                remaining.join(", "),
            );
        }
    }
}

#[cfg(test)]
fn request_eq(a: &Request, b: &Request) -> bool {
    // `Request` does not derive `PartialEq`. JSON round-trip equality
    // is the cheapest stable identity check that survives `#[non_exhaustive]`
    // additions to the enum.
    match (serde_json::to_string(a), serde_json::to_string(b)) {
        (Ok(sa), Ok(sb)) => sa == sb,
        _ => false,
    }
}

#[cfg(test)]
impl NiriClient for MockClient {
    fn send(&mut self, req: Request) -> Result<Response, IpcError> {
        let (expected, reply) = self
            .queue
            .pop_front()
            .unwrap_or_else(|| panic!("unexpected IPC request: {req:?}; queue exhausted"));
        if !request_eq(&expected, &req) {
            panic!("MockClient request mismatch: expected {expected:?}, got {req:?}",);
        }
        match reply {
            Ok(resp) => Ok(resp),
            Err(msg) => Err(IpcError::Server(msg)),
        }
    }
}

#[cfg(test)]
impl Drop for MockClient {
    fn drop(&mut self) {
        if !self.queue.is_empty() && !std::thread::panicking() {
            panic!(
                "MockClient dropped with {} unconsumed expectation(s); \
                 remaining: {:?}; \
                 call assert_consumed_in_order() or fix the test logic",
                self.queue.len(),
                self.queue.iter().map(|(req, _)| req).collect::<Vec<_>>(),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;
    use std::os::unix::net::UnixListener;
    use std::sync::Mutex;
    use std::thread;

    use niri_ipc::{Reply, Request, Response};

    use super::*;
    use crate::error::{CliError, MalformedResponseSource};

    // `$NIRI_SOCKET` is process-global; tests that mutate it must run
    // serialized. Rust 2024 made `std::env::set_var` unsafe — wrap
    // accordingly inside the lock.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct EnvGuard {
        prev: Option<std::ffi::OsString>,
    }

    impl EnvGuard {
        fn set(value: &std::path::Path) -> Self {
            let prev = std::env::var_os(SOCKET_PATH_ENV);
            // SAFETY: ENV_LOCK serializes all `set_var` / `remove_var`
            // calls on `NIRI_SOCKET` within this test module. No other
            // thread in the test process touches the variable.
            unsafe { std::env::set_var(SOCKET_PATH_ENV, value) };
            EnvGuard { prev }
        }

        fn unset() -> Self {
            let prev = std::env::var_os(SOCKET_PATH_ENV);
            // SAFETY: same as above.
            unsafe { std::env::remove_var(SOCKET_PATH_ENV) };
            EnvGuard { prev }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            // SAFETY: same as above; the guard is constructed only inside
            // the ENV_LOCK-held region.
            unsafe {
                match self.prev.take() {
                    Some(v) => std::env::set_var(SOCKET_PATH_ENV, v),
                    None => std::env::remove_var(SOCKET_PATH_ENV),
                }
            }
        }
    }

    /// Waits for the listener thread to `bind()` on `path`. Polls
    /// `path.exists()` instead of blocking on a fixed sleep so the fast
    /// path stays fast (~0 ms) while still tolerating up to 500 ms of
    /// CI scheduling skew.
    fn wait_for_socket_bind(path: &std::path::Path) {
        for _ in 0..50 {
            if path.exists() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    /// Returns a unique-to-this-test socket path under `/tmp`. Avoids
    /// `tempfile` as a dev-dep; uses PID + a per-call counter to keep
    /// concurrent test runs disjoint.
    fn unique_socket_path(tag: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "jiji-activities-test-{}-{}-{}.sock",
            std::process::id(),
            n,
            tag,
        ))
    }

    /// Spawns a listener thread that accepts one connection, reads one
    /// request line, and writes `reply_line` verbatim (without an
    /// implicit newline — supply one in `reply_line` for valid framing,
    /// omit it to simulate truncation).
    fn spawn_listener(
        path: std::path::PathBuf,
        reply_line: String,
    ) -> thread::JoinHandle<std::io::Result<String>> {
        thread::spawn(move || -> std::io::Result<String> {
            let listener = UnixListener::bind(&path)?;
            let (mut sock, _) = listener.accept()?;
            let mut reader = std::io::BufReader::new(sock.try_clone()?);
            let mut req_line = String::new();
            reader.read_line(&mut req_line)?;
            sock.write_all(reply_line.as_bytes())?;
            // Best-effort cleanup. Don't fail the test if it races.
            let _ = std::fs::remove_file(&path);
            Ok(req_line)
        })
    }

    // ---- SocketClient tests ----

    #[test]
    fn socket_client_round_trip() {
        let _lock = ENV_LOCK.lock().expect("env lock not poisoned");
        let path = unique_socket_path("round-trip");
        let reply = Reply::Ok(Response::Version("test".into()));
        let mut reply_line = serde_json::to_string(&reply).expect("reply serializes");
        reply_line.push('\n');
        let handle = spawn_listener(path.clone(), reply_line);

        wait_for_socket_bind(&path);

        let _guard = EnvGuard::set(&path);
        let mut client = SocketClient::new();
        let resp = client.send(Request::Version).expect("send succeeds");
        match resp {
            Response::Version(s) => assert_eq!(s, "test"),
            other => panic!("unexpected response variant: {other:?}"),
        }

        let req_line = handle
            .join()
            .expect("listener thread panicked")
            .expect("listener IO");
        assert!(req_line.contains("Version"), "request line: {req_line}");
    }

    #[test]
    fn socket_client_missing_env_is_transport() {
        let _lock = ENV_LOCK.lock().expect("env lock not poisoned");
        let _guard = EnvGuard::unset();
        let mut client = SocketClient::new();
        let err = client.send(Request::Version).expect_err("must fail");
        match err {
            IpcError::Transport(io) => {
                assert_eq!(io.kind(), io::ErrorKind::NotFound);
            }
            other => panic!("expected Transport(NotFound), got {other:?}"),
        }
    }

    #[test]
    fn socket_client_connect_path_missing_is_transport() {
        let _lock = ENV_LOCK.lock().expect("env lock not poisoned");
        let path = unique_socket_path("refused");
        // Ensure the path does not exist — no listener bound.
        let _ = std::fs::remove_file(&path);
        let _guard = EnvGuard::set(&path);
        let mut client = SocketClient::new();
        let err = client.send(Request::Version).expect_err("must fail");
        match err {
            IpcError::Transport(io) => {
                assert_eq!(io.kind(), io::ErrorKind::NotFound);
            }
            other => panic!("expected Transport(NotFound), got {other:?}"),
        }
    }

    #[test]
    fn socket_client_garbage_reply_is_decode() {
        let _lock = ENV_LOCK.lock().expect("env lock not poisoned");
        let path = unique_socket_path("garbage");
        let handle = spawn_listener(path.clone(), "not json\n".into());
        wait_for_socket_bind(&path);
        let _guard = EnvGuard::set(&path);
        let mut client = SocketClient::new();
        let err = client.send(Request::Version).expect_err("must fail");
        handle
            .join()
            .expect("listener thread panicked")
            .expect("listener IO failed");
        assert!(
            matches!(err, IpcError::Decode(_)),
            "expected Decode, got {err:?}",
        );
    }

    #[test]
    fn socket_client_server_error_reply() {
        let _lock = ENV_LOCK.lock().expect("env lock not poisoned");
        let path = unique_socket_path("server-err");
        let reply: Reply = Err("no such activity".to_string());
        let mut reply_line = serde_json::to_string(&reply).expect("reply serializes");
        reply_line.push('\n');
        let handle = spawn_listener(path.clone(), reply_line);
        wait_for_socket_bind(&path);
        let _guard = EnvGuard::set(&path);
        let mut client = SocketClient::new();
        let err = client.send(Request::Version).expect_err("must fail");
        handle
            .join()
            .expect("listener thread panicked")
            .expect("listener IO failed");
        match err {
            IpcError::Server(msg) => assert_eq!(msg, "no such activity"),
            other => panic!("expected Server, got {other:?}"),
        }
    }

    // ---- MockClient tests ----

    #[test]
    fn mock_client_remaining_count_tracks_queue() {
        let mut client = MockClient::new();
        assert_eq!(client.remaining_count(), 0);
        client.expect(Request::Activities, Ok(Response::Activities(vec![])));
        assert_eq!(client.remaining_count(), 1);
        let _ = client.send(Request::Activities);
        assert_eq!(client.remaining_count(), 0);
    }

    #[test]
    fn mock_client_pops_in_order() {
        let mut client = MockClient::new();
        client.expect(Request::Version, Ok(Response::Version("v1".into())));
        client.expect(Request::Activities, Ok(Response::Activities(vec![])));

        let r1 = client.send(Request::Version).expect("first send ok");
        assert!(matches!(r1, Response::Version(ref s) if s == "v1"));
        let r2 = client.send(Request::Activities).expect("second send ok");
        assert!(matches!(r2, Response::Activities(_)));

        client.assert_consumed_in_order();
    }

    #[test]
    #[should_panic(expected = "unexpected IPC request")]
    fn mock_client_panics_on_unexpected_request() {
        let mut client = MockClient::new();
        let _ = client.send(Request::Version);
    }

    #[test]
    #[should_panic(expected = "MockClient request mismatch")]
    fn mock_client_panics_on_request_mismatch() {
        let mut client = MockClient::new();
        client.expect(Request::Version, Ok(Response::Version("v".into())));
        let _ = client.send(Request::Activities);
    }

    #[test]
    #[should_panic(expected = "queue not fully consumed")]
    fn mock_client_assert_consumed_in_order_panics_when_dirty() {
        let mut client = MockClient::new();
        client.expect(Request::Version, Ok(Response::Version("v".into())));
        client.assert_consumed_in_order();
    }

    // ---- IpcError -> CliError mapping ----

    #[test]
    fn ipc_error_transport_maps_to_socket_unavailable() {
        let ipc = IpcError::Transport(io::Error::new(io::ErrorKind::ConnectionRefused, "x"));
        let cli: CliError = ipc.into();
        assert_eq!(cli.exit_code(), 69);
        assert!(matches!(cli, CliError::SocketUnavailable(_)));
    }

    #[test]
    fn ipc_error_decode_maps_to_malformed_response() {
        let json_err =
            serde_json::from_str::<()>("not json").expect_err("malformed JSON fails to parse");
        let ipc = IpcError::Decode(json_err);
        let cli: CliError = ipc.into();
        assert_eq!(cli.exit_code(), 65);
        assert!(matches!(
            cli,
            CliError::MalformedResponse(MalformedResponseSource::Decode(_))
        ));
    }

    #[test]
    fn ipc_error_server_maps_to_malformed_response() {
        let ipc = IpcError::Server("opaque".into());
        let cli: CliError = ipc.into();
        assert_eq!(cli.exit_code(), 65);
        match cli {
            CliError::MalformedResponse(MalformedResponseSource::Server(msg)) => {
                assert_eq!(msg, "opaque");
            }
            other => panic!("expected Server source, got {other:?}"),
        }
    }

    // ---- MockGuard / install_mock ----

    #[test]
    fn mock_guard_drop_clears_thread_local() {
        {
            let _g = install_mock(MockClient::new());
            // Install the mock and drop the guard at scope exit. The post-scope
            // check below verifies the thread-local override is cleared.
        }
        // After scope exit, the override must be cleared. A fresh
        // make_client() must return the production SocketClient, which
        // fails Transport(NotFound) on unset env — not a mock-queue-empty
        // panic.
        let _lock = ENV_LOCK.lock().expect("env lock not poisoned");
        let _guard = EnvGuard::unset();
        let mut c = make_client();
        let err = c.send(Request::Version).unwrap_err();
        assert!(
            matches!(err, IpcError::Transport(_)),
            "expected Transport after mock guard dropped, got {err:?}",
        );
    }

    // ---- EnvGuard round-trip ----

    #[test]
    fn env_guard_drop_restores_prior_value() {
        let _lock = ENV_LOCK.lock().expect("env lock not poisoned");
        // Outer guard captures None (assuming prior state was unset, which it
        // is since ENV_LOCK serializes and prior tests clean up). Inner guard
        // captures "/sentinel-prior". Inner drops first → restores
        // "/sentinel-prior". Outer drops → restores None.
        let _outer = EnvGuard::set(std::path::Path::new("/sentinel-prior"));
        {
            let _inner = EnvGuard::set(std::path::Path::new("/sentinel-inner"));
            assert_eq!(
                std::env::var("NIRI_SOCKET").as_deref(),
                Ok("/sentinel-inner")
            );
        }
        assert_eq!(
            std::env::var("NIRI_SOCKET").as_deref(),
            Ok("/sentinel-prior"),
            "EnvGuard drop must restore prior value",
        );
        // _outer's drop restores NIRI_SOCKET's prior state (None, since the
        // lock acquirer started clean).
    }

    #[test]
    fn env_guard_drop_restores_unset_state() {
        let _lock = ENV_LOCK.lock().expect("env lock not poisoned");
        // SAFETY: env mutation serialized by ENV_LOCK.
        unsafe {
            std::env::remove_var("NIRI_SOCKET");
        }
        {
            let _g = EnvGuard::set(std::path::Path::new("/sentinel-inner"));
            assert_eq!(
                std::env::var("NIRI_SOCKET").as_deref(),
                Ok("/sentinel-inner"),
                "EnvGuard::set must apply the new value when prior was unset",
            );
        }
        assert!(
            std::env::var_os("NIRI_SOCKET").is_none(),
            "EnvGuard drop must restore unset state when prior was unset",
        );
    }
}
