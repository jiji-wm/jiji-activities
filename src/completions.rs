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

use std::io::{self, Write};

use anyhow::Result;
use clap::CommandFactory;
use clap_complete::Shell;

use crate::cli::Cli;

/// Subcommands whose first positional argument is an existing activity
/// name. Adding a verb here augments the fish completion only.
const FISH_DYNAMIC_VERBS: [&str; 6] = [
    "switch",
    "move-window",
    "move-workspace",
    "remove",
    "save",
    "assign-workspace",
];

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
    writeln!(w, "# Dynamic activity-name completion.")?;
    for verb in FISH_DYNAMIC_VERBS {
        writeln!(
            w,
            "complete -c niri-activities -n \"__fish_seen_subcommand_from {verb}\" -f -a \"({FISH_NAMES_CMD})\"",
        )?;
    }
    Ok(())
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
    fn fish_dynamic_emits_one_line_per_verb() {
        let out = String::from_utf8(fish_dynamic_bytes()).unwrap();
        for verb in FISH_DYNAMIC_VERBS {
            let needle = format!("__fish_seen_subcommand_from {verb}\"");
            assert!(
                out.contains(&needle),
                "fish dynamic output missing condition for `{verb}`:\n{out}",
            );
        }
    }

    #[test]
    fn fish_dynamic_does_not_emit_line_for_create() {
        // `create <name>` takes a new name; completing against existing
        // names would be misleading. Guards against an accidental addition
        // of "create" to `FISH_DYNAMIC_VERBS`.
        let out = String::from_utf8(fish_dynamic_bytes()).unwrap();
        assert!(
            !out.contains("__fish_seen_subcommand_from create\""),
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
}
