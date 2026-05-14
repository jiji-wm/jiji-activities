//! `niri-activities` — KDE-style Activities CLI for the niri compositor.
//!
//! `main` is intentionally thin: parse the CLI, dispatch, then map any
//! error back to a sysexits-style exit code. All real work lives in
//! [`cli::dispatch`] and the per-subcommand stubs.

use std::process::ExitCode;

use clap::Parser;

mod cli;
mod error;
mod ipc;

fn main() -> ExitCode {
    let parsed = match cli::Cli::try_parse() {
        Ok(parsed) => parsed,
        Err(err) => {
            // clap prints its own message; we only decide the code.
            // `--help` / `--version` use stdout and exit 0; real
            // parse errors use stderr and exit 64 (`EX_USAGE`,
            // overriding clap's default of 2).
            let _ = err.print();
            return if err.use_stderr() {
                ExitCode::from(64)
            } else {
                ExitCode::SUCCESS
            };
        }
    };

    match cli::dispatch(parsed) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            // `{:#}` walks the full anyhow chain so context layers
            // added via `.context(...)` survive to stderr.
            eprintln!("niri-activities: {err:#}");
            ExitCode::from(u8::try_from(error::map_exit(&err)).unwrap_or(1))
        }
    }
}
