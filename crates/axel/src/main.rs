//! `axel` — agent orchestration, designed for Rex.
//!
//! See `docs/DESIGN.md`. This binary is thin glue over the library crates:
//! it detects Rex, opens the floating launcher, performs launches, ingests
//! harness hooks and exposes a few inspection commands.

mod cli;
mod cmd;
mod paths;

use clap::Parser;

fn main() {
    let args = cli::Cli::parse();
    if let Err(err) = cmd::dispatch(args) {
        eprintln!("axel: {err:#}");
        std::process::exit(1);
    }
}
