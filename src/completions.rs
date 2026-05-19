//! `completions <shell>` — emit a shell completion script to stdout.
//!
//! Mirrors niri's own `niri completions <shell>` subcommand. Fish output is
//! augmented with dynamic activity-name completion appended after the
//! `clap_complete` base: tab-completing the positional `name` argument of
//! `switch`, `move-window`, `move-workspace`, `remove`, and `save` shells
//! back into `jiji-activities list --format=name` for live candidates.
//! Bash, zsh, elvish, and PowerShell receive the static base only; dynamic
//! variants for those shells are out of scope until there is concrete
//! demand.
//!
//! Verbs deliberately absent from the dynamic set:
//!
//! - `create <name>` — the argument is a new name; completing against
//!   existing names would be misleading.
//! - `assign-workspace` — takes no positional argument. The picker
//!   handles multi-select internally; the CLI surface itself is a unit
//!   variant. Any completion at `assign-workspace <TAB>` is wrong.
//! - `switch-previous`, `move-window-here`, `list`, `completions` —
//!   no activity-name positional.
//!
//! ## Position-aware conditions
//!
//! The augmentation uses two helper functions to fire only where activity
//! names are accepted:
//!
//! - `__fish_jiji_activities_using_subcommand <name>` — clap_complete's
//!   own helper, true when the user is currently inside the named
//!   subcommand (parses global flags correctly, unlike the looser
//!   `__fish_seen_subcommand_from`).
//! - `__jiji_activities_no_positional_yet` — emitted by this module;
//!   true when no positional arg has been provided after the subcommand.
//!   Combined with the using-subcommand check, this restricts completion
//!   to the *first* positional position for single-arg verbs.
//!
//! All current dynamic verbs accept exactly one positional, so the
//! combined condition is uniform. If a future verb accepts multiple
//! activity-name positionals (variadic), drop the `no_positional_yet`
//! guard for that verb so completion fires at every position.

use std::io::{self, Write};

use anyhow::Result;
use clap::CommandFactory;
use clap_complete::Shell;

use crate::cli::Cli;

/// Subcommands accepting exactly one activity-name positional. Completion
/// fires only at that position; after the user has typed a name, the
/// `__jiji_activities_no_positional_yet` helper returns false and the
/// completion stops offering candidates.
const FISH_SINGLE_ARG_VERBS: [&str; 5] =
    ["switch", "move-window", "move-workspace", "remove", "save"];

/// Shell command invoked at fish tab-completion time to enumerate candidate
/// activity names. `2>/dev/null` swallows the "niri socket unavailable"
/// stderr path so a stopped compositor yields zero candidates silently
/// rather than producing visible error noise during a tab press.
const FISH_NAMES_CMD: &str = "jiji-activities list --format=name 2>/dev/null";

pub(crate) fn run(shell: Shell) -> Result<()> {
    let stdout = io::stdout();
    let mut out = stdout.lock();
    let mut cmd = Cli::command();
    clap_complete::generate(shell, &mut cmd, "jiji-activities", &mut out);
    if matches!(shell, Shell::Fish) {
        emit_fish_dynamic(&mut out)?;
    }
    out.flush()?;
    Ok(())
}

fn emit_fish_dynamic<W: Write>(w: &mut W) -> io::Result<()> {
    writeln!(w)?;
    writeln!(w, "# Dynamic activity-name completion (position-aware).")?;
    writeln!(w)?;
    emit_no_positional_yet_helper(w)?;
    writeln!(w)?;
    for verb in FISH_SINGLE_ARG_VERBS {
        writeln!(
            w,
            "complete -c jiji-activities \
             -n \"__fish_jiji_activities_using_subcommand {verb}; \
             and __jiji_activities_no_positional_yet\" \
             -f -a \"({FISH_NAMES_CMD})\"",
        )?;
    }
    Ok(())
}

