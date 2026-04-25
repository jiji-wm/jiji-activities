# `niri-activities` design — Phase 3 of activities

Implementer-grade design for the user-facing CLI of KDE-style Activities on niri. Owns Phase 3 of the activities workstream; takes over from §8 of the compositor DD (`~/projects/desktop/de/niri/docs/activities/design.md`), which now redirects here.

This DD is the source of truth for everything user-facing: subcommand surface, error model, exit codes, IPC strategy, picker integration, output formats, test strategy. The compositor DD remains the source of truth for IPC types and compositor-side semantics.

---

## 1. Goal and scope

`niri-activities` is the CLI surface for Activities — a workspace-grouping abstraction landed compositor-side in Phases 1–2. Each invocation makes one or more sequential niri IPC calls, then exits. **No daemon, no persistent state, no event-stream subscription** in v1; those would be justified only by a UX bottleneck the per-invocation model can't meet (live activity indicator in a panel, built-in layer-shell picker with pre-loaded state).

Out of scope for v1, parked for v2: built-in gtk4-layer-shell picker (Option A from compositor DD §8.3), daemon mode, D-Bus interface, save-to-config-on-exit semantics beyond the explicit `save` subcommand.

## 2. Source-of-truth split

| Concern | Lives in | DD |
|---|---|---|
| Activities data model, IPC types, compositor layout machinery | fork at `~/projects/desktop/de/niri/niri/gajdusek/` | `~/projects/desktop/de/niri/docs/activities/design.md` |
| User-facing CLI surface, error model, picker integration | this repo | this file |

The compositor DD exposes `Request::Activities`, `Action::SwitchActivity`, `Action::CreateActivity`, etc. via the fork's `niri-ipc` crate. This DD consumes those types; it never invents them. When the compositor DD lands a new action, this DD opens a sub-phase that wraps it.

## 3. CLI surface

Lifted from compositor DD §8.2; refined here per the error/exit-code spec in §4.1. Each subcommand documents its IPC call(s), success output, and error mapping.

### `niri-activities switch [<name>]`
- IPC: `Action::SwitchActivity { name }` (named) or fuzzel picker → same action (no arg).
- Success: silent, exit 0.
- Errors: unknown name → `ActivityNotFound` (66); already-active → no-op exit 0.

### `niri-activities switch-previous` (alias: `toggle`)
- IPC: `Action::SwitchActivityPrevious`.
- Success: silent, exit 0. Errors: none specific.

### `niri-activities move-window [<name>]`
- IPC: `Action::MoveWindowToActivity { name }`. Operates on focused window.
- Picker variant when no arg.

### `niri-activities move-workspace [<name>]`
- IPC: `Action::MoveWorkspaceToActivity { name }`. Operates on focused workspace.
- Picker variant when no arg.

### `niri-activities assign-workspace`
- Edits the activity-membership set of the focused workspace. UX is the open question parked in Phase 3.0 — see §4.4.

### `niri-activities create <name>`
- IPC: `Action::CreateActivity { name }`. Errors: name collision → `EX_CANTCREAT` (73).

### `niri-activities remove <name>`
- IPC: `Action::RemoveActivity { name }`. Errors: unknown name → `ActivityNotFound` (66); attempting to remove a config-declared activity surfaces the compositor's error verbatim.

### `niri-activities save <name>`
- Not an IPC call. Edits the user's `config.kdl` (appending `activity "name"`), then `Action::ReloadConfig`. Config-edit strategy decided in Phase 3.6.

### `niri-activities list [--json | --format=<spec>]`
- IPC: `Request::Activities`. Output format per §4.5.

## 4. Architectural decisions

Each subsection has a **Proposed:** lead sentence stating the agent recommendation. Phase 3.0's checkboxes ratify (or amend) these proposals before any implementation begins.

### 4.1 Error model and exit codes

**Proposed:** propagate via `anyhow::Error`; map to `sysexits.h`-style codes at the `main()` boundary through a typed inner error enum.

