# niri-activities

A user-facing CLI for KDE-style **Activities** on the [niri](https://github.com/YaLTeR/niri/) wayland compositor.

Activities are a workspace-grouping abstraction: a named bundle of workspaces you can switch between as a unit. Each activity has its own set of workspaces, and switching activities changes the entire active workspace set on each monitor in one step. Useful for separating contexts (work / personal / a specific project) without the heaviness of separate user sessions.

## Status

**Pre-alpha — repo bootstrapped 2026-04-25.** The CLI surface, error model, and IPC strategy are still being designed; nothing is implemented yet. Running the binary currently prints a stub message and exits with `EX_USAGE` (64).

## Design

The compositor side of Activities is documented in the niri workspace:

- **Compositor design (workspace-as-atom model, IPC extensions, data model):** `~/projects/desktop/de/niri/docs/activities/design.md`
- **CLI implementer-grade design (this repo):** `docs/design.md` *(in progress — see Active work below)*

The split is deliberate: the compositor DD covers compositor-side machinery (workspace pool, monitor view, IPC additions); this repo covers everything user-facing (subcommands, error model, output format, picker integration, test strategy).

## Active work

Authoring `docs/design.md` is the first sub-phase of the CLI loop. Q3's open items from the parent workspace (error model & exit codes; `niri msg` shell-out vs direct `niri-ipc` library use; fuzzel picker contract; `list` output format; test strategy) are the design questions to settle in the DD before any implementation begins.

## Build

```sh
cargo build --release
```

## License

MIT — see `LICENSE`.
