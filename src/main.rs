use anyhow::Result;
use clap::Parser;

use cct::cli;
use cct::commands;

fn main() -> Result<()> {
    let cli = cli::Cli::parse();
    if let Err(err) = commands::run(cli.command) {
        eprintln!("Error: {err:#}");
        std::process::exit(1);
    }
    Ok(())
}
