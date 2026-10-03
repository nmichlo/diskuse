use clap::{Args, Parser, Subcommand};
use signal_hook::consts::{SIGINT, SIGTERM};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Parser)]
#[command(version, about, args_conflicts_with_subcommands = true)]
struct Cli {
    /// Browse PATH full screen while it is scanned. Without PATH, pick a
    /// volume to browse first.
    path: Option<PathBuf>,
    #[command(flatten)]
    reclaimable: Reclaimable,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Print the size of PATH and of each of its direct children, and save
    /// the result for `show`. Ctrl-C stops the scan, then prints and saves
    /// what it found, marked incomplete.
    Scan {
        path: PathBuf,
        /// Worker threads [default: 4, up to the number of cores while waiting on the disk]
        #[arg(long)]
        threads: Option<NonZeroUsize>,
        /// Directory reader, for the differential test and benchmarks
        #[arg(long, value_enum, default_value_t, hide = true)]
        reader: diskuse::Reader,
        /// Stop after listing N directories below PATH, as Ctrl-C would, for
        /// the tests
        #[arg(long, value_name = "N", hide = true)]
        stop_after: Option<usize>,
        #[command(flatten)]
        output: Output,
    },
    /// Print the last saved scan of PATH, exactly as `scan` printed it,
    /// without scanning.
    Show {
        path: PathBuf,
        #[command(flatten)]
        output: Output,
    },
}

/// `-r`, which only macOS has.
#[derive(Args)]
struct Reclaimable {
    /// Add a column of reclaimable bytes: the space deleting each item
    /// alone frees, while clones of its files elsewhere remain
    #[cfg(target_os = "macos")]
    #[arg(short, long)]
    reclaimable: bool,
}

impl Reclaimable {
    fn on(&self) -> bool {
        #[cfg(target_os = "macos")]
        return self.reclaimable;
        #[cfg(not(target_os = "macos"))]
        false
    }
}

/// Flags `scan` and `show` share, so `show` can print what `scan` did.
#[derive(Args)]
struct Output {
    #[command(flatten)]
    reclaimable: Reclaimable,
    /// Print JSON instead of text
    #[arg(long)]
    json: bool,
    /// Levels of subdirectories in the JSON [default: 1]
    #[arg(long, requires = "json")]
    depth: Option<usize>,
    /// Also list the N largest files
    #[arg(long, value_name = "N",
          value_parser = clap::value_parser!(u16).range(1..=diskuse::LARGEST as i64))]
    top: Option<u16>,
}

impl Output {
    fn reclaimable(&self) -> bool {
        self.reclaimable.on()
    }

    fn print(&self, tree: &impl diskuse::ReadTree) {
        let top = self.top.map(usize::from);
        match self.json {
            true => print!(
                "{}",
                diskuse::json(tree, self.reclaimable(), self.depth.unwrap_or(1), top)
            ),
            false => print!("{}", diskuse::report(tree, self.reclaimable(), top)),
        }
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match (cli.command, cli.path) {
        (
            Some(Command::Scan {
                path,
                threads,
                reader,
                stop_after,
                output,
            }),
            _,
        ) => scan(&path, threads, reader, stop_after, &output),
        (Some(Command::Show { path, output }), _) => show(&path, &output),
        (None, path) => browse(path.as_deref(), cli.reclaimable.on()),
    }
}

fn browse(path: Option<&Path>, reclaimable: bool) -> ExitCode {
    match (diskuse::browse(path, reclaimable), path) {
        (Ok(()), _) => ExitCode::SUCCESS,
        (Err(e), Some(path)) => {
            eprintln!("diskuse: {}: {e}", path.display());
            ExitCode::FAILURE
        }
        (Err(e), None) => {
            eprintln!("diskuse: cannot list volumes: {e}");
            ExitCode::FAILURE
        }
    }
}

fn scan(
    path: &Path,
    threads: Option<NonZeroUsize>,
    reader: diskuse::Reader,
    stop_after: Option<usize>,
    output: &Output,
) -> ExitCode {
    // the number of the signal that stopped the scan, if one did
    let signal = Arc::new(AtomicUsize::new(0));
    for sig in [SIGINT, SIGTERM] {
        // only fails for signals that cannot be caught
        signal_hook::flag::register_usize(sig, Arc::clone(&signal), sig as usize).unwrap();
    }
    let listed = AtomicUsize::new(0);
    let stopped = Arc::clone(&signal);
    let stop = diskuse::Stop::new(move || {
        let enough = stop_after.is_some_and(|n| listed.fetch_add(1, Ordering::Relaxed) >= n);
        enough || stopped.load(Ordering::Relaxed) != 0
    });
    let opts = diskuse::ScanOptions {
        threads,
        reclaimable: output.reclaimable(),
        reader,
        stop,
    };
    let tree = match diskuse::scan(path, &opts) {
        Ok(tree) => tree,
        Err(e) => {
            eprintln!("diskuse: {}: {e}", path.display());
            return ExitCode::FAILURE;
        }
    };
    output.print(&tree);
    // the printed result stands even if it cannot be saved
    let saved =
        diskuse::CacheDir::from_env().and_then(|cache| cache.save(path, &tree, opts.reclaimable));
    if let Err(e) = saved {
        eprintln!("diskuse: warning: scan not saved: {e}");
    }
    // like a shell reports a process the signal ended
    match signal.load(Ordering::Relaxed) {
        0 => ExitCode::SUCCESS,
        sig => ExitCode::from(128 + sig as u8),
    }
}

fn show(path: &Path, output: &Output) -> ExitCode {
    let file = diskuse::CacheDir::from_env().and_then(|cache| cache.read(path));
    // read in place: no tree is built
    let error = match file.as_ref().map(|f| f.as_ref().and_then(|f| f.check())) {
        Ok(Some(saved)) if output.reclaimable() && !saved.reclaimable => {
            format!("saved scan for {} has no reclaimable sizes", path.display())
        }
        Ok(Some(saved)) => {
            output.print(&saved.tree);
            return ExitCode::SUCCESS;
        }
        Ok(None) => format!("no saved scan for {}", path.display()),
        Err(e) => format!("cannot read saved scan for {}: {e}", path.display()),
    };
    eprintln!("diskuse: {error}");
    ExitCode::FAILURE
}
