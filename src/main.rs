use clap::{Args, Parser, Subcommand};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

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
    /// the result for `show`. On macOS, a saved scan of PATH is brought up
    /// to date from the changes since, when macOS recorded them.
    Scan {
        path: PathBuf,
        /// Scan all of PATH, even if a saved scan could be brought up to date
        #[arg(long)]
        full: bool,
        /// Worker threads [default: available parallelism]
        #[arg(long)]
        threads: Option<NonZeroUsize>,
        /// Directory reader, for the differential test and benchmarks
        #[arg(long, value_enum, default_value_t, hide = true)]
        reader: disksweep::Reader,
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
          value_parser = clap::value_parser!(u16).range(1..=disksweep::LARGEST as i64))]
    top: Option<u16>,
}

impl Output {
    fn reclaimable(&self) -> bool {
        self.reclaimable.on()
    }

    fn print(&self, tree: &disksweep::Tree) {
        let top = self.top.map(usize::from);
        match self.json {
            true => print!(
                "{}",
                disksweep::json(tree, self.reclaimable(), self.depth.unwrap_or(1), top)
            ),
            false => print!("{}", disksweep::report(tree, self.reclaimable(), top)),
        }
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match (cli.command, cli.path) {
        (
            Some(Command::Scan {
                path,
                full,
                threads,
                reader,
                output,
            }),
            _,
        ) => scan(&path, full, threads, reader, &output),
        (Some(Command::Show { path, output }), _) => show(&path, &output),
        (None, path) => browse(path.as_deref(), cli.reclaimable.on()),
    }
}

fn browse(path: Option<&Path>, reclaimable: bool) -> ExitCode {
    match (disksweep::browse(path, reclaimable), path) {
        (Ok(()), _) => ExitCode::SUCCESS,
        (Err(e), Some(path)) => {
            eprintln!("disksweep: {}: {e}", path.display());
            ExitCode::FAILURE
        }
        (Err(e), None) => {
            eprintln!("disksweep: cannot list volumes: {e}");
            ExitCode::FAILURE
        }
    }
}

fn scan(
    path: &Path,
    full: bool,
    threads: Option<NonZeroUsize>,
    reader: disksweep::Reader,
    output: &Output,
) -> ExitCode {
    let opts = disksweep::ScanOptions {
        threads,
        reclaimable: output.reclaimable(),
        reader,
    };
    let cache = disksweep::CacheDir::from_env();
    // a saved scan that cannot be read is replaced, like a missing one
    let saved = (cache.as_ref().ok())
        .filter(|_| !full)
        .and_then(|cache| cache.load(path).ok().flatten())
        .filter(|saved| saved.reclaimable == opts.reclaimable);
    let updated =
        saved.and_then(|saved| disksweep::update(path, saved.tree, &opts, cache.as_ref().ok()));
    let tree = match updated.map_or_else(|| disksweep::scan(path, &opts), Ok) {
        Ok(tree) => tree,
        Err(e) => {
            eprintln!("disksweep: {}: {e}", path.display());
            return ExitCode::FAILURE;
        }
    };
    output.print(&tree);
    // the printed result stands even if it cannot be saved
    let saved = cache.and_then(|cache| cache.save(path, &tree, opts.reclaimable));
    if let Err(e) = saved {
        eprintln!("disksweep: warning: scan not saved: {e}");
    }
    ExitCode::SUCCESS
}

fn show(path: &Path, output: &Output) -> ExitCode {
    let loaded = disksweep::CacheDir::from_env().and_then(|cache| cache.load(path));
    let error = match loaded {
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
    eprintln!("disksweep: {error}");
    ExitCode::FAILURE
}
