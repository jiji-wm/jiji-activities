# jiji-activities

A user-facing CLI for KDE-style **Activities** on the **jiji** Wayland compositor — a hard-fork of [niri](https://github.com/niri-wm/niri) that adds the activities feature. See [`~/projects/desktop/de/jiji/docs/jiji-fork.md`](../../jiji/docs/jiji-fork.md) for the fork strategy.

> **Rename note (2026-05-19).** This crate was renamed from `niri-activities` to `jiji-activities` as part of the hard-fork branding. The compositor binary it talks to is still called `niri` on disk (its source-level rename to `jiji` is a follow-up sub-phase); the IPC crate is still called `niri-ipc`. So this CLI talks to `$NIRI_SOCKET` and depends on `niri-ipc` for now; both will rename in the compositor source-rename sub-phase.

## Status

**v0.1.0 tagged (`cb8b573`).** Implementation complete except for the
`move-window` subcommand, which is blocked on an IPC variant that has not yet
been implemented in the jiji compositor (the variant is on the roadmap but
unimplemented). Every other subcommand is wired and covered by tests.

## Concept

Activities are a workspace-grouping abstraction: a named bundle of workspaces
you can switch between as a unit. Each activity has its own set of workspaces,
and switching activities changes the entire active workspace set on each monitor
in one step. Useful for separating contexts (work / personal / a specific
project) without the heaviness of separate user sessions.

## Runtime dependencies

Beyond a running niri compositor (which exposes the IPC socket at
`$NIRI_SOCKET`), two external picker binaries must be on `$PATH`. Each is used
by a distinct subset of subcommands:

| Binary   | Used by                                       | Why                              |
| -------- | --------------------------------------------- | -------------------------------- |
| `fuzzel` | `switch`, `move-workspace` (no-name variants), `assign-workspace` (`« Only one… »` path) | Single-select fuzzy picker.      |
| `rofi`   | `assign-workspace`                            | Multi-select dmenu picker.       |

**rofi 2.0 or newer is required** for native Wayland layer-shell support.
Debian bookworm ships rofi 1.7 (pre-2.0) — install from bookworm-backports or
build from source. Debian trixie and newer ship rofi 2.0+. Fuzzel is available
on Debian bookworm and later via `apt install fuzzel`.

If either binary is missing from `$PATH`, the CLI exits with code 69 and a
precise stderr message naming the absent binary, e.g.:

```text
jiji-activities: picker unavailable: fuzzel: not on $PATH (required for single-select picker)
jiji-activities: picker unavailable: rofi: not on $PATH (required for multi-select picker)
```

The diagnostic names the *binary*, not "a picker," so the fix is always
unambiguous.

## Install

This release builds from source only. The `niri-ipc` dependency is a local
`path = ` reference into a checkout of the jiji compositor (which carries
the activities-related IPC variants that don't exist upstream).
Concretely, the path is `../../jiji/jiji/niri-ipc` relative to this
repo; the workspace layout that produces that path is documented in the parent
jiji workspace's `CLAUDE.md`.

```sh
# From a jiji workspace with the jiji compositor checked out alongside this repo.
cargo build --release
cargo install --path . --locked
```

`cargo install jiji-activities` (with no `--path` or `--git`) does **not** work
in this release: the crate is `publish = false` and depends on an unpublished
`niri-ipc`. Once Phase D lands `gajdusek/jiji` on GitHub with the
`feature/activities` branch pushed, `Cargo.toml` will switch to a pinned
`niri-ipc = { git = "...", rev = "..." }` line (the crate is still called
`niri-ipc` until the compositor source-rename sub-phase renames it to
`jiji-ipc`) and `cargo install --git` will become the supported install path.
This is a known v0.1.0 limitation, not a permanent shape.

## Usage

All subcommands speak directly to the niri compositor over the IPC socket
`$NIRI_SOCKET`. Subcommands that take an optional activity name open a picker
when the name is omitted; picker cancellation is treated as a successful
no-op and exits 0.

### `list` — enumerate activities

```sh
jiji-activities list                          # plain, focus-marked
jiji-activities list --json                   # versioned JSON envelope
jiji-activities list --format name,kind,focused
```

`--json` and `--format` are mutually exclusive (clap-enforced). The plain
format prints one row per activity with a leading `*` on the focused row, a
fixed-width name column, the kind (`(config)` for activities declared in
`niri.conf`, `(runtime)` for ones created via `create` or `save`), and a
trailing workspace/window count:

```text
  Work      (config)   [1 workspace, 3 windows]
* Personal  (config)   [2 workspaces, 5 windows]
  Gaming    (runtime)  [0 workspaces, 0 windows]
```

The `--json` envelope is wrapped in a versioned shape so consumers can detect
schema drift:

```json
{
  "schema_version": 1,
  "activities": [
    { "name": "Work", "kind": "config", "focused": true,
      "workspaces": [{ "id": 10, "name": "main", "sticky": false },
                     { "id": 11, "name": null, "sticky": false }],
      "window_count": 12 }
  ]
}
```

`schema_version` only increments on backward-incompatible envelope changes;
new optional fields land at the same version. Consumers should reject
unrecognised versions.

`--format=<spec>` emits one comma-joined line per activity. Recognised fields
(case-sensitive, no whitespace tolerated around commas): `name`, `kind`,
`focused`, `workspace_count`, `window_count`. Unknown or duplicate fields are
rejected with exit 64. Example output for `--format=name,kind,focused`:

```text
Work,config,false
Personal,config,true
Gaming,runtime,false
```

### `switch` — focus an activity

```sh
jiji-activities switch Work    # by name
jiji-activities switch         # picker (fuzzel)
```

Switching to the already-active activity is a silent no-op (no output, exit 0).
With no name argument, fuzzel opens with the activity list; user cancellation
exits 0.

### `switch-previous` (alias `toggle`) — flip to the previous activity

```sh
jiji-activities switch-previous
jiji-activities toggle          # alias
```

Switches to the activity that was active before the current one. With no
previous activity, the compositor returns an error and the CLI surfaces it
verbatim.

### `move-workspace` — relocate the focused workspace

```sh
jiji-activities move-workspace Personal   # by name
jiji-activities move-workspace            # picker (fuzzel)
```

Removes the focused workspace from every activity it currently belongs to and
assigns it exclusively to the named one. Picker cancellation exits 0 with no
IPC mutation.

### `assign-workspace` — multi-select activity membership

```sh
jiji-activities assign-workspace
```

Opens a rofi multi-select picker showing every activity, plus two sentinel rows
at the top:

- `« Select all »` — assign the focused workspace to every existing activity in
  one call.
- `« Only one… »` — chain into a single-select fuzzel picker to assign to
  exactly one activity (matches the `move-workspace` flow without leaving the
  picker UI).

Existing memberships are pre-checked in the picker so the user can see the
current state at a glance. Cancellation (closing the picker with no selection)
exits 0 without any IPC mutation.

### `create` — declare a runtime activity

```sh
jiji-activities create scratch
```

Creates a new runtime activity. Runtime activities are not persisted across a
compositor restart — use `save` to write the declaration into your config.
Name collisions with existing activities (config-declared or runtime) exit 73.

### `remove` — delete a runtime activity

```sh
jiji-activities remove scratch
```

Removes a runtime activity. The compositor **refuses to remove
config-declared activities**: edit `niri.conf` and reload to do that
(config is the source of truth for declared activities). Attempting to remove
a config-declared activity surfaces the compositor's refusal verbatim and exits
65.

### `save` — persist a runtime activity to `niri.conf`

```sh
jiji-activities save scratch
```

Appends an `activity "scratch"` node to your niri config file (`$NIRI_CONFIG`
if set, otherwise the platform default — on Linux, `~/.config/niri/config.kdl`) and
triggers a config reload over IPC. The edit goes through the `kdl` crate's
KDL v1 parser, which preserves the surrounding formatting, comments, and
whitespace — only the new node is inserted.

The reload is automatic: niri picks up the new declaration without a manual
intervention. If the IPC reload fails (compositor parse error, dead socket),
the on-disk edit is still in place; rerun `save` or fix the upstream issue and
reload manually.

**Caveat for dotfiles managers (chezmoi, yadm, GNU Stow, etc.):** `save` edits
the live config file directly, not the source-tracked copy in your dotfiles
repository. You must re-import the change with your dotfiles tool (`chezmoi
re-add ~/.config/niri/config.kdl` or equivalent) for it to survive the next
config sync.

### `move-window` — not yet implemented

```sh
jiji-activities move-window <name>   # exits 70 with NotImplemented
```

Currently returns `subcommand not yet implemented: move-window` (exit 70). The
upstream IPC variant for moving the focused window to a named activity is
pending in the niri fork; once it lands, the wrapper will be written. Use
`move-workspace` if moving the whole workspace is acceptable; otherwise, this
operation is not yet available.

## Exit codes

The CLI uses BSD `<sysexits.h>` codes where they apply, plus a small handful of
project-specific extras. Picker cancellation is **deliberately exit 0** — a
user backing out of a picker is not a failure, it is the user choosing not to
act.

| Code | Meaning                                                                                  |
| ---: | ---------------------------------------------------------------------------------------- |
|    0 | Success — including picker cancellation and stdout-pipe-closed (e.g. `... \| head -1`). |
|    1 | Untyped failure (fallback for any error that didn't map to a specific code).             |
|   64 | Argument-parse failure: unknown subcommand, missing arg, invalid `--format` spec.        |
|   65 | Compositor returned an unexpected reply (wrong variant, decode error, server error).     |
|   66 | Named activity not found.                                                                |
|   69 | `$NIRI_SOCKET` unreachable (stderr `"niri socket unavailable:"`) OR external picker (`fuzzel` / `rofi`) missing from `$PATH` (stderr `"picker unavailable:"`). |
|   70 | Subcommand not yet implemented (currently: `move-window`).                                                                                                      |
|   73 | `create`/`save` compositor refusal (stderr `"cannot create activity:"`) OR config-file edit failed (stderr `"config edit failed:"`).                            |

Stderr always names the failure mode with a stable prefix (`jiji-activities:
picker unavailable: ...`, `jiji-activities: niri socket unavailable: ...`,
`jiji-activities: config edit failed: ...`, etc.) so consumers can pattern-match
the surface without parsing the trailing detail.

## Manual smoke test

The default `cargo test` lane (`tests/cli.rs`, `tests/picker_shim.rs`,
`tests/rofi_shim.rs`) covers every subcommand against in-process mocks and
tempdir picker shims — no live compositor required. A separate `--ignored`
test layer (`tests/smoke.rs`) exercises the same subcommands against a real
running niri, asserting *side effects* (post-IPC state observable via
`niri msg --json`) rather than just exit codes.

Run it manually after any change that touches IPC wiring or output
formatting:

```sh
cargo test --test smoke -- --ignored --test-threads=1
```

`--test-threads=1` is mandatory — these tests mutate compositor state
(create / switch / remove activities) and would race if run in parallel.

**Prerequisites:**

- A running niri compositor with its IPC socket reachable via
  `$NIRI_SOCKET`. Tests with the precondition unmet log a `smoke: SKIP`
  breadcrumb to stderr and pass without action.
- `niri` on `$PATH` (used as a side-effect verifier; **must be the gajdusek
  fork build** — upstream `niri msg activities` exits non-zero and the entire
  smoke run skips with a `niri msg activities failed (...)` breadcrumb).

**Side-effect warning.** Smoke tests create runtime activities under a
`__nact_smoke_<test>_<pid>_<nanos>` prefix and best-effort-remove them at
the end of each test. If a test panics before cleanup, or cleanup itself
fails (compositor unreachable, etc.), stranded activities remain in the
session. Recover with:

```sh
jiji-activities list | grep __nact_smoke
jiji-activities remove <stranded-name>
```

Runtime activities do not persist across a compositor restart, so a
session restart also clears them.

The `save` subcommand and picker-driven variants are deliberately **not**
covered by the smoke layer — `save` would mutate the operator's real
`~/.config/niri/config.kdl`, and picker variants require an interactive
fuzzel / rofi binary. Cover them by exercising the binary by hand.

## Caveats

Things to know for v0.1.0:

- **Picker UX is proof-of-concept quality.** The fuzzel / rofi invocations are
  serviceable but unstyled — no theming, no per-activity icons, no preview
  pane. They will be refined post-v0.1.0.
- **`move-window` is not implemented yet** (see the subcommand section above).
- **Source-only install.** Until the `niri-ipc` dependency switches from a
  local `path = ` reference to a pinned git rev, this crate cannot be installed
  from crates.io or via `cargo install --git` alone. Build from a workspace
  checkout (see the **Install** section).
- **`save` does not integrate with dotfiles managers.** The edit lands on the
  live config file; if you track `~/.config/niri/config.kdl` in chezmoi / yadm
  / GNU Stow, you must re-import the change manually.

## License

MIT — see `LICENSE`.
