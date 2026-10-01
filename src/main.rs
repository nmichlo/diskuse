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
        /// Add a column of reclaimable bytes: the space deleting each item
        /// alone frees, while clones of its files elsewhere remain
        #[cfg(target_os = "macos")]
        #[arg(short, long)]
        reclaimable: bool,
        /// Directory reader, for the differential test and benchmarks
        #[arg(long, value_enum, default_value_t, hide = true)]
        reader: disksweep::Reader,
    },
}

fn main() -> ExitCode {
    let Command::Scan {
        path,
        threads,
        #[cfg(target_os = "macos")]
        reclaimable,
        reader,
    } = Cli::parse().command;
    #[cfg(not(target_os = "macos"))]
    let reclaimable = false;
    let opts = disksweep::ScanOptions {
        threads,
        reclaimable,
        reader,
    };
    match disksweep::scan(&path, &opts) {
        Ok(tree) => {
            print!("{}", disksweep::report(&tree, reclaimable));
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("disksweep: {}: {e}", path.display());
            ExitCode::FAILURE
        }
    }
}
