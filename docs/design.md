# `niri-activities` design — Phase 3 of activities

Implementer-grade design for the user-facing CLI of KDE-style Activities on niri. Owns Phase 3 of the activities workstream; takes over from §8 of the compositor DD (`~/projects/desktop/de/niri/docs/activities/design.md`), which now redirects here.

This DD is the source of truth for everything user-facing: subcommand surface, error model, exit codes, IPC strategy, picker integration, output formats, test strategy. The compositor DD remains the source of truth for IPC types and compositor-side semantics.

---

## 1. Goal and scope

`niri-activities` is the CLI surface for Activities — a workspace-grouping abstraction landed compositor-side in Phases 1–2. Each invocation makes one or more sequential niri IPC calls, then exits. **No daemon, no persistent state, no event-stream subscription** in v1; those would be justified only by a UX bottleneck the per-invocation model can't meet (live activity indicator in a panel, pre-loaded picker state). Pickers (`fuzzel` for single-select, `rofi` for multi-select per §4.4) are external binaries spawned per invocation — same Unix-philosophy idiom as the rest of the niri ecosystem (anyrun, niri-ror, etc.); no Rust GUI code in this crate.

**Picker integration is PoC-quality in v1.** Both the fuzzel-for-single-select and rofi-for-multi-select integrations are first cuts focused on getting the binary functional end-to-end against the fork's IPC. The known UX seams (rofi pre-marker prefix workaround in §4.4, two-tier picker mental model, sentinel-row encoding for bulk actions) are documented but not polished; expect a v2 picker pass once real usage surfaces what actually limits the UX. This caveat applies to **every** picker-using subcommand (`switch`, `move-window`, `move-workspace`, `assign-workspace`), not just multi-select — fuzzel is "the one we have" rather than "the one we'd design."

Out of scope for v1, parked for v2: daemon mode, D-Bus interface, save-to-config-on-exit semantics beyond the explicit `save` subcommand, picker fallbacks beyond fuzzel/rofi (yad / zenity / built-in layer-shell parked unless distros lacking rofi 2.0+ become a real constraint), chezmoi-aware config editing (treated as out of scope same as every other niri-ecosystem app — see §3 `save` note).

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
- *Picker UX is PoC-quality in v1 (§1 caveat); fuzzel is "the one we have." A v2 redesign pass is parked in Appendix B.*

### `niri-activities switch-previous` (alias: `toggle`)
- IPC: `Action::SwitchActivityPrevious`.
- Success: silent, exit 0. Errors: none specific.

### `niri-activities move-window [<name>]`
- IPC: `Action::MoveWindowToActivity { name }`. Operates on focused window.
- Picker variant when no arg.
- **Fork-side variant TBD; Phase 3.6 prerequisite — see Appendix C.** `Action::MoveWindowToActivity` is not present at the niri-ipc rev pinned in Phase 3.1 (`54aee6582cbfc11b4e69fa8a602cf2653e29df4a`); either the compositor loop lands it before Phase 3.6, or Phase 3.6 absorbs the rev-bump.

### `niri-activities move-workspace [<name>]`
- IPC: `Action::MoveWorkspaceToActivity { name }`. Operates on focused workspace.
- Picker variant when no arg.

### `niri-activities assign-workspace`
- Spawns `rofi -dmenu -multi-select` (per §4.4) with the activity list, the focused workspace's current memberships pre-marked. The user toggles rows; bulk actions (`« Select all »` / `« Select none »` / `« Only one… »`) are sentinel rows. Confirm commits the diff via `Action::SetWorkspaceActivities`; cancel exits with no changes. Exit 0 either way.
- The `« Only one… »` sentinel chains a follow-up single-select rofi picker — the activity selected there becomes the sole membership. Cancellation of the chained picker exits 0 with no IPC dispatched. Implements the "unassign from all except this one" requirement.
- *Picker UX is PoC-quality in v1 (§1 caveat); the rofi pre-marker prefix workaround in §4.4 is the most visible seam.*

### `niri-activities create <name>`
- IPC: `Action::CreateActivity { name }`. Errors: name collision → `EX_CANTCREAT` (73).

### `niri-activities remove <name>`
- IPC: `Action::RemoveActivity { name }`. Errors: unknown name → `ActivityNotFound` (66); attempting to remove a config-declared activity surfaces the compositor's error verbatim.

### `niri-activities save <name>`
- Not an IPC call. Edits the user's `config.kdl` (appending `activity "name"`), then `Action::ReloadConfig`. Config-edit strategy decided in Phase 3.6.
- **Chezmoi (or any other dotfiles manager) is out of scope.** `save` writes to `$XDG_CONFIG_HOME/niri/config.kdl` directly, the same way every other niri-ecosystem app edits its own config. Users on chezmoi-managed setups must `chezmoi re-add` (or equivalent) after `save` to keep their dotfiles repo in sync — same coupling as for any other tool that edits niri/waybar/swaync configs. Detecting and integrating with chezmoi would add a runtime dep and a coupling we don't want; documented here as a known limitation rather than fixed.

### `niri-activities list [--json | --format=<spec>]`
- IPC: `Request::Activities` + `Request::Workspaces` + `Request::Windows` (three sequential calls per invocation; client-side join populates the per-activity workspace list and `window_count` documented in §4.5 — the wire-level `Activity` carries neither). Output format per §4.5.

## 4. Architectural decisions

Each subsection has a **Proposed:** lead sentence stating the agent recommendation. Phase 3.0's checkboxes ratify (or amend) these proposals before any implementation begins.

### 4.1 Error model and exit codes

**Proposed:** propagate via `anyhow::Error`; map to `sysexits.h`-style codes at the `main()` boundary through a typed inner error enum.

| Code | sysexits constant | Trigger |
|---|---|---|
| 0 | EX_OK | success, including silent picker cancellation |
| 64 | EX_USAGE | clap argument errors, unknown subcommand |
| 65 | EX_DATAERR | malformed IPC response: wrong `Response` variant (`MalformedResponseSource::WrongVariant`), wire JSON decode failure (`Decode`), or compositor returned `Reply::Err(String)` (`Server`) |
| 66 | EX_NOINPUT | activity name not found |
| 69 | EX_UNAVAILABLE | `$NIRI_SOCKET` unset; connect refused |
| 70 | EX_SOFTWARE | panic, programming-error path (a stub left unimplemented, etc.) |
| 73 | EX_CANTCREAT | create activity failed (name collision, config-edit failure) |

