//! The `diskuse` command line: arguments, `scan`, `show` and the browser.

use clap::{Args, Parser, Subcommand};
use diskuse_core::{CacheDir, LARGEST, ReadTree, ScanOptions, Stop, Units, json, report};
use signal_hook::consts::{SIGINT, SIGTERM};
use std::ffi::OsString;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
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
    #[command(flatten)]
    si: Si,
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
        reader: Reader,
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

/// `--reader`: [`diskuse_core::Reader`], as an argument.
#[derive(Clone, Copy, Default, clap::ValueEnum)]
enum Reader {
    #[default]
    Auto,
    Portable,
}

impl From<Reader> for diskuse_core::Reader {
    fn from(reader: Reader) -> Self {
        match reader {
            Reader::Auto => Self::Auto,
            Reader::Portable => Self::Portable,
        }
    }
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

/// `--si`.
#[derive(Args)]
struct Si {
    /// Sizes in powers of 1000 (kB, MB, GB), as Finder and disk makers, not
    /// of 1024 (KiB, MiB, GiB). The browser switches with `u`
    #[arg(long)]
    si: bool,
}

impl Si {
    fn units(&self) -> Units {
        match self.si {
            true => Units::Decimal,
            false => Units::Binary,
        }
    }
}

/// Flags `scan` and `show` share, so `show` can print what `scan` did.
#[derive(Args)]
struct Output {
    #[command(flatten)]
    reclaimable: Reclaimable,
    #[command(flatten)]
    si: Si,
    /// Print JSON instead of text, sizes in bytes
    #[arg(long)]
    json: bool,
    /// Levels of subdirectories in the JSON [default: 1]
    #[arg(long, requires = "json")]
    depth: Option<usize>,
    /// Also list the N largest files
    #[arg(long, value_name = "N",
          value_parser = clap::value_parser!(u16).range(1..=LARGEST as i64))]
    top: Option<u16>,
}

impl Output {
    fn reclaimable(&self) -> bool {
        self.reclaimable.on()
    }

    fn print(&self, tree: &impl ReadTree) {
        let top = self.top.map(usize::from);
        match self.json {
            true => print!(
                "{}",
                json(tree, self.reclaimable(), self.depth.unwrap_or(1), top)
            ),
            false => print!("{}", report(tree, self.reclaimable(), top, self.si.units())),
        }
    }
}

/// Runs the `diskuse` command line with `args`, the program name first, and
/// returns its exit code. The binary and the Python package's console
/// script both call this, so they cannot drift apart.
pub fn cli(args: impl IntoIterator<Item = OsString>) -> u8 {
    let cli = Cli::parse_from(args);
    crate::logfile::init_from_env();
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
        (None, path) => browse(path.as_deref(), cli.reclaimable.on(), cli.si.units()),
    }
}

fn browse(path: Option<&Path>, reclaimable: bool, units: Units) -> u8 {
    match (crate::app::browse(path, reclaimable, units), path) {
        (Ok(()), _) => 0,
        (Err(e), Some(path)) => {
            eprintln!("diskuse: {}: {e}", path.display());
            1
        }
        (Err(e), None) => {
            eprintln!("diskuse: cannot list volumes: {e}");
            1
        }
    }
}

fn scan(
    path: &Path,
    threads: Option<NonZeroUsize>,
    reader: Reader,
    stop_after: Option<usize>,
    output: &Output,
) -> u8 {
    // the number of the signal that stopped the scan, if one did
    let signal = Arc::new(AtomicUsize::new(0));
    for sig in [SIGINT, SIGTERM] {
        // only fails for signals that cannot be caught
        signal_hook::flag::register_usize(sig, Arc::clone(&signal), sig as usize).unwrap();
    }
    let listed = AtomicUsize::new(0);
    let stopped = Arc::clone(&signal);
    let stop = Stop::new(move || {
        let enough = stop_after.is_some_and(|n| listed.fetch_add(1, Ordering::Relaxed) >= n);
        enough || stopped.load(Ordering::Relaxed) != 0
    });
    let opts = ScanOptions {
        threads,
        reclaimable: output.reclaimable(),
        reader: reader.into(),
        stop,
    };
    let tree = match diskuse_core::scan(path, &opts) {
        Ok(tree) => tree,
        Err(e) => {
            eprintln!("diskuse: {}: {e}", path.display());
            return 1;
        }
    };
    output.print(&tree);
    // the printed result stands even if it cannot be saved
    let saved = CacheDir::from_env().and_then(|cache| cache.save(path, &tree, opts.reclaimable));
    if let Err(e) = saved {
        eprintln!("diskuse: warning: scan not saved: {e}");
    }
    // like a shell reports a process the signal ended
    match signal.load(Ordering::Relaxed) {
        0 => 0,
        sig => 128 + sig as u8,
    }
}

fn show(path: &Path, output: &Output) -> u8 {
    let file = CacheDir::from_env().and_then(|cache| cache.read(path));
    // read in place: no tree is built
    let error = match file.as_ref().map(|f| f.as_ref().and_then(|f| f.check())) {
        Ok(Some(saved)) if output.reclaimable() && !saved.reclaimable => {
            format!("saved scan for {} has no reclaimable sizes", path.display())
        }
        Ok(Some(saved)) => {
            output.print(&saved.tree);
            return 0;
        }
        Ok(None) => format!("no saved scan for {}", path.display()),
        Err(e) => format!("cannot read saved scan for {}: {e}", path.display()),
    };
    eprintln!("diskuse: {error}");
    1
}
