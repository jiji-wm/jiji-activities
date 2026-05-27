# CLAUDE.md

Operational guidance for Claude Code (claude.ai/code) when working in the jiji-activities CLI repo.

## What this is

`jiji-activities` is the user-facing CLI for KDE-style activities on the **jiji** Wayland compositor (hard-fork of niri; was `niri-activities` before the 2026-05-19 rename). It is the Phase 3 deliverable of the activities workstream — the design lives in `docs/design.md` and is jointly owned with the compositor-side DD at `~/projects/desktop/de/jiji/docs/activities/design.md`.

The crate is a thin Rust binary (anyhow + clap-derive + `niri-ipc`) that wraps the jiji compositor's `Action::*Activity*` IPC variants. The `niri-ipc` crate name will rename to `jiji-ipc` in the compositor source-rename sub-phase. Pickers (fuzzel single-select, rofi multi-select) are external runtime deps.

## Build & test

```sh
cargo +nightly fmt --all          # required before every commit
cargo check                       # quick compile check
cargo test --all                  # all default tests; 268 + 6 ignored as of v0.1.0
cargo clippy --all --all-targets  # zero-warning baseline established Phase 3.1
```

Smoke tests against a live compositor are gated behind `#[ignore]`; run with `cargo test --test smoke -- --ignored --test-threads=1`. The smoke layer leaves no residue if all six tests pass (each runtime activity created carries the `__nact_smoke_<test>_<pid>_<nanos>` prefix; `RuntimeActivityGuard` does best-effort cleanup on Drop).

### Implementer discipline (read by jiji-rust-implementer)

- **`assert_cmd` rigor.** End-to-end CLI behavior is tested via `assert_cmd` with `$PATH`-scoped shim executables — never by mutating the real environment or talking to a live compositor.
- **Exit-code consistency.** Map error classes to stable exit codes; pin them in tests. A changed exit code without a test update is a stop-and-report condition.

## Live install

The user's installed binary lives at `~/.cargo/bin/jiji-activities`. After any feature change touching the CLI surface:

```sh
cargo install --path . --offline
```

Fish completions are regenerated automatically by chezmoi's `run_onchange_install-packages.sh.tmpl` on the next `chezmoi apply`. To force a re-fire after a CLI change, bump `# hash:` in that template file in the chezmoi source repo.

## Shell completion invariants

The `completions` subcommand emits `clap_complete`'s output plus a dynamic fish augmentation appended after it. The static part auto-tracks the clap-derive surface; the dynamic part is hand-rolled and needs manual sync when the CLI surface changes.

**Manual sync triggers — when you touch `src/cli.rs`:**

| Change to `Cmd` enum | Action in `src/completions.rs` |
|---|---|
| Add a verb whose first positional is an *existing* activity name (`switch`-like) | Add to `FISH_SINGLE_ARG_VERBS`. |
| Add a verb taking a *new* name (like `create`) | Leave OUT of `FISH_SINGLE_ARG_VERBS`. Optionally pin the intent with a new negative-space test. |
| Add a verb taking no positional at all (unit variant, picker-driven like `assign-workspace`) | Leave OUT entirely. The unit-variant shape `Cmd::Foo,` (no field block) is the authoritative signal — not the docstring. |
| Add a variadic-positional verb (multiple activity names; **none currently exist**) | Follow the future-variadic pattern documented in the module rustdoc: emit the verb's `complete` line *without* the `no_positional_yet` guard so completion fires at every positional. Probably also rename `FISH_SINGLE_ARG_VERBS` and split. |
| Rename or remove a subcommand | Update or drop the corresponding const entry. |
| Rename `list --format=name` or change its output format | Update `FISH_NAMES_CMD`. |

The unit tests in `completions::tests` iterate the const, so a renamed verb that breaks the iteration surfaces immediately. **But a forgotten *addition* is silent** — the new verb just won't get dynamic completion. Audit explicitly.

**Read the `Cmd` variant shape, not the docstring.** This rule was added after `29e304d` corrected my misclassification of `assign-workspace` as variadic. Its docstring (`"Assign the focused workspace to one or more activities via picker"`) led me to put it in `FISH_VARIADIC_VERBS`, but the variant shape `Cmd::AssignWorkspace,` (no field block) makes clear it takes no positional. The "one or more" referred to picker rows, not CLI args. The clap variant shape is the only authoritative signal.

After any sync, run:

```sh
cargo test --all
cargo install --path . --offline   # so the user's binary emits the new output
jiji-activities completions fish > ~/.config/fish/completions/jiji-activities.fish
```

Or bump `# hash:` in the chezmoi script and `chezmoi apply` to do the last two steps as one.

**Position-aware conditions discipline.** Single-arg verbs combine `__fish_jiji_activities_using_subcommand <verb>` (clap_complete's helper, subcommand-context-aware) with the `__jiji_activities_no_positional_yet` helper emitted by this module. Variadic verbs use only the using-subcommand check. **Never reach for `__fish_seen_subcommand_from <verb>` in dynamic conditions** — it fires anywhere `<verb>` has appeared in the command line, including after the user has already supplied a positional name. That bug landed at `28658d8` and was fixed at `d16a08d` after the user reported it; the rule exists so a future verb does not retrace the same mistake.

## Active design doc

`docs/design.md` is the implementer-grade DD owning Phase 3 of the activities workstream. The most-recent sub-phase outcomes are recorded as `**Reviewed:**` annotations under each Phase header; Phase 3.9 (shell completions) closed at `d16a08d` / `6d0c6a9`. The workspace `CLAUDE.md` at `~/projects/desktop/de/jiji/CLAUDE.md` tracks which sub-phase is active across sessions.

## Git

- Follow the global `~/CLAUDE.md` commit conventions: `Review-Needed: committed by Claude Code` + `AI-Assisted: <mode> (<model-id>)` trailers. Never `Co-Authored-By:`.
- Each commit is a single coherent unit. niri-ipc rev bumps land in their own commit before any code that depends on the new variants.
- Pre-commit and commit-msg hooks enforce: no design-doc references in subject or body (no `Phase`, sub-phase / sub-step / `§X.X` / `Box N` / `Appendix X` / `DD` / `design.md` / `Reviewed: YYYY-MM-DD`). Commits that legitimately edit only `docs/design.md` are exempted.
- Never push without explicit human request.

## Loop integration

The jiji-activities loop lives in `~/projects/desktop/de/jiji/.claude/` (the niri workspace) — agents `cli-architect` / `cli-implementer` / `cli-fixer` / `cli-scribe`, slash commands `/cli:land-subphase` / `/cli:next-subphase` / `/cli:implement` / `/cli:apply-review` / `/cli:scribe-review`. Direct-implementation work (no loop) is fine for small scopes; the loop is for sub-phases that warrant the full architect → implementer → review → fixer → scribe round-trip.
