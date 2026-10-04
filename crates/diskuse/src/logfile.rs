//! The debug log: `DISKUSE_LOG=file` appends one line per notable event,
//! a scan starting or done, changes the OS missed and why. Off unless set:
//! the browser has the screen, so a file is the only place to say them.

use log::{Level, LevelFilter, Log, Metadata, Record};
use std::fs::File;
use std::io::Write;
use std::sync::Mutex;
use std::time::SystemTime;

struct FileLog(Mutex<File>);

impl Log for FileLog {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= Level::Info
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        // the time of day, UTC: std knows no time zones
        let secs = SystemTime::UNIX_EPOCH.elapsed().map_or(0, |d| d.as_secs()) % 86400;
        let (h, m, s) = (secs / 3600, secs / 60 % 60, secs % 60);
        let level = record.level().as_str().to_ascii_lowercase();
        // a full disk is when this runs: a line lost is not worth failing for
        let _ = writeln!(
            self.0.lock().unwrap(),
            "{h:02}:{m:02}:{s:02}Z {level:<5} {}",
            record.args()
        );
    }

    fn flush(&self) {}
}

/// Starts logging to the file `$DISKUSE_LOG` names, if it is set and can
/// be opened. Says so on stderr if it cannot.
pub(crate) fn init_from_env() {
    let Some(path) = std::env::var_os("DISKUSE_LOG").filter(|p| !p.is_empty()) else {
        return;
    };
    // the one file diskuse writes outside its cache dir, and only when asked
    #[allow(clippy::disallowed_methods)]
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path);
    match file {
        Ok(file) => {
            // a logger set before, by whoever runs the command as a library, stays
            if log::set_boxed_logger(Box::new(FileLog(Mutex::new(file)))).is_ok() {
                log::set_max_level(LevelFilter::Info);
            }
        }
        Err(e) => eprintln!("diskuse: cannot log to {}: {e}", path.to_string_lossy()),
    }
}