**Picker cancellation is exit 0**, not an error: fuzzel exiting non-zero with empty stdout means the user backed out, which is a normal outcome. Only IPC errors, type-mismatch errors, and explicit failures map to non-zero.

`anyhow::Error` carries chained context for stderr; the `main()` dispatcher does `match err.downcast_ref::<CliError>()` against a typed enum (`SocketUnavailable`, `ActivityNotFound`, `MalformedResponse`, `CantCreate`, ...) to pick the exit code. Fallback for un-typed errors is exit 1.

`MalformedResponse` carries a typed `MalformedResponseSource` with three variants — `Decode` (wire parse failure, holds the `serde_json::Error`), `Server` (compositor returned `Reply::Err(String)`), and `WrongVariant` (the wire parsed cleanly but the `Response` variant did not match the request that was sent). Splitting these typed-rather-than-stringified keeps the `serde_json::Error` chain reachable via `Error::source` for `{:#}` formatting, and lets stderr name the failure mode precisely (e.g. `expected Response::Activities, got Response::Workspaces(...)` for the `WrongVariant` case).

### 4.2 IPC strategy: fork's `niri-ipc` via git+rev

**Proposed:** depend on the fork's `niri-ipc` crate via a pinned git rev. Activities-specific IPC variants (`Request::Activities`, `Action::SwitchActivity`, etc.) only exist on the fork — crates.io's `niri-ipc` has none of them.

```toml
[dependencies]
niri-ipc = { git = "https://github.com/gajdusek/niri", rev = "<pin>" }
```

**niri-ipc dep stopgap (Phase 3.1):** the dep is currently `path = "../../niri/niri/gajdusek/niri-ipc"` (tracked rev: `54aee6582cbfc11b4e69fa8a602cf2653e29df4a`). The `feature/activities` branch is local-only and does not resolve via a git+rev URL. Switch to `git = "https://github.com/gajdusek/niri", rev = "<sha>"` once the branch is pushed. Pin rev rather than branch so IPC bumps are deliberate and reviewable.

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

### 4.4 Picker contracts (two-tier: fuzzel for single-select, rofi for multi-select)

**Proposed:** two external pickers, chosen per subcommand by the shape of the selection. Both spawned per invocation via `std::process::Command`; no Rust GUI code in this crate.

