//! Labels for directories, shown after the name and coloured by how safe
//! deleting one is: `node_modules/  [cache: npm]`. The selected row's
//! label is explained in the footer. Only a label: disksweep deletes
//! nothing, so deleting one is done by hand, after a reveal.
//!
//! Three tiers, the first that matches wins:
//! - system: protected by macOS (the SIP `restricted` flag). Set by macOS,
//!   so it needs no list.
//! - cache: rebuilt by a tool on demand: [`CACHES`], or any dir holding a
//!   `CACHEDIR.TAG` (<https://bford.info/cachedir/>), which cargo, pip and
//!   others write, so most caches need no list either.
//! - known: big folders macOS and common apps keep, from [`KNOWN`], each
//!   with how to clean it up.
//!
//! Every rule is decided from the listing the directory is in and its
//! path, except the system flag and the venv and `CACHEDIR.TAG` rules: one
//! `lstat` or one small read each, and only for the dirs on screen.

/// How safe deleting a labelled dir is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Tier {
    /// macOS protects it: do not delete.
    System,
    /// A tool rebuilds it on demand.
    Cache,
    /// A big folder macOS or an app keeps, to clean up from that app.
    Known,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Label {
    pub tier: Tier,
    /// After the name, in brackets.
    pub text: &'static str,
    /// What it is and how to clean it up, for the footer.
    pub why: &'static str,
}

