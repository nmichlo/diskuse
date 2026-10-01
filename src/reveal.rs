//! Showing an item in the desktop's file manager. The only module that runs
//! other programs, and only `open` or `xdg-open`, which never change files.

use std::ffi::OsString;
use std::io;
use std::path::Path;
use std::process::{Command, Stdio};

/// The desktop that can show a path, if any.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Desktop {
    Mac,
    Linux,
    /// Over SSH, or Linux with no display: a window would open elsewhere or
    /// not at all.
    None,
}

impl Desktop {
    /// The desktop of this process.
    pub fn from_env() -> Self {
        let set = |k: &str| std::env::var_os(k).is_some_and(|v| !v.is_empty());
        Self::detect(cfg!(target_os = "macos"), set)
    }

    /// The desktop on macOS or Linux, given which environment variables are
    /// `set`.
    pub fn detect(macos: bool, set: impl Fn(&str) -> bool) -> Self {
        if set("SSH_CONNECTION") {
            Self::None
        } else if macos {
            Self::Mac
        } else if set("DISPLAY") || set("WAYLAND_DISPLAY") {
            Self::Linux
        } else {
            Self::None
        }
    }

    /// The program and arguments that show `path` in its folder (`reveal`)
    /// or open it. Linux file managers cannot select a file, so reveal
    /// opens the folder.
    pub fn command(self, path: &Path, reveal: bool) -> Option<(&'static str, Vec<OsString>)> {
        let path = path.as_os_str().to_owned();
        match self {
            Self::Mac if reveal => Some(("/usr/bin/open", vec!["-R".into(), path])),
            Self::Mac => Some(("/usr/bin/open", vec![path])),
            Self::Linux if reveal => {
                let parent = Path::new(&path).parent()?.as_os_str().to_owned();
                Some(("xdg-open", vec![parent]))
            }
            Self::Linux => Some(("xdg-open", vec![path])),
            Self::None => None,
        }
    }
}

/// Starts `program` with `args`, no shell, its output discarded. Returns
/// without waiting; a thread reaps it.
pub fn spawn(program: &str, args: &[OsString]) -> io::Result<()> {
    // the one place disksweep runs a program: `open` or `xdg-open`
    #[allow(clippy::disallowed_methods)]
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    std::thread::spawn(move || child.wait());
    Ok(())
}