| Code | sysexits constant | Trigger |
|---|---|---|
| 0 | EX_OK | success, including silent picker cancellation |
| 64 | EX_USAGE | clap argument errors, unknown subcommand |
| 65 | EX_DATAERR | malformed IPC response (reply shape didn't match Request) |
| 66 | EX_NOINPUT | activity name not found |
| 69 | EX_UNAVAILABLE | `$NIRI_SOCKET` unset; connect refused |
| 70 | EX_SOFTWARE | panic, programming-error path (a stub left unimplemented, etc.) |
| 73 | EX_CANTCREAT | create activity failed (name collision, config-edit failure) |

**Picker cancellation is exit 0**, not an error: fuzzel exiting non-zero with empty stdout means the user backed out, which is a normal outcome. Only IPC errors, type-mismatch errors, and explicit failures map to non-zero.

`anyhow::Error` carries chained context for stderr; the `main()` dispatcher does `match err.downcast_ref::<CliError>()` against a typed enum (`SocketUnavailable`, `ActivityNotFound`, `MalformedResponse`, `CantCreate`, ...) to pick the exit code. Fallback for un-typed errors is exit 1.

### 4.2 IPC strategy: fork's `niri-ipc` via git+rev

**Proposed:** depend on the fork's `niri-ipc` crate via a pinned git rev. Activities-specific IPC variants (`Request::Activities`, `Action::SwitchActivity`, etc.) only exist on the fork — crates.io's `niri-ipc` has none of them.

```toml
[dependencies]
niri-ipc = { git = "https://github.com/gajdusek/niri", rev = "<pin>" }
```

**Why not shell out to `niri msg`?** Two reasons that compound:
- `niri msg` is the fork binary's own CLI, which forwards to the same IPC types we'd link. Going through a process boundary means parsing stringified JSON (brittle), and re-discovering structured errors that the typed Request/Response already gives us. We pay the brittleness cost without the abstraction win.
- `niri msg` returns text to stdout, not a typed `Response`. We'd hand-roll a parser per command and lose anyhow's context chaining.

**Why not path-depend on `../../niri/niri/gajdusek/niri-ipc`?** Path deps assume the workspace layout is identical on every machine that builds this crate. Git+rev ships everywhere with no implicit cross-repo coupling. The coordination cost (bumping a rev when the fork's IPC moves) is the same; the deployability is much better.

**Bumping the rev becomes a deliberate sub-phase.** When the fork lands a new activity-related IPC variant, this DD opens a sub-phase that bumps the rev and exposes the variant via a new subcommand or flag. Drift is detected by the test layer in §4.6 (an integration test that exercises every variant we wrap will fail to compile if the rev moves and a variant changes shape).

### 4.3 IPC client trait

**Proposed:** hide socket marshalling behind a trait so subcommand logic can be unit-tested without standing up a real socket.

```rust
pub(crate) trait NiriClient {
    fn send(&mut self, req: Request) -> Result<Response, IpcError>;
}

pub(crate) struct SocketClient { /* one-shot connection per send(), talks $NIRI_SOCKET */ }
pub(crate) struct MockClient   { canned: VecDeque<(Request, Response)> }
```

Subcommand logic takes `&mut dyn NiriClient`. Tests inject `MockClient` (with a queue of expected request/reply pairs; panics on unexpected request to surface test gaps); the binary wires `SocketClient`. This is the standard rust-CLI test pattern, and is a precondition for `assert_cmd` to be useful — every test would otherwise need a live niri or a Unix-socket fixture.

`MockClient::expect(req, reply)` and `assert_consumed_in_order()` form the test-level contract; the trait surface stays minimal (one method).

### 4.4 Fuzzel picker contract

**Proposed:** spawn `fuzzel --dmenu --prompt <subcommand-prompt>`, write items to stdin (newline-separated), read selected line from stdout. Cancellation (fuzzel exits non-zero with empty stdout) → exit 0 silently. No fallback to rofi/dmenu in v1; fuzzel is the default niri-ecosystem picker (cf. anyrun, the niri-ror examples), and adding a backend abstraction is premature.

| Subcommand (no arg) | Picker prompt | Items |
|---|---|---|
| `switch` | `Switch to activity:` | activity names, focused first |
| `move-window` | `Move window to activity:` | activity names |
| `move-workspace` | `Move workspace to activity:` | activity names |

**Open question for Phase 3.0** (`assign-workspace` UX): per compositor DD §3.2, `assign-workspace` is a *checkbox* UI — the user picks the subset of activities that own a workspace. fuzzel `--dmenu` is single-select. Three candidate paths:

- **(A) Loop one fuzzel prompt per activity** ("Add to Work? yes / no", repeat for each). Clunky; ships in v1.
- **(B) Non-interactive flags only** — `niri-activities assign-workspace --add Work --remove Personal`. Clean for scripting; no interactive UX.
- **(C) Defer `assign-workspace` to v2** alongside the built-in gtk4-layer-shell picker (Option A in compositor DD §8.3).

This is a UX decision where the user's preference matters. Phase 3.0 has an explicit ratification box for it.

### 4.5 `list` output format

**Proposed:** three modes, controlled by mutually exclusive flags.

**Default (no flag) — human-readable plain text.** One line per activity; the focused activity is prefixed `*`, others ` `:

```
* Work       (config) [3 workspaces, 12 windows]
  Personal   (config) [2 workspaces, 5 windows]
  Gaming     (runtime) [1 workspace, 0 windows]
```

Column widths are computed from the longest name + a 2-space gutter. Truncation rule: never truncate; if a name is wider than the terminal, the line wraps via the terminal's wrap behavior (no manual truncation).

**`--json` — machine-readable JSON.** Schema (stable contract, version-bump if shape changes):

```json
[
  {
    "name": "Work",
    "kind": "config",
    "focused": true,
    "workspaces": [
      {"id": 1, "name": "main", "sticky": false}
    ],
    "window_count": 12
  }
]
```

**`--format=<spec>` — comma-separated fields per line.** E.g., `--format=name,kind,focused`:

```
Work,config,true
Personal,config,false
Gaming,runtime,false
```

Recognized fields: `name`, `kind`, `focused`, `workspace_count`, `window_count`. Unknown field → `EX_USAGE`. Useful for scripting (`niri-activities list --format=name | rofi -dmenu`).

`--json` and `--format=` are mutually exclusive (clap validates). Both override the default.

### 4.6 Test strategy

**Proposed:** three layers, in increasing fixture cost.

**Pure-function unit tests** in each module — parsing, formatting, error mapping. Fastest feedback, no fixtures. Live `#[cfg(test)] mod tests` in the same source file as the function under test.

**Subcommand integration tests via `MockClient`** (per §4.3) plus `assert_cmd` for the binary boundary. Each subcommand has at minimum a golden-path test, an error-path test, and (where applicable) a picker-cancellation test. `MockClient` is wired into the binary via a test-only factory: a `#[cfg(test)]` `pub fn make_client()` that the test overrides via a thread-local, or via an env var the binary checks first. Decided concretely in Phase 3.2.

**End-to-end smoke test against a real niri**, gated `#[ignore]`. Manual run (`cargo test -- --ignored`); not part of `cargo test` default. Asserts side effects (post-action workspace state via `niri msg`) rather than process exit codes.

`cargo test --all` runs unit + MockClient integration; `cargo test -- --ignored` adds the smoke layer.

---

## 5. Phases

### Phase 3.0 — Design ratification (no code)

Each box is a human-gated decision. The architect refuses to plan Phase 3.1+ until every box is `[x]` or amended. **Proposed:** entries are agent recommendations; the human ratifies or amends in-place before the loop drives implementation.

- [ ] Error model & exit codes — see §4.1. **Proposed:** anyhow + sysexits.h mapping per the table.
- [ ] IPC strategy — see §4.2. **Proposed:** fork's `niri-ipc` via git+rev (pin TBD in 3.1).
- [ ] IPC client trait — see §4.3. **Proposed:** `NiriClient` trait, `SocketClient` + `MockClient` impls.
- [ ] Fuzzel picker contract — see §4.4. **Proposed:** `--dmenu` single-select, no rofi fallback in v1.
- [ ] **Open:** `assign-workspace` UX — see §4.4 sub-question. Pick (A) loop, (B) flags-only, or (C) defer to v2.
- [ ] `list` output format — see §4.5. **Proposed:** default plain / `--json` / `--format=<spec>`.
- [ ] Test strategy — see §4.6. **Proposed:** unit + MockClient/assert_cmd integration + ignored e2e smoke.

### Phase 3.1 — Skeleton & error machinery

- [ ] Add Cargo deps ratified in 3.0: `clap` (derive), `niri-ipc` (git+rev — pin to fork HEAD at the time the box is landed and record the rev in the commit message), `serde`, `serde_json`, `anyhow`. **Lock the rev to the actual fork HEAD; do not invent a rev.**
- [ ] `clap`-based subcommand dispatch matching §3. Each subcommand stub prints `not implemented` to stderr and exits 70 (EX_SOFTWARE). Top-level binary still produces useful `--help`.
- [ ] `CliError` enum (typed: `SocketUnavailable`, `ActivityNotFound`, `MalformedResponse`, `CantCreate`, `Usage`) with rustdoc on each variant naming the trigger condition. `main()` dispatcher maps via `downcast_ref` to exit codes per §4.1.
- [ ] `--version` prints `env!("CARGO_PKG_VERSION")`.
- [ ] Unit tests for the error → exit code mapping (one test per code). Pin clippy baseline (likely zero).

### Phase 3.2 — IPC adapter

- [ ] `NiriClient` trait + `SocketClient` impl per §4.3. `SocketClient::send` connects to `$NIRI_SOCKET`, sends one `Request`, awaits one `Response`. Connection per call (no persistent state).
- [ ] `MockClient` impl with a `VecDeque<(Request, Response)>` queue. `expect(req, reply)` enqueues; `send` panics on unexpected request to surface test gaps; `assert_consumed_in_order()` for end-of-test invariant.
- [ ] Map niri-ipc transport errors: connect-refused / `$NIRI_SOCKET` unset → `SocketUnavailable`; reply-shape mismatch → `MalformedResponse`. Concrete error-variant table in the rustdoc on `IpcError`.
- [ ] Wiring strategy for tests (the §4.6 open detail): pick env-var or thread-local injection for `make_client()`. Document the chosen mechanism inline.
- [ ] Unit tests: `SocketClient` against a temp Unix socket fixture (single tokio task / std-thread accepts one connection, replies with a fixed `Response`). `MockClient` panic-on-unexpected coverage.

### Phase 3.3 — `list` subcommand

- [ ] Plain output per §4.5. Edge cases: zero activities → empty stdout; long names → no truncation.
- [ ] `--json` output per §4.5; matches the documented schema exactly.
- [ ] `--format=<spec>` per §4.5; unknown field → `EX_USAGE`.
- [ ] `--json` and `--format=` mutually exclusive (clap-level).
- [ ] Integration tests via `MockClient` + `assert_cmd`: golden plain output (3 activities, focused middle), golden JSON, three `--format=` variants, zero-activities plain, zero-activities JSON.

### Phase 3.4 — `switch <name>` subcommand (no picker yet)

- [ ] Dispatch IPC `Action::SwitchActivity { name }`.
- [ ] Unknown name → `ActivityNotFound` → exit 66 (the compositor returns a structured error; map it).
- [ ] Already-active name → no-op silently, exit 0 (verify against compositor DD §5.3 — switching to the active activity is a documented no-op).
- [ ] Integration tests: golden, unknown name, already-active.

### Phase 3.5 — fuzzel picker

- [ ] Spawn fuzzel via `std::process::Command`, pipe items to stdin (one activity per line), read stdout selection.
- [ ] Cancellation (non-zero exit + empty stdout) → exit 0 silently.
- [ ] `niri-activities switch` (no arg) opens picker, then dispatches §3.4 path with the chosen name.
- [ ] Integration test: shim binary on `PATH` overrides `fuzzel` for the test process; reads stdin, writes a fixed line to stdout, exits 0. Tests the full pipe-and-read flow.
- [ ] `which fuzzel` failure → `EX_UNAVAILABLE` with a stderr message naming the binary.

### Phase 3.6 — Action subcommands

Group landings — most of these are 1–2 line wrappers around a single `Action` variant, plus a small bit of arg parsing. Group by shared scaffolding (the picker dance, the `<name>`-or-picker pattern).

- [ ] `switch-previous` / `toggle` (alias) — wraps `Action::SwitchActivityPrevious`.
- [ ] `move-window <activity>` and `move-window` (picker variant) — wraps `Action::MoveWindowToActivity`.
- [ ] `move-workspace <activity>` and `move-workspace` (picker variant) — wraps `Action::MoveWorkspaceToActivity`.
- [ ] `create <name>` — wraps `Action::CreateActivity`. Name collision → exit 73 (EX_CANTCREAT).
- [ ] `remove <name>` — wraps `Action::RemoveActivity`. Unknown name → exit 66; removing a config-declared activity surfaces the compositor's error verbatim.
- [ ] `assign-workspace` — implements the path chosen in 3.0.
- [ ] `save <name>` — non-IPC: edits user's `config.kdl` (appending `activity "name"`), then `Action::ReloadConfig`. Decide config-edit strategy: structured (`kdl` crate) vs. string-append heuristic. The structured path is safer (handles arbitrary existing config); the heuristic ships fast. Pick during this sub-phase based on `kdl` crate maturity.

### Phase 3.7 — Polish & v0.1.0

- [ ] README install/usage docs (currently a stub) — usage examples for every subcommand.
- [ ] Manual smoke test against a running niri (the `--ignored` test layer per §4.6) — author the e2e tests, document the manual run cadence.
- [ ] `cargo clippy --all --all-targets` clean against the baseline established in Phase 3.1.
- [ ] Tag `v0.1.0`.

---

## Appendix A: Source code map (one-liner per file)

Populated as files land. Initial state: `src/main.rs` is the stub from the bootstrap commit (`92e26ef`).

## Appendix B: Open questions parked for v2

- **Built-in gtk4-layer-shell picker** (Option A from compositor DD §8.3). Code-heavy; deferred until UX limits of fuzzel surface in real use.
- **Daemon mode** (event-stream-driven; required for live activity indicator in panels). Not justified for v1 — per-invocation IPC overhead is sub-millisecond.
- **D-Bus interface** (panel integration via D-Bus instead of CLI calls). Premature; no panel currently consumes it.
- **Save-to-config-on-exit** semantics beyond the explicit `save` subcommand (e.g., auto-save runtime activities at shutdown). Out of scope; the compositor already discards runtime activities at restart per design.

## Appendix C: Deferred Suggestions (review-surfaced parked items)

*(no entries yet)*
