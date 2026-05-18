//! `completions <shell>` — emit a shell completion script to stdout.
//!
//! Mirrors niri's own `niri completions <shell>` subcommand. Fish output is
//! augmented with dynamic activity-name completion appended after the
//! `clap_complete` base: tab-completing the positional `name` argument of
//! `switch`, `move-window`, `move-workspace`, `remove`, `save`, and
//! `assign-workspace` shells back into `niri-activities list --format=name`
//! for live candidates. Bash, zsh, elvish, and PowerShell receive the static
//! base only; dynamic variants for those shells are out of scope until there
//! is concrete demand.
//!
//! `create <name>` is intentionally absent from the dynamic set — the
//! argument is a new name, and completing against existing names would be
//! misleading.
//!
//! ## Position-aware conditions
//!
//! The augmentation uses two helper functions to fire only where activity
//! names are accepted:
//!
//! - `__fish_niri_activities_using_subcommand <name>` — clap_complete's
//!   own helper, true when the user is currently inside the named
//!   subcommand (parses global flags correctly, unlike the looser
//!   `__fish_seen_subcommand_from`).
//! - `__niri_activities_no_positional_yet` — emitted by this module;
//!   true when no positional arg has been provided after the subcommand.
//!   Combined with the using-subcommand check, this restricts completion
//!   to the *first* positional position for single-arg verbs (`switch`,
//!   `move-window`, `move-workspace`, `remove`, `save`).
//!
//! `assign-workspace` is variadic, so it uses the using-subcommand check
//! only — completion fires for every positional position.

use std::io::{self, Write};

use anyhow::Result;
use clap::CommandFactory;
use clap_complete::Shell;

use crate::cli::Cli;

/// Subcommands accepting exactly one activity-name positional. Completion
/// fires only at that position; after the user has typed a name, the
/// `__niri_activities_no_positional_yet` helper returns false and the
/// completion stops offering candidates.
const FISH_SINGLE_ARG_VERBS: [&str; 5] =
    ["switch", "move-window", "move-workspace", "remove", "save"];

/// Subcommands accepting one or more activity-name positionals. Completion
/// fires at every positional position (no `no_positional_yet` guard).
const FISH_VARIADIC_VERBS: [&str; 1] = ["assign-workspace"];

/// Shell command invoked at fish tab-completion time to enumerate candidate
/// activity names. `2>/dev/null` swallows the "niri socket unavailable"
/// stderr path so a stopped compositor yields zero candidates silently
/// rather than producing visible error noise during a tab press.
const FISH_NAMES_CMD: &str = "niri-activities list --format=name 2>/dev/null";

pub(crate) fn run(shell: Shell) -> Result<()> {
    let stdout = io::stdout();
    let mut out = stdout.lock();
    let mut cmd = Cli::command();
    clap_complete::generate(shell, &mut cmd, "niri-activities", &mut out);
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
            "complete -c niri-activities \
             -n \"__fish_niri_activities_using_subcommand {verb}; \
             and __niri_activities_no_positional_yet\" \
             -f -a \"({FISH_NAMES_CMD})\"",
        )?;
    }
    for verb in FISH_VARIADIC_VERBS {
        writeln!(
            w,
            "complete -c niri-activities \
             -n \"__fish_niri_activities_using_subcommand {verb}\" \
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
        "function __niri_activities_no_positional_yet\n    \
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
    fn fish_dynamic_emits_using_subcommand_line_for_every_dynamic_verb() {
        let out = String::from_utf8(fish_dynamic_bytes()).unwrap();
        for verb in FISH_SINGLE_ARG_VERBS
            .iter()
            .chain(FISH_VARIADIC_VERBS.iter())
        {
            let needle = format!("__fish_niri_activities_using_subcommand {verb}");
            assert!(
                out.contains(&needle),
                "fish dynamic output missing using_subcommand condition for `{verb}`:\n{out}",
            );
        }
    }

    #[test]
    fn fish_dynamic_guards_single_arg_verbs_with_no_positional_yet() {
        // Single-arg verbs must combine the using_subcommand check with
        // the no-positional-yet helper, so completion does not fire after
        // the user has already provided a name (the bug this guards
        // against: `niri-activities switch Foo <TAB>` offering activity
        // names for a non-existent second positional).
        let out = String::from_utf8(fish_dynamic_bytes()).unwrap();
        for verb in FISH_SINGLE_ARG_VERBS {
            let needle = format!(
                "__fish_niri_activities_using_subcommand {verb}; \
                 and __niri_activities_no_positional_yet"
            );
            assert!(
                out.contains(&needle),
                "single-arg verb `{verb}` missing combined position guard:\n{out}",
            );
        }
    }

    #[test]
    fn fish_dynamic_does_not_guard_variadic_verbs_with_no_positional_yet() {
        // Variadic verbs accept multiple positionals; the position guard
        // would incorrectly stop completion after the first name.
        let out = String::from_utf8(fish_dynamic_bytes()).unwrap();
        for verb in FISH_VARIADIC_VERBS {
            let pattern_with_guard = format!(
                "__fish_niri_activities_using_subcommand {verb}; \
                 and __niri_activities_no_positional_yet"
            );
            assert!(
                !out.contains(&pattern_with_guard),
                "variadic verb `{verb}` must not carry the no_positional_yet guard:\n{out}",
            );
        }
    }

    #[test]
    fn fish_dynamic_does_not_emit_line_for_create() {
        // `create <name>` takes a new name; completing against existing
        // names would be misleading. Guards against an accidental addition
        // of "create" to either verb const.
        let out = String::from_utf8(fish_dynamic_bytes()).unwrap();
        assert!(
            !out.contains("__fish_niri_activities_using_subcommand create"),
            "fish dynamic output must not include `create`:\n{out}",
        );
    }

    #[test]
    fn fish_dynamic_uses_list_format_name_for_candidates() {
        // Pins the source-of-truth wire: candidates must come from
        // `niri-activities list --format=name` so a rename of the CLI
        // surface that breaks this contract fails loudly here.
        let out = String::from_utf8(fish_dynamic_bytes()).unwrap();
        assert!(
            out.contains("(niri-activities list --format=name 2>/dev/null)"),
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
            out.contains("function __niri_activities_no_positional_yet"),
            "fish dynamic output must define the position-guard helper:\n{out}",
        );
        let helper_pos = out
            .find("function __niri_activities_no_positional_yet")
            .unwrap();
        let first_use_pos = out.find("and __niri_activities_no_positional_yet").unwrap();
        assert!(
            helper_pos < first_use_pos,
            "helper function must be defined before first use",
        );
    }
}
