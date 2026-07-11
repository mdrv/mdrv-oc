// ===========================================================================
// main.rs — entry point for the `mdrv-oc` binary.
//
// All real work lives in the library + `cli.rs`. `main` does the two things
// idiomatic Rust CLIs do:
//   1. Parse args (`Cli::parse()`). clap handles `--help`/`--version` and
//      exits on its own, so we never see those here.
//   2. Run the command. Returning `anyhow::Result<()>` from `main` lets an
//      `Err` bubble all the way up; the runtime prints it and sets the exit
//      code to 1.
//
// `mod cli;` pulls in `src/cli.rs`. Because no `lib.rs` declares `mod cli;`,
// this file is binary-only (the library never sees clap).
// ===========================================================================

mod cli;

use clap::Parser;
use cli::{run, Cli};

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    run(cli)
}