/// What a cache rule needs besides the directory's name.
enum Needs {
    Nothing,
    /// A file of this name next to it, in the same listing.
    Sibling(&'static str),
    /// The directory it is in has this name.
    Parent(&'static str),
    /// A file of this name inside it: one `lstat`.
    Inside(&'static str),
}

/// `(name, what else it needs, label, why)`. The README lists the same
/// table.
const CACHES: [(&str, Needs, &str, &str); 9] = [
    (
        "node_modules",
        Needs::Nothing,
        "cache: npm",
        "npm packages: npm install rebuilds them",
    ),
    (
        "target",
        Needs::Sibling("Cargo.toml"),
        "cache: cargo",
        "Rust build output: cargo build rebuilds it",
    ),
    (
        ".gradle",
        Needs::Nothing,
        "cache: gradle",
        "Gradle caches: rebuilt on the next build",
    ),
    (
        "__pycache__",
        Needs::Nothing,
        "cache: python",
        "Python bytecode: rebuilt on import",
    ),
    (
        ".venv",
        Needs::Inside("pyvenv.cfg"),
        "cache: venv",
        "a Python virtualenv: recreate it from the project's requirements",
    ),
    (
        "venv",
        Needs::Inside("pyvenv.cfg"),
        "cache: venv",
        "a Python virtualenv: recreate it from the project's requirements",
    ),
    (
        "DerivedData",
        Needs::Nothing,
        "cache: xcode",
        "Xcode build output: rebuilt on the next build",
    ),
    (
        ".cache",
        Needs::Nothing,
        "cache",
        "caches of command-line tools: rebuilt on demand",
    ),
    (
        "Caches",
        Needs::Parent("Library"),
        "cache",
        "app caches: apps rebuild them, though some start slower once",
    ),
];

const CACHEDIR_TAG: Label = Label {
    tier: Tier::Cache,
    text: "cache",
    why: "marked a cache by the tool that made it (CACHEDIR.TAG): rebuilt on demand",
};

const SYSTEM: Label = Label {
    tier: Tier::System,
    text: "system",
    why: "protected by macOS (System Integrity Protection): it cannot be deleted",
};

/// `(path, label, why)`: a path below the home dir if it starts with `~/`,
/// else absolute. The README lists the same table.
const KNOWN: [(&str, &str, &str); 18] = [
    ("~/.Trash", "trash", "the Trash: empty it in Finder"),
    (
        "~/Downloads",
        "downloads",
        "downloaded files, often old installers and archives",
    ),
    (
        "~/Library/Developer/Xcode/Archives",
        "xcode archives",
        "app archives: delete old ones in Xcode, Window > Organizer",
    ),
    (
        "~/Library/Developer/Xcode/iOS DeviceSupport",
        "xcode device support",
        "debug symbols per iOS version: downloaded again when a device connects",
    ),
    (
        "~/Library/Developer/CoreSimulator",
        "simulators",
        "iOS simulators and their data: xcrun simctl delete unavailable removes old ones",
    ),
    (
        "~/Library/Application Support/MobileSync/Backup",
        "device backups",
        "iPhone and iPad backups: manage them in Finder, under the device",
    ),
    (
        "~/Library/Containers/com.docker.docker",
        "docker",
        "Docker Desktop's disk image: docker system prune frees space in it",
    ),
    (
        "~/Library/Mail",
        "mail",
        "Mail's copy of your messages and attachments",
    ),
    (
        "~/Library/Messages",
        "messages",
        "Messages history and attachments: Settings > Messages can keep less",
    ),
    (
        "~/Pictures/Photos Library.photoslibrary",
        "photos",
        "the Photos library: manage it in Photos, never in Finder",
    ),
    (
        "~/Library/Android/sdk",
        "android sdk",
        "Android SDK and emulator images: manage them in Android Studio",
    ),
    (
        "~/.rustup/toolchains",
        "rust toolchains",
        "Rust toolchains: rustup toolchain uninstall removes old ones",
    ),
    (
        "~/.cargo/registry",
        "cargo registry",
        "downloaded crates: cargo downloads them again when needed",
    ),
    (
        "~/.npm",
        "npm cache",
        "npm's download cache: npm cache clean --force empties it",
    ),
    (
        "~/go/pkg/mod",
        "go modules",
        "the Go module cache: go clean -modcache empties it",
    ),
    (
        "~/.ollama/models",
        "ollama models",
        "Ollama model weights: ollama rm removes one",
    ),
    (
        "/private/var/vm",
        "swap",
        "swap and the sleep image: managed by macOS",
    ),
    (
        "/private/var/folders",
        "temporary",
        "per-user temporary files and caches: macOS cleans them up",
    ),
];

/// What [`label`] knows of a directory.
pub(crate) struct Dir<'a> {
    pub name: &'a [u8],
    /// The name of the directory it is in.
    pub parent: &'a [u8],
    /// Its real path, if known, and the home dir.
    pub path: Option<&'a [u8]>,
    pub home: Option<&'a [u8]>,
}

/// The label of `dir`, if a rule matches. `sibling` says whether its
/// parent holds a file of a name, `inside` whether it does, `system`
/// whether macOS protects it, and `tagged` whether it holds a valid
/// `CACHEDIR.TAG`.
pub(crate) fn label(
    dir: &Dir<'_>,
    sibling: impl Fn(&str) -> bool,
    inside: impl Fn(&str) -> bool,
    system: impl FnOnce() -> bool,
    tagged: impl FnOnce() -> bool,
) -> Option<Label> {
    if system() {
        return Some(SYSTEM);
    }
    let cache = CACHES.iter().find(|(n, ..)| n.as_bytes() == dir.name);
    if let Some((_, needs, text, why)) = cache {
        let matched = match *needs {
            Needs::Nothing => true,
            Needs::Sibling(file) => sibling(file),
            Needs::Parent(name) => dir.parent == name.as_bytes(),
            Needs::Inside(file) => inside(file),
        };
        if matched {
            return Some(Label {
                tier: Tier::Cache,
                text,
                why,
            });
        }
    }
    if tagged() {
        return Some(CACHEDIR_TAG);
    }
    let path = dir.path?;
    KNOWN.iter().find_map(|&(at, text, why)| {
        let matches = match at.strip_prefix("~") {
            Some(below) => dir
                .home
                .is_some_and(|home| path.strip_prefix(home) == Some(below.as_bytes())),
            None => path == at.as_bytes(),
        };
        matches.then_some(Label {
            tier: Tier::Known,
            text,
            why,
        })
    })
}