/// Emits a fish helper that returns true iff no positional argument has
/// been provided after the subcommand. Uses `commandline -opc` (tokens
/// before cursor, excluding the current word being completed) and counts
/// non-flag tokens after the first non-flag token (the subcommand).
fn emit_no_positional_yet_helper<W: Write>(w: &mut W) -> io::Result<()> {
    writeln!(
        w,
        "function __jiji_activities_no_positional_yet\n    \
             set -l tokens (commandline -opc)\n    \
             set -e tokens[1]\n    \
             set -l found_subcommand 0\n    \
             set -l positional_count 0\n    \
             for tok in $tokens\n        \
                 if string match -q -- '-*' $tok\n            \
                     continue\n        \
                 end\n        \
                 if test $found_subcommand -eq 0\n            \
                     set found_subcommand 1\n            \
                     continue\n        \
                 end\n        \
                 set positional_count (math $positional_count + 1)\n    \
             end\n    \
             test $positional_count -eq 0\n\
         end",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fish_dynamic_bytes() -> Vec<u8> {
        let mut buf = Vec::new();
        emit_fish_dynamic(&mut buf).expect("write to Vec");
        buf
    }

    #[test]
    fn fish_dynamic_guards_every_verb_with_no_positional_yet() {
        // Every current dynamic verb takes exactly one positional name, so
        // the combined position guard applies uniformly. This also pins
        // the bug fixed at d16a08d (loose `__fish_seen_subcommand_from`
        // fires anywhere the verb has been seen — including after a name
        // is already typed).
        let out = String::from_utf8(fish_dynamic_bytes()).unwrap();
        for verb in FISH_SINGLE_ARG_VERBS {
            let needle = format!(
                "__fish_jiji_activities_using_subcommand {verb}; \
                 and __jiji_activities_no_positional_yet"
            );
            assert!(
                out.contains(&needle),
                "verb `{verb}` missing combined position guard:\n{out}",
            );
        }
    }

    #[test]
    fn fish_dynamic_does_not_emit_line_for_create() {
        // `create <name>` takes a new name; completing against existing
        // names would be misleading. Guards against an accidental addition
        // of "create" to `FISH_SINGLE_ARG_VERBS`.
        let out = String::from_utf8(fish_dynamic_bytes()).unwrap();
        assert!(
            !out.contains("__fish_jiji_activities_using_subcommand create"),
            "fish dynamic output must not include `create`:\n{out}",
        );
    }

    #[test]
    fn fish_dynamic_does_not_emit_line_for_assign_workspace() {
        // `assign-workspace` is a unit variant — no positional name.
        // The picker handles multi-select internally; tab-completing at
        // `assign-workspace <TAB>` would offer activity names where none
        // are accepted. Guards against the regression that landed at
        // 28658d8 (misclassified as variadic).
        let out = String::from_utf8(fish_dynamic_bytes()).unwrap();
        assert!(
            !out.contains("__fish_jiji_activities_using_subcommand assign-workspace"),
            "fish dynamic output must not include `assign-workspace` \
             (it is a unit variant, picker-only):\n{out}",
        );
    }

    #[test]
    fn fish_dynamic_uses_list_format_name_for_candidates() {
        // Pins the source-of-truth wire: candidates must come from
        // `jiji-activities list --format=name` so a rename of the CLI
        // surface that breaks this contract fails loudly here.
        let out = String::from_utf8(fish_dynamic_bytes()).unwrap();
        assert!(
            out.contains("(jiji-activities list --format=name 2>/dev/null)"),
            "fish dynamic output must invoke `list --format=name`:\n{out}",
        );
    }

    #[test]
    fn fish_dynamic_defines_no_positional_yet_helper() {
        // The helper function definition must be emitted before the
        // `complete` lines that reference it; otherwise fish would log a
        // "unknown function" warning on every tab press.
        let out = String::from_utf8(fish_dynamic_bytes()).unwrap();
        assert!(
            out.contains("function __jiji_activities_no_positional_yet"),
            "fish dynamic output must define the position-guard helper:\n{out}",
        );
        let helper_pos = out
            .find("function __jiji_activities_no_positional_yet")
            .unwrap();
        let first_use_pos = out.find("and __jiji_activities_no_positional_yet").unwrap();
        assert!(
            helper_pos < first_use_pos,
            "helper function must be defined before first use",
        );
    }
}
