use clap::{Parser, Subcommand};
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Print the size of PATH and of each of its direct children.
    Scan {
        path: PathBuf,
        /// Worker threads [default: available parallelism]
        #[arg(long)]
        threads: Option<NonZeroUsize>,
    },
}

fn main() -> ExitCode {
    let Command::Scan { path, threads } = Cli::parse().command;
    match disksweep::scan(&path, &disksweep::ScanOptions { threads }) {
        Ok(tree) => {
            print!("{}", disksweep::report(&tree));
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("disksweep: {}: {e}", path.display());
            ExitCode::FAILURE
        }
    }
}