> **PoC-quality caveat (v1).** Both integrations are first cuts. The fuzzel calls are minimal stdin/stdout pipes; the rofi multi-select integration uses a `[x] `/`[ ] ` prefix workaround (§4.4 "Pre-check current memberships" below) because rofi mainline does not yet support multi-row pre-check natively (upstream tracking: PR #1809 against `davatorium/rofi`, open and unmerged as of 2026-04). A v2 picker overhaul — proper rofi pre-check once #1809 lands, *or* migration to a different launcher — is parked in Appendix B. Treat the v1 picker UX as functional, not polished.

#### Single-select subcommands → fuzzel

`switch`, `move-window`, `move-workspace` spawn `fuzzel --dmenu --prompt <subcommand-prompt>`, write items to stdin (newline-separated), read selected line from stdout. Cancellation (fuzzel exits non-zero with empty stdout) → exit 0 silently. fuzzel is the default niri-ecosystem picker (cf. anyrun, niri-ror examples).

| Subcommand (no arg) | Picker prompt | Items |
|---|---|---|
| `switch` | `Switch to activity:` | activity names, focused first |
| `move-window` | `Move window to activity:` | activity names |
| `move-workspace` | `Move workspace to activity:` | activity names |

#### Multi-select with bulk actions → rofi

`assign-workspace` is fundamentally a multi-select operation (per compositor DD §3.2 it edits the workspace's activity-membership set). fuzzel **does not and will not** support multi-select — issue [dnkl/fuzzel#244](https://codeberg.org/dnkl/fuzzel/issues/244) is labeled `wontfix`, no PRs, no roadmap. We do not wait on this.

**Resolution: spawn `rofi -dmenu -multi-select` for `assign-workspace`.** Mainline rofi 2.0.0 merged the `lbonn/rofi-wayland` fork; rofi is now Wayland-layer-shell-native and packaged in Debian testing/sid. Other Wayland launchers were canvassed (walker, tofi, wofi, anyrun) — none have multi-select. Custom gtk4-layer-shell popup was the previous proposal; rofi supersedes it and removes a module of Rust GTK code.

Invocation:

```sh
rofi -dmenu -multi-select \
     -p 'Activities — Ctrl+Space toggle, Enter save' \
     -ballot-selected-str '[x] ' \
     -ballot-unselected-str '[ ] '
```

The prompt explicitly names the multi-select keybind because rofi's default `Ctrl+Space` is not commonly known and a user pressing `Enter` on a row expecting to toggle would silently dismiss the picker with that single row as the selection.

Stdin: one activity name per line, plus three sentinel rows at the top (see "Bulk actions" below). Stdout: the selected lines, one per line. Cancellation (Escape, or rofi non-zero exit) → exit 0 silently, no IPC dispatched.

**Pre-check current memberships.** rofi mainline does not currently support multi-row pre-check in multi-select mode. Adjacent flags don't fit: `-selected-row` works only in single-select; `-a` marks rows as styled-active but doesn't include them in the saved selection on `Enter`. Upstream tracking is [PR #1809 (`davatorium/rofi`)](https://github.com/davatorium/rofi/pull/1809) "Pre-check boxes on multi-select", maintained by lbonn (the Wayland-fork merger) and open as of 2026-04, plus issue [#1806](https://github.com/davatorium/rofi/issues/1806) for the original request. We do not block on it.

The v1 workaround: render activity names with a `[x] ` prefix already baked into the input line for activities the workspace is currently in, and `[ ] ` for ones it is not. The user toggles via rofi's normal multi-select keybind (`Ctrl+Space`); `Enter` confirms; we parse the returned lines and strip the prefix.

**Save semantics — full replacement, not diff.** On `Enter`, the saved membership set is the literal set of rows rofi returns as selected — we do **not** XOR against the starting state. This makes the picker behave like "edit the membership set; what you check is what you save", which matches the shell-multi-select intuition and avoids a confusing two-state mental model (where the prefix would mean "starting state" and the ballot would mean "pending change," with the diff being the saved value). Consequence: the user must re-check rows that are currently in-set if they want to keep them. The `[x] ` prefix is informational — it tells the user "this row is currently in-set, so re-check it to preserve" — not load-bearing for save semantics.

This still leaves a visual seam (rofi's own ballot character via `-ballot-selected-str` looks similar to our prefix) but the parsing rule is unambiguous: returned rows = saved set. Documented as a UX nit replaceable in v2 once #1809 lands or a different launcher is chosen.

**Bulk actions.** Three sentinel rows at the top of the input list:

- `« Select all »` — when present in the returned selection, the saved state becomes "every activity"; any other selected rows are ignored.
- `« Select none »` — when present in the returned selection, the saved state becomes "no activities"; any other selected rows are ignored. Reversible inside the same `assign-workspace` flow (the user re-runs and re-checks); **no confirmation prompt**.
- `« Only one… »` — when present in the returned selection, the picker exits and we **chain a follow-up single-select rofi** (no `-multi-select` flag) showing the activity list with no sentinel rows. The activity picked there becomes the sole membership. Cancellation of the chained picker → exit 0 silently, no IPC dispatched. This implements the "unassign from all except this one" requirement without the `Only Foo` per-row sentinel-doubling that would clutter the list.

**Sentinel precedence** when multiple sentinels appear in the returned selection (user toggled more than one before pressing Enter):

1. `« Select none »` wins — safest destructive default.
2. `« Only one… »` — opens chained picker; literal selection ignored.
3. `« Select all »` — literal selection ignored.
4. Literal selection — neither sentinel returned.

**Magic-item naming.** The `« … »` Unicode brackets (U+00AB, U+00BB) are unlikely to collide with niri activity names by convention, but the compositor permits arbitrary strings. Disambiguation: at picker-open time, validate that no real activity name equals one of our sentinel strings; if collision detected, fall back to a less collision-prone name (`__niri_activities_select_all__`, `__niri_activities_select_none__`, `__niri_activities_only_one__`) and log a debug note. This is a corner-case guard, not the main path.

#### Why rofi over the alternatives canvassed

| Candidate | Verdict | Why |
|---|---|---|
| **fuzzel** | out | dnkl/fuzzel#244 wontfix; no multi-select on roadmap |
| **rofi 2.0+** | **chosen** | native multi-select + filter + Wayland layer-shell + Debian-packaged + parseable stdout |
| walker / tofi / wofi / anyrun | out | no multi-select; anyrun#78 unimplemented |
| zenity `--list --checklist` | out | GTK checkbox list works on Wayland but no live filter / search input |
| yad `--list --checklist --search-column=N` | viable plan B | GTK3 + Wayland + filter; documented as a v2 fallback if rofi unavailable on a target distro |
| gum `choose --no-limit` | out | TUI-only; would require spawning a terminal window |
| custom gtk4-layer-shell popup | out | superseded — rofi covers the requirement with zero new Rust GUI code |

**Runtime dep contract.** rofi is required at runtime for `assign-workspace`, the same way fuzzel is required for the single-select subcommands. Detection: at the top of the `assign-workspace` flow, `which rofi` (or equivalent) → if missing, exit 69 (`EX_UNAVAILABLE`) with a stderr message naming the binary and pointing at the README's install section. README documents both `fuzzel` and `rofi` as runtime deps.

### 4.5 `list` output format

**Proposed:** three modes, controlled by mutually exclusive flags.

**Default (no flag) — human-readable plain text.** One line per activity; the focused activity is prefixed `*`, others ` `:

```
* Work       (config) [3 workspaces, 12 windows]
  Personal   (config) [2 workspaces, 5 windows]
  Gaming     (runtime) [1 workspace, 0 windows]
```

Column widths are computed from the longest name + a 2-space gutter. Truncation rule: never truncate; if a name is wider than the terminal, the line wraps via the terminal's wrap behavior (no manual truncation).

Counts pluralise on `n != 1`: `1 workspace` / `0 workspaces` / `12 workspaces`; same rule for `window` / `windows`. Zero activities → empty stdout (no header line, no trailing newline).

**`--json` — machine-readable JSON.** Schema is wrapped in a top-level object carrying a `schema_version` integer so consumers can branch on shape changes without parsing failures. Bumping `schema_version` is a breaking change; additive fields (new optional keys) keep the version constant.

```json
{
  "schema_version": 1,
  "activities": [
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
}
```

When the activities list is empty, `--json` emits `{"schema_version": 1, "activities": []}` (the envelope is never omitted; consumers parse one shape unconditionally). The empty-stdout zero-case applies only to the default plain output.

Consumers that don't care about versioning can read `.activities[]` directly via `jq`:

```sh
niri-activities list --json | jq -r '.activities[].name'
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

**Subcommand integration tests via `MockClient`** (per §4.3) plus `assert_cmd` for the binary boundary. (`assert_cmd` introduced in Phase 3.1 as a dev-dep alongside the binary skeleton.) Each subcommand has at minimum a golden-path test, an error-path test, and (where applicable) a picker-cancellation test. `MockClient` is wired into the binary via a thread-local override landed in Phase 3.2: `make_client()` is a `#[cfg(test)]`-aware factory — in test builds it consults a thread-local populated by `install_mock` (returning an RAII guard); in production builds the thread-local is not compiled in and `make_client()` returns a fresh `SocketClient` directly. Env-var injection was rejected because it would leak test infrastructure into production codepaths. **Asymmetry for Phase 3.3+:** `assert_cmd`-launched smoke tests cross a process boundary and cannot use the thread-local; in-process unit tests in each subcommand's `tests` module use it.

**End-to-end smoke test against a real niri**, gated `#[ignore]`. Manual run (`cargo test -- --ignored`); not part of `cargo test` default. Asserts side effects (post-action workspace state via `niri msg`) rather than process exit codes.

`cargo test --all` runs unit + MockClient integration; `cargo test -- --ignored` adds the smoke layer.

---

## 5. Phases

### Phase 3.0 — Design ratification (no code)

Each box is a human-gated decision. The architect refuses to plan Phase 3.1+ until every box is `[x]` or amended. **Proposed:** entries are agent recommendations; the human ratifies or amends in-place before the loop drives implementation.

- [x] Error model & exit codes — see §4.1. **Proposed:** anyhow + sysexits.h mapping per the table.
- [x] IPC strategy — see §4.2. **Proposed:** fork's `niri-ipc` via git+rev (pin TBD in 3.1).
- [x] IPC client trait — see §4.3. **Proposed:** `NiriClient` trait, `SocketClient` + `MockClient` impls.
- [x] Picker contracts — see §4.4. **Proposed:** two-tier external pickers — fuzzel `--dmenu` for single-select (`switch` / `move-*`); `rofi -dmenu -multi-select` for `assign-workspace` with `« Select all »` / `« Select none »` / `« Only one… »` sentinel rows for bulk actions; `« Only one… »` chains a follow-up single-select rofi for the "unassign from all except this one" path. Save semantics are full-replacement (returned rows = saved set), not diff. Rofi prompt names the multi-select keybind (`Ctrl+Space`) for discoverability. Both runtime deps documented in the README; no Rust GUI code in v1. **PoC-quality** — see §1 caveat and §4.4 PoC note; v2 picker overhaul (track rofi PR #1809 for proper pre-check, evaluate alternative launchers) parked in Appendix B.
- [x] `list` output format — see §4.5. **Proposed:** default plain / `--json` / `--format=<spec>`.
- [x] Test strategy — see §4.6. **Proposed:** unit + MockClient/assert_cmd integration + ignored e2e smoke.

### Phase 3.1 — Skeleton & error machinery

- [x] Add Cargo deps ratified in 3.0: `clap` (derive), `niri-ipc` (git+rev — pin to fork HEAD at the time the box is landed and record the rev in the commit message), `serde`, `serde_json`, `anyhow`. **Lock the rev to the actual fork HEAD; do not invent a rev.** Landed as `413b49d` (`cargo: add clap, niri-ipc, serde, anyhow deps`).
- [x] `clap`-based subcommand dispatch matching §3. Each subcommand stub prints `not implemented` to stderr and exits 70 (EX_SOFTWARE). Top-level binary still produces useful `--help`. Landed as `e7d6743` (`cli: skeleton subcommand dispatch + typed error enum`).
- [x] `CliError` enum (typed: `SocketUnavailable`, `ActivityNotFound`, `MalformedResponse`, `CantCreate`, `Usage`) with rustdoc on each variant naming the trigger condition. `main()` dispatcher maps via `downcast_ref` to exit codes per §4.1. Landed as `e7d6743`.
- [x] `--version` prints `env!("CARGO_PKG_VERSION")`. Landed as `e7d6743`.
- [x] Unit tests for the error → exit code mapping (one test per code). Pinned at 0 warnings as of `e7d6743`.

**Reviewed:** 2026-05-14 (`413b49d`, `e7d6743`). Phase 3.1 — `413b49d` adds the dependency surface (clap, niri-ipc via path stopgap, serde, serde_json, anyhow, assert_cmd + predicates as dev-deps); `e7d6743` lands the skeleton: nine-subcommand clap dispatch, six-variant `CliError` typed enum with `map_exit`, manual clap-error routing for help/version, and four `assert_cmd` integration smoke tests. Reviewed across four review aspects (general code quality, silent-failure surface, comment accuracy, test coverage) via two `/pr-review-toolkit:review-pr` passes (initial + targeted re-review of fixer's amendments); re-review was clean. **Finding worth surfacing (niri-ipc path stopgap):** `413b49d` uses `path = "../../niri/niri/gajdusek/niri-ipc"` rather than the spec's `git+rev` because `feature/activities` is local-only and a git URL would not resolve. The tracked rev (`54aee6582cbfc11b4e69fa8a602cf2653e29df4a`) is recorded in the commit body; the migration trigger is "once the branch is pushed." See §4.2 stopgap note. **Finding worth surfacing (`NotImplemented` as the sixth variant):** the DD's §4.1 table has five exit codes for the non-stub path; `e7d6743` adds `NotImplemented` (exit 70, `EX_SOFTWARE`) as the sixth variant powering the stub arms. This variant is expected to disappear as sub-phases land real implementations; tracking it here so Phase 3.6's cleanup reviewer knows to look for lingering stubs. **Finding worth surfacing (fuzzel-cancellation exit 0 contract):** §4.1's "picker cancellation is exit 0" rule has no variant in the Phase 3.1 enum — the non-IPC cancellation path will be represented as `Ok(())` return from the picker fn (no `CliError` involved). Confirmed at review; no follow-up needed for Phase 3.1, but Phase 3.5 must not introduce a `PickerCancelled` variant that would exit non-zero. Post-review fixes squashed into `e7d6743`: three escalations triaged to Appendix C (typed source carriers on `SocketUnavailable`/`MalformedResponse`, struct variants for all five string-shaped variants, `map_exit` chain-walk doc trim); `toggle` alias routing confirmed clean (`toggle_alias_routes_to_switch_previous` test). DD amended in this commit: §3 `move-window` IPC-gap note; §4.2 stopgap paragraph; §4.6 `assert_cmd` introduction parenthetical; Phase 3.1 boxes flipped to `[x]` with landing notes; Box 5 wording updated to "Pinned at 0 warnings as of `e7d6743`"; Appendix A extended with Phase 3.1 files; Appendix C opened with four entries (fork-side `MoveWindowToActivity`, typed source carriers, struct variants, chain-walk doc trim). Same 16 tests green (11 before Phase 3.1, delta +5: `socket_unavailable_survives_context_wrap`, `cli_error_survives_context_wrap_in_alternate_format`, `toggle_alias_routes_to_switch_previous`, `no_args_exits_64`, `list_json_and_format_conflict_exits_64`); `cargo clippy --all --all-targets` 0 warnings — Phase 3.1 commits pin the project clippy baseline at zero. Proceed to Phase 3.2 (IPC adapter) with the reviewed base.

### Phase 3.2 — IPC adapter

- [x] `NiriClient` trait + `SocketClient` impl per §4.3. `SocketClient::send` connects to `$NIRI_SOCKET`, sends one `Request`, awaits one `Response`. Connection per call (no persistent state). Landed as `38c2f18`.
- [x] `MockClient` impl with a `VecDeque<(Request, Response)>` queue. `expect(req, reply)` enqueues; `send` panics on unexpected request to surface test gaps; `assert_consumed_in_order()` for end-of-test invariant. Landed as `38c2f18`.
- [x] Map niri-ipc transport errors: connect-refused / `$NIRI_SOCKET` unset → `SocketUnavailable`; reply-shape mismatch → `MalformedResponse`. Concrete error-variant table in the rustdoc on `IpcError`. Landed as `38c2f18`.
- [x] Wiring strategy for tests (the §4.6 open detail): thread-local injection via `make_client()` consulting a `#[cfg(test)]` thread-local. Env-var rejected. Landed as `38c2f18`.
- [x] Unit tests: `SocketClient` against a temp Unix socket fixture (std-thread `UnixListener` accepts one connection, replies with a fixed `Response`). `MockClient` panic-on-unexpected coverage. Landed as `38c2f18`.

**Reviewed:** 2026-05-14 (`38c2f18`, was `61145f2` → `c4bc5f5` → `38c2f18` across two fixer amends). Phase 3.2 — all five IPC-adapter boxes in one commit: `NiriClient` trait, `SocketClient` open-coded round-trip, `MockClient` with `Drop`-enforced queue-consumption check, thread-local test-injection, and the `IpcError` → `CliError` mapping with three new unit tests. Reviewed across five review aspects (general code quality, silent-failure surface, comment accuracy, test coverage, type design). **Finding worth surfacing (`SocketClient` open-coded round-trip):** `SocketClient::send` does not delegate to `niri_ipc::socket::Socket::send`. The upstream helper's `?` on `serde_json::from_str` converts the decode error into `io::Error`, which would collapse a malformed reply into `IpcError::Transport` (exit 69) and erase `IpcError::Decode` (exit 65). The rationale is captured in the module rustdoc and the commit body; a future refactor that restores delegation would re-collapse the 65-vs-69 distinction. **Finding worth surfacing (`Drop` guard for `MockClient`):** `MockClient` implements `Drop` with a `!std::thread::panicking()` gate; if the instance is dropped with unconsumed expectations it panics, naming the remaining request shapes. The original spec called `assert_consumed_in_order()` as an explicit end-of-test call; the `Drop` guard is an additional forward-looking defense against Phase 3.3+ tests that forget the explicit assertion. Post-review fixes squashed into `38c2f18`: typed source carriers landed — `SocketUnavailable(io::Error)` and `MalformedResponse(MalformedResponseSource { Decode(serde_json::Error), Server(String) })` replacing the Phase 3.1 `String` shapes (Appendix C entry partially resolved — see below); four `error.rs` tests amended to construct the typed shapes; env-var injection rejected and thread-local chosen with RAII guard pattern; `Mutex<()>` serialization added for env-mutating tests under Rust 2024 `unsafe { set_var }`. DD also amended in this commit: §4.1 row 65 trigger language expanded; §4.6 thread-local injection mechanism documented with Phase 3.3+ boundary asymmetry noted; Appendix A extended with `src/ipc.rs`; Appendix C typed-source-carriers entry marked partially landed. Resolves Appendix C entry: "Typed source carriers on `CliError` variants" — `SocketUnavailable(io::Error)` and `MalformedResponse(MalformedResponseSource)` landed in this commit; the "struct variants" sibling entry remains open. Same 32 tests green (was 16; delta +16: 12 new `ipc.rs` tests + 3 updated `error.rs` tests + 1 existing integration test `list_json_and_format_conflict_exits_64` that had already landed in Phase 3.1's `tests/cli.rs`; counted as 31 in an earlier tally — corrected here); `cargo clippy --all --all-targets` clean. Proceed to Phase 3.3 (`list` subcommand) with the reviewed base.

### Phase 3.3 — `list` subcommand

- [x] Plain output per §4.5. Edge cases: zero activities → empty stdout; long names → no truncation. Landed as `8fb13cf`.
- [x] `--json` output per §4.5; matches the documented schema exactly. Landed as `8fb13cf`.
- [x] `--format=<spec>` per §4.5; unknown field → `EX_USAGE`. Landed as `8fb13cf`.
- [x] `--json` and `--format=` mutually exclusive — regression test pins the clap-level `conflicts_with` rule (wiring already landed in the skeleton commit; this box adds the pinning test). Landed as `8fb13cf`.
- [x] Integration tests via `MockClient` + `assert_cmd`: golden plain output (3 activities, focused middle), golden JSON, three `--format=` variants, zero-activities plain, zero-activities JSON. Landed as `8fb13cf`.

**Reviewed:** 2026-05-14 (`8fb13cf`, was `da3cd9e` → `a955749` → `8fb13cf` across two fixer amends). Phase 3.3 — all five `list`-subcommand boxes in one squashed commit: three render paths (plain, `--json`, `--format=<spec>`), client-side join of Activities × Workspaces × Windows in fixed IPC order, five `MockClient`-driven in-process tests, and eight `assert_cmd` integration tests crossing the process boundary. Reviewed across five review aspects (general code quality, silent-failure surface, comment accuracy, test coverage, type design). **Finding worth surfacing (`CliError::OutputPipeClosed` as typed BrokenPipe sentinel):** cycle-1 originally landed stdout-write `BrokenPipe` as an `err.chain().any(io::ErrorKind::BrokenPipe)` predicate in `main.rs`. This was a silent-failure regression: the predicate would have matched `CliError::SocketUnavailable(io_err)` when the inner `io::Error` happened to be `BrokenPipe` (e.g., niri compositor crashing mid-write), suppressing the error with exit 0 instead of exit 69. Cycle-2 introduced `CliError::OutputPipeClosed` as a typed sentinel emitted only at write sites in `src/list.rs`; `main.rs` checks for this variant specifically before falling through to the general `map_exit` path. The `root_cause()` vs. `chain().any()` asymmetry between `classify_write_err` and `is_stdout_pipe_closed` is noted in Appendix C as a defensive maintenance item. **Finding worth surfacing (`MalformedResponseSource::WrongVariant` — first production use):** Phase 3.3 is the first sub-phase to construct a `WrongVariant` value in production code (`src/list.rs` raises it on each of the three IPC responses when the returned `Response` variant does not match the expected one). The `variant_name` helper called from `WrongVariant`'s context string returns `"Response::<unknown>"` for any unrecognised variant beyond the enumerated 16; the forward-compat gap is parked in Appendix C. **Finding worth surfacing (`dead_code` debt cleared):** Phase 3.3 is the first production consumer of the IPC layer; `src/ipc.rs`'s `NiriClient` trait, `IpcError` enum, and `make_client` factory no longer carry `#[allow(dead_code)]` markers. Post-review fixes squashed into `8fb13cf`: cycle-1 — 7 tests added covering `WrongVariant` on `Workspaces`/`Windows` arms, `run()` dispatch routing, zero-case `--format=` output, `WorkspaceCount`/`WindowCount` fields, duplicate-field validation; cycle-2 — ~8 further tests covering `BrokenPipe` predicate coverage, `MockClient::remaining_count` self-test, `variant_name` assertion strengthening on three existing tests, whitespace strict-contract pin, `Activities WrongVariant` context-layer assertion; `CliError::OutputPipeClosed` introduced replacing the `chain().any()` predicate; `MockClient::remaining_count()` test-only accessor added to `src/ipc.rs`. DD amended in this commit: Phase 3.2 Reviewed: test count corrected (31 → 32); Phase 3.3 boxes flipped to `[x]` with landing notes; Appendix A extended with `src/list.rs` and `src/error.rs`/`src/main.rs`/`src/ipc.rs` deltas; Appendix C extended with five new entries. Same 62 tests green (54 unit + 8 integration; was 32; delta +30 — ~+7 from cycle-1, ~+8 from cycle-2, remainder from the initial implementation); `cargo clippy --all --all-targets` 0 warnings (baseline unchanged). Proceed to Phase 3.4 (`switch <name>`) with the reviewed base.

### Phase 3.4 — `switch <name>` subcommand (no picker yet)

- [ ] Dispatch IPC `Action::SwitchActivity { name }`.
- [ ] Unknown name → `ActivityNotFound` → exit 66 (the compositor returns a structured error; map it).
- [ ] Already-active name → no-op silently, exit 0 (verify against compositor DD §5.3 — switching to the active activity is a documented no-op).
- [ ] Integration tests: golden, unknown name, already-active.

### Phase 3.5 — fuzzel picker (single-select)

- [ ] Spawn fuzzel via `std::process::Command`, pipe items to stdin (one activity per line), read stdout selection.
- [ ] Cancellation (non-zero exit + empty stdout) → exit 0 silently.
- [ ] `niri-activities switch` (no arg) opens picker, then dispatches §3.4 path with the chosen name.
- [ ] Integration test: shim binary on `PATH` overrides `fuzzel` for the test process; reads stdin, writes a fixed line to stdout, exits 0. Tests the full pipe-and-read flow.
- [ ] `which fuzzel` failure → `EX_UNAVAILABLE` with a stderr message naming the binary.

### Phase 3.5b — `assign-workspace` multi-select picker (rofi)

Lands the `assign-workspace` UI per §4.4. Distinct from Phase 3.5 (fuzzel) because the picker tool, semantics (multi-select), and selection-parsing logic differ; bundling would conflate two review surfaces. Smaller than the prior gtk4-layer-shell variant of this phase — no Rust GUI code.

- [ ] `src/picker/multi_select.rs` — `which rofi` detection (→ `EX_UNAVAILABLE` if missing), spawn `rofi -dmenu -multi-select -p 'Activities — Ctrl+Space toggle, Enter save' -ballot-selected-str '[x] ' -ballot-unselected-str '[ ] '`, write activity list (with `« Select all »` / `« Select none »` / `« Only one… »` sentinel rows + `[x] `/`[ ] ` pre-marker prefixes) to stdin, read newline-separated selection from stdout.
- [ ] Selection parsing: strip the `[x] `/`[ ] ` prefix from each returned line; recognize sentinel rows; collision-guard sentinel names against real activity names at picker-open (rename to `__niri_activities_*__` if collision, log debug).
- [ ] Sentinel precedence (per §4.4): `« Select none »` > `« Only one… »` > `« Select all »` > literal selection.
- [ ] **Save semantics: full replacement, not diff.** Returned rows = saved membership set. Dispatch `Action::SetWorkspaceActivities { workspace, activities }` unconditionally (the compositor handles the no-change case as a no-op).
- [ ] `« Only one… »` chained picker: spawn a follow-up `rofi -dmenu -p 'Only this activity:'` (no `-multi-select`, no sentinels) with the activity list; the picked activity becomes the sole membership. Cancellation of the chained picker → exit 0 silently, no IPC.
- [ ] Wire to `assign-workspace` subcommand: query current memberships → build pre-marked input → present picker → resolve sentinels per precedence → dispatch IPC.
- [ ] Cancellation of the primary picker (rofi non-zero exit, empty stdout) → exit 0 silently, no IPC.
- [ ] Tests: integration test via shim binary on `PATH` overriding `rofi` for the test process (same pattern as Phase 3.5's fuzzel shim — extract shared shim infrastructure if natural) — reads stdin, writes a fixed selection, exits 0. Cover: literal selection, each sentinel singly, sentinel precedence on multi-sentinel returns, `« Only one… »` chained-picker happy path, chained-picker cancellation. Unit tests for the prefix-parsing and sentinel-precedence pure functions.
- [ ] README updated: rofi listed as a runtime dep alongside fuzzel; install hint for Debian (`apt install rofi`); note that rofi 2.0+ is required for native Wayland-layer-shell (mainline merge of `lbonn/rofi-wayland`); document the PoC-quality caveat for the picker UX (§1, §4.4) so users know the rough edges are known.
- [ ] Stretch: handle stale snapshot — an external `niri-activities create` while the picker is open. v1 takes a snapshot at picker-open; if stale at save, the IPC error surfaces normally per §4.1. Document; defer reactive refresh to v2 (Appendix B).

### Phase 3.6 — Action subcommands

Group landings — most of these are 1–2 line wrappers around a single `Action` variant, plus a small bit of arg parsing. Group by shared scaffolding (the picker dance, the `<name>`-or-picker pattern). `assign-workspace` is **not** in this list — it landed in Phase 3.5b alongside its bespoke picker.

- [ ] `switch-previous` / `toggle` (alias) — wraps `Action::SwitchActivityPrevious`.
- [ ] `move-window <activity>` and `move-window` (picker variant) — wraps `Action::MoveWindowToActivity`.
- [ ] `move-workspace <activity>` and `move-workspace` (picker variant) — wraps `Action::MoveWorkspaceToActivity`.
- [ ] `create <name>` — wraps `Action::CreateActivity`. Name collision → exit 73 (EX_CANTCREAT).
- [ ] `remove <name>` — wraps `Action::RemoveActivity`. Unknown name → exit 66; removing a config-declared activity surfaces the compositor's error verbatim.
- [ ] `save <name>` — non-IPC: edits user's `config.kdl` (appending `activity "name"`), then `Action::ReloadConfig`. Decide config-edit strategy: structured (`kdl` crate) vs. string-append heuristic. The structured path is safer (handles arbitrary existing config); the heuristic ships fast. Pick during this sub-phase based on `kdl` crate maturity.

### Phase 3.7 — Polish & v0.1.0

- [ ] README install/usage docs (currently a stub) — usage examples for every subcommand.
- [ ] Manual smoke test against a running niri (the `--ignored` test layer per §4.6) — author the e2e tests, document the manual run cadence.
- [ ] `cargo clippy --all --all-targets` clean against the baseline established in Phase 3.1.
- [ ] Tag `v0.1.0`.

---

## Appendix A: Source code map (one-liner per file)

Populated as files land. Initial state: `src/main.rs` is the stub from the bootstrap commit (`92e26ef`).

**After Phase 3.1 (`413b49d`, `e7d6743`):**
- `src/main.rs` — entry point; clap `try_parse()` + manual clap-error routing (help/version → exit 0, parse errors → exit 64) + `map_exit` dispatch; prints full anyhow chain via `{:#}`.
- `src/cli.rs` — clap-derive `Cli` + `Cmd` enum with all nine subcommands; `dispatch()` routes to per-subcommand stub fns returning `CliError::NotImplemented` (exit 70).
- `src/error.rs` — `CliError` six-variant typed enum (Usage 64, MalformedResponse 65, ActivityNotFound 66, SocketUnavailable 69, NotImplemented 70, CantCreate 73) with rustdoc trigger conditions; `map_exit()` downcasts `anyhow::Error` via `downcast_ref`; un-typed errors fall through to exit 1.
- `tests/cli.rs` — `assert_cmd` integration smoke tests pinning the CLI surface: `--version`, `--help`, unknown-subcommand → exit 64, switch stub → exit 70.

**After Phase 3.2 (`38c2f18`):**
- `src/ipc.rs` — IPC trait `NiriClient`, `SocketClient` (one-shot `$NIRI_SOCKET` round-trip, open-coded to preserve `Decode`-vs-`Transport` distinction), `MockClient` (test fixture with `Drop`-enforced queue-consumption check), `IpcError` three-variant enum with rustdoc table, `IpcError` → `CliError` mapping via `From`; thread-local `make_client()` factory with `install_mock` RAII guard.

**After Phase 3.3 (`8fb13cf`):**
- `src/list.rs` — subcommand body for `list`; three render paths (plain default, `--json` envelope, `--format=<spec>` CSV); client-side join of Activities × Workspaces × Windows in fixed IPC order; first production constructor for `MalformedResponseSource::WrongVariant`.
- `src/error.rs` — gains `CliError::OutputPipeClosed` (typed sentinel for stdout-write BrokenPipe; emitted at `list` write sites, suppressed to exit 0 by `main.rs` before `map_exit`).
- `src/main.rs` — gains `is_stdout_pipe_closed` predicate + `OutputPipeClosed` → `ExitCode::SUCCESS` short-circuit ahead of `map_exit`; `list` arm in `dispatch()` now routes to `src/list.rs::run()`.
- `src/ipc.rs` — `#[allow(dead_code)]` markers removed from `NiriClient`, `IpcError`, and `make_client` (now consumed by production `list` call sites); test-only `MockClient::remaining_count()` accessor added.

## Appendix B: Open questions parked for v2

### Picker UX overhaul (whole subsection — track this together)

The v1 picker integration is PoC-quality (§1 caveat). Once the binary is functional end-to-end, evaluate whether the seams below collectively warrant a redesign or whether targeted fixes suffice. Decision deferred to "after the binary actually runs" per the design author's call.

- **rofi multi-row pre-check (upstream tracking).** [PR #1809 — Pre-check boxes on multi-select](https://github.com/davatorium/rofi/pull/1809) by KSXGitHub against `davatorium/rofi`, references issue [#1806](https://github.com/davatorium/rofi/issues/1806). Maintained by lbonn (Wayland-fork merger); open and unmerged as of 2026-04 with active history (force-pushed November 2024). **Watch this PR.** If it merges, the v1 `[x] `/`[ ] ` prefix workaround in §4.4 can be retired in favor of native pre-check, and the "PoC-quality" caveat shrinks meaningfully. If it stalls indefinitely, escalate by either patching `rofi` locally for development, switching launchers, or accepting the workaround.
- **Picker tool re-evaluation.** rofi may turn out to be the wrong fit for reasons not yet visible. Candidates already canvassed in §4.4 ("Why rofi over the alternatives canvassed" table): yad `--list --checklist` (Plan B if rofi unavailable on target distros — GTK3 + Wayland + filter), zenity (no live filter), TUI options (gum — terminal subprocess overhead), custom gtk4-layer-shell popup (rejected for Rust GUI dep weight). Re-litigate if real usage surfaces blocker-class issues with rofi.
- **Generalized single-select replacement for fuzzel** on `switch` / `move-window` / `move-workspace`. The two-tier split (fuzzel vs. rofi) is by selection cardinality. If users find fuzzel limiting (previews, richer rendering, activity icons), `rofi -dmenu` (single-select mode) is a near-drop-in replacement. Behind real UX evidence, not speculative.
- **Reactive refresh of the multi-select picker** on external activity changes during a picker invocation. v1 takes a snapshot at picker-open; stale-at-save surfaces as a normal IPC error per §4.1. v2 could subscribe to the activities event stream and live-update; requires either daemon mode or a sidecar process feeding rofi via its dynamic-update protocol.
- **Two-prefix visual seam in `assign-workspace`.** §4.4's full-replacement save semantics resolve the *parsing* ambiguity (returned rows = saved set), but the on-screen overlap of our `[x] ` prefix with rofi's own ballot character remains a UX nit. Drops out automatically when rofi PR #1809 lands; otherwise revisit if users complain.

### Other v2 parking lot

- **Daemon mode** (event-stream-driven; required for live activity indicator in panels). Not justified for v1 — per-invocation IPC overhead is sub-millisecond.
- **D-Bus interface** (panel integration via D-Bus instead of CLI calls). Premature; no panel currently consumes it.
- **Save-to-config-on-exit** semantics beyond the explicit `save` subcommand (e.g., auto-save runtime activities at shutdown). Out of scope; the compositor already discards runtime activities at restart per design.
- **Chezmoi / dotfiles-manager integration for `save`.** v1 writes `$XDG_CONFIG_HOME/niri/config.kdl` directly (§3 `save` note). Out of scope for v1 — same as every other niri-ecosystem app that edits its own config. Re-evaluate only if the user-base on dotfiles-managers is large enough that manual `chezmoi re-add` becomes a real friction.

## Appendix C: Deferred Suggestions (review-surfaced parked items)

- **Fork-side `Action::MoveWindowToActivity`** — required by the `move-window` subcommand; not present at the niri-ipc rev pinned in Phase 3.1 (`54aee6582cbfc11b4e69fa8a602cf2653e29df4a`); either a compositor-loop sub-phase lands it before Phase 3.6, or Phase 3.6 absorbs the rev-bump. From review of `e7d6743` (2026-05-14). IPC variant was out of scope for Phase 3.1 (skeleton-only commit); recorded here so Phase 3.6 planning has a concrete prerequisite to resolve.

- **Typed source carriers on `CliError` variants** — Convert `SocketUnavailable(String)` → `SocketUnavailable(io::Error)` and `MalformedResponse(String)` → `MalformedResponse(serde_json::Error)`. Rationale: encodes the error producer in the type so a future call site cannot accidentally route the wrong error class through the wrong variant. Cost: every call site that constructs these variants in Phase 3.2+ must take the typed source rather than stringify. From review of `e7d6743` (2026-05-14). Fixer escalated rather than folded because the shape change propagates through every future call site — architect decision. Consider as Phase 3.1.1 polish before Phase 3.2 wires the first IPC call, or accept the stringly-typed contract permanently. **Partially landed: 2026-05-14 (`38c2f18`).** `SocketUnavailable(io::Error)` and `MalformedResponse(MalformedResponseSource { Decode(serde_json::Error), Server(String) })` landed in Phase 3.2; `MalformedResponseSource` is a two-variant enum rather than the originally proposed bare `serde_json::Error`, to distinguish compositor-side `Err(String)` from wire-decode failure. The "struct variants for all five `CliError` variants" sibling entry remains open.

- **Struct variants for `CliError`** — Convert `String`-shaped variants (`Usage`, `MalformedResponse`, `ActivityNotFound`, `SocketUnavailable`, `CantCreate`) to struct variants with named fields (e.g., `Usage { message: String }`). Rationale: all five variants are type-interchangeable today, so a copy-paste error producing `CliError::Usage("connect refused: ...")` compiles cleanly and exits 64 instead of 69; named-field syntax makes the wrong choice visible in review. Cost: every construction site updates. From review of `e7d6743` (2026-05-14). Bundle with the typed-source-carriers entry above if both are accepted.

- **`map_exit` chain-walk doc trim** — The `socket_unavailable_survives_context_wrap` test (in `src/error.rs::tests`) and the `map_exit` rustdoc both reference "chain walk" as the pinned contract. anyhow's `downcast_ref` already walks the chain, so the test pins the observable contract (`CliError` survives `.context()` wrapping → correct typed exit code), not the iteration strategy. Trim "chain-walk" wording to "context-wrap survival" the next time these comments are touched. From review of `e7d6743` (2026-05-14). Non-blocking — the contract pinned is correct, only the description of the mechanism is imprecise.

- **`variant_name` forward-compat for unknown `Response` variants** — `variant_name` returns `"Response::<unknown>"` for any `Response` variant outside the currently enumerated 16, dropping the variant identity entirely. Extracting the actual variant name from `format!("{r:?}")` would require changing the return type from `&'static str` to `Cow<'static, str>`, with cascading impact on `wrong_variant` and its callers. From review of `8fb13cf` (2026-05-14). Defer to a future sub-phase or to whenever the next non-mechanical touch lands on this code.

- **`serde_json::to_string(&req).expect(...)` in `src/ipc.rs`** — pre-existing pattern (predates Phase 3.3), but Phase 3.3 made it load-bearing as the first production consumer of the IPC layer. The justification "Request serialization is infallible" is wrong in principle — a future upstream `niri-ipc` change introducing a fallible `Serialize` path would turn this into a production panic. From review of `8fb13cf` (2026-05-14). Convert to `?` + new `IpcError::EncodeRequest(serde_json::Error)` variant when the next IPC-layer touch happens.

- **CSV escaping for activity names containing commas** — `--format=<spec>` output silently corrupts CSV when activity names contain commas. Documented in code as out-of-scope for v1; user contract is "activity names don't contain commas." From review of `8fb13cf` (2026-05-14). If the v1 stance changes, implement RFC 4180 quoting (~10 lines).

- **`CliError::OutputPipeClosed` Display message** — currently `"stdout pipe closed"`. Under the current call graph this variant is unreachable in production (the `main.rs` suppression always fires first), but if a future code path reaches `map_exit` with this variant a more explanatory message would help. From review of `8fb13cf` (2026-05-14). Batch into the next `list.rs` / `error.rs` touch.

- **`classify_write_err` vs. `is_stdout_pipe_closed` error-traversal asymmetry** — `classify_write_err` uses `root_cause()` while `is_stdout_pipe_closed` uses `chain().any()`; currently symmetric in behavior because no write site has intermediate non-`io::Error` wrappers. If a future render fn wraps `io::Error` in a non-`Error` type, `root_cause()` would still traverse via `source()`. From review of `8fb13cf` (2026-05-14). Trivial defensive note — consider symmetrising to `chain().any()` in `classify_write_err` on the next touch.
