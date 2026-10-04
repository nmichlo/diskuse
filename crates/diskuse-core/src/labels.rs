//! Labels for directories: each colours its dir's name by how safe
//! deleting it is, and the line about the selected row says which label
//! and why, `cache: npm | npm install rebuilds it`. Only a label: diskuse deletes
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
//! Most rules are decided from the directory's name and path. The system
//! flag, a project file beside it and `CACHEDIR.TAG` take one `lstat` or
//! one small read each, so [`Labels::label`] is for the dirs someone looks
//! at, not for every dir of a tree.

use crate::sys;
use crate::tree::ReadTree;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

/// How a valid `CACHEDIR.TAG` starts (<https://bford.info/cachedir/>).
const CACHEDIR_SIGNATURE: &[u8] = b"Signature: 8a477f597d28d172789f06886806bc55";

/// How safe deleting a labelled dir is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tier {
    /// macOS protects it: do not delete.
    System,
    /// A tool rebuilds it on demand.
    Cache,
    /// A big folder macOS or an app keeps, to clean up from that app.
    Known,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Label {
    pub tier: Tier,
    /// What it is, in a word or two: `cache: npm`.
    pub text: &'static str,
    /// What it is and how to clean it up, for the `i` box.
    pub why: &'static str,
}

impl Label {
    /// `why` in a few words, for the line about the row at the cursor:
    /// what to do about it, the part after its colon.
    pub fn short(&self) -> &'static str {
        match self.why.rsplit_once(": ") {
            Some((_, what)) => what,
            None => self.why,
        }
    }
}

/// What a cache rule needs besides the directory's name.
enum Needs {
    Nothing,
    /// A file of one of these names next to it, in the same listing.
    Sibling(&'static [&'static str]),
    /// The directory it is in has this name.
    Parent(&'static str),
    /// A file of this name inside it: one `lstat`.
    Inside(&'static str),
}

use Needs::{Inside, Nothing, Parent, Sibling};

/// `(name, what else it needs, label, why)`: build output and downloads
/// that a tool makes again on demand. Names other tools use for other
/// things (`target`, `build`, `Pods`) need the tool's project file beside
/// them. The README lists the same table.
const CACHES: [(&str, Needs, &str, &str); 34] = [
    (
        "node_modules",
        Nothing,
        "cache: npm",
        "npm install rebuilds it",
    ),
    (
        "bower_components",
        Nothing,
        "cache: bower",
        "bower install rebuilds it",
    ),
    (
        ".next",
        Nothing,
        "cache: next.js",
        "Next.js build output: next build rebuilds it",
    ),
    (
        ".nuxt",
        Nothing,
        "cache: nuxt",
        "Nuxt build output: nuxt build rebuilds it",
    ),
    (
        ".svelte-kit",
        Nothing,
        "cache: sveltekit",
        "SvelteKit build output: rebuilt on the next build",
    ),
    (
        ".turbo",
        Nothing,
        "cache: turborepo",
        "Turborepo cache: rebuilt on the next build",
    ),
    (
        ".parcel-cache",
        Nothing,
        "cache: parcel",
        "Parcel cache: rebuilt on the next build",
    ),
    (
        ".angular",
        Nothing,
        "cache: angular",
        "Angular CLI cache: rebuilt on the next build",
    ),
    (
        ".docusaurus",
        Nothing,
        "cache: docusaurus",
        "Docusaurus build cache: rebuilt on the next build",
    ),
    (
        ".expo",
        Nothing,
        "cache: expo",
        "Expo cache: rebuilt on the next start",
    ),
    (
        "target",
        Sibling(&["Cargo.toml"]),
        "cache: cargo",
        "Rust build output: cargo build rebuilds it",
    ),
    (
        "target",
        Sibling(&["pom.xml"]),
        "cache: maven",
        "Maven build output: mvn package rebuilds it",
    ),
    (
        ".gradle",
        Nothing,
        "cache: gradle",
        "Gradle caches: rebuilt on the next build",
    ),
    (
        "build",
        Sibling(&["build.gradle", "build.gradle.kts"]),
        "cache: gradle",
        "Gradle build output: rebuilt on the next build",
    ),
    (
        "__pycache__",
        Nothing,
        "cache: python",
        "Python bytecode: rebuilt on import",
    ),
    (
        ".pytest_cache",
        Nothing,
        "cache: pytest",
        "pytest cache: rebuilt on the next run",
    ),
    (
        ".mypy_cache",
        Nothing,
        "cache: mypy",
        "mypy cache: rebuilt on the next run",
    ),
    (
        ".ruff_cache",
        Nothing,
        "cache: ruff",
        "ruff cache: rebuilt on the next run",
    ),
    (
        ".tox",
        Nothing,
        "cache: tox",
        "tox environments: rebuilt on the next run",
    ),
    (
        ".nox",
        Nothing,
        "cache: nox",
        "nox environments: rebuilt on the next run",
    ),
    (
        ".ipynb_checkpoints",
        Nothing,
        "cache: jupyter",
        "Jupyter autosaves of notebooks you have saved since",
    ),
    (
        ".venv",
        Inside("pyvenv.cfg"),
        "cache: venv",
        "a Python virtualenv: recreate it from the project's requirements",
    ),
    (
        "venv",
        Inside("pyvenv.cfg"),
        "cache: venv",
        "a Python virtualenv: recreate it from the project's requirements",
    ),
    (
        "DerivedData",
        Nothing,
        "cache: xcode",
        "Xcode build output: rebuilt on the next build",
    ),
    (
        "Pods",
        Sibling(&["Podfile"]),
        "cache: cocoapods",
        "CocoaPods dependencies: pod install restores them",
    ),
    (
        ".build",
        Sibling(&["Package.swift"]),
        "cache: swiftpm",
        "Swift package build output: swift build rebuilds it",
    ),
    (
        ".dart_tool",
        Nothing,
        "cache: dart",
        "Dart tool cache: rebuilt by dart pub get",
    ),
    (
        "_build",
        Sibling(&["mix.exs"]),
        "cache: elixir",
        "Elixir build output: mix compile rebuilds it",
    ),
    (
        "deps",
        Sibling(&["mix.exs"]),
        "cache: elixir",
        "Elixir dependencies: mix deps.get restores them",
    ),
    (
        ".stack-work",
        Nothing,
        "cache: stack",
        "Haskell Stack build output: stack build rebuilds it",
    ),
    (
        "dist-newstyle",
        Nothing,
        "cache: cabal",
        "Cabal build output: cabal build rebuilds it",
    ),
    (
        ".terraform",
        Nothing,
        "cache: terraform",
        "Terraform providers and modules: terraform init restores them",
    ),
    (
        ".zig-cache",
        Nothing,
        "cache: zig",
        "Zig build cache: rebuilt on the next build",
    ),
    (
        ".cache",
        Nothing,
        "cache",
        "caches of command-line tools: rebuilt on demand",
    ),
];

/// Last, so a tool's own rule wins.
const LIBRARY_CACHES: (&str, Needs, &str, &str) = (
    "Caches",
    Parent("Library"),
    "cache",
    "app caches: apps rebuild them, though some start slower once",
);

const CACHEDIR_TAG: Label = Label {
    tier: Tier::Cache,
    text: "cache",
    why: "the tool that made it marked it a cache (CACHEDIR.TAG): rebuilt on demand",
};

const SYSTEM: Label = Label {
    tier: Tier::System,
    text: "system",
    why: "macOS keeps it from being changed or deleted: protected by SIP",
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

/// Labels the folders of the trees of one root: holds what the rules by
/// path need, the root's real path and the home dir's.
#[derive(Clone, Debug)]
pub struct Labels {
    real: Option<PathBuf>,
    home: Option<PathBuf>,
}

impl Labels {
    /// For trees of `root`. `home` is the home dir the `~/` rules are
    /// below, if any.
    pub fn new(root: &Path, home: Option<&Path>) -> Self {
        Self {
            real: std::fs::canonicalize(root).ok(),
            home: home.and_then(|h| std::fs::canonicalize(h).ok()),
        }
    }

    /// The label of folder `id` of `tree`, if a rule matches.
    pub fn label(&self, tree: &impl ReadTree, id: u32) -> Option<Label> {
        let at = tree.path(id);
        let name = tree.name(id);
        // the root's name is its whole path
        let (above, parent) = match id {
            0 => (at.parent().map(Path::to_path_buf), None),
            _ => {
                let parent = tree.path(tree.record(id).parent);
                let name = parent.file_name().map(|n| n.as_bytes().to_vec());
                (Some(parent), name)
            }
        };
        let name = match id {
            0 => at.file_name().map_or(name, OsStrExt::as_bytes),
            _ => name,
        };
        let real = self.real.as_ref().map(|real| real.join(tree.relative(id)));
        let dir = Dir {
            name,
            parent: parent.as_deref().unwrap_or_default(),
            path: real.as_ref().map(|p| p.as_os_str().as_bytes()),
            home: self.home.as_ref().map(|h| h.as_os_str().as_bytes()),
        };
        let sibling = |file: &str| above.as_ref().is_some_and(|d| sys::exists(&d.join(file)));
        let inside = |file: &str| sys::exists(&at.join(file));
        let system = || sys::restricted(&at);
        let tagged = || sys::starts_with(&at.join("CACHEDIR.TAG"), CACHEDIR_SIGNATURE);
        label(&dir, sibling, inside, system, tagged)
    }
}

/// What [`label`] knows of a directory.
struct Dir<'a> {
    name: &'a [u8],
    /// The name of the directory it is in.
    parent: &'a [u8],
    /// Its real path, if known, and the home dir.
    path: Option<&'a [u8]>,
    home: Option<&'a [u8]>,
}

/// The label of `dir`, if a rule matches. `sibling` says whether its
/// parent holds a file of a name, `inside` whether it does, `system`
/// whether macOS protects it, and `tagged` whether it holds a valid
/// `CACHEDIR.TAG`.
fn label(
    dir: &Dir<'_>,
    sibling: impl Fn(&str) -> bool,
    inside: impl Fn(&str) -> bool,
    system: impl FnOnce() -> bool,
    tagged: impl FnOnce() -> bool,
) -> Option<Label> {
    if system() {
        return Some(SYSTEM);
    }
    let rules = CACHES.iter().chain([&LIBRARY_CACHES]);
    let mut named = rules.filter(|(n, ..)| n.as_bytes() == dir.name);
    let cache = named.find(|(_, needs, ..)| match *needs {
        Nothing => true,
        Sibling(files) => files.iter().any(|f| sibling(f)),
        Parent(name) => dir.parent == name.as_bytes(),
        Inside(file) => inside(file),
    });
    if let Some(&(_, _, text, why)) = cache {
        return Some(Label {
            tier: Tier::Cache,
            text,
            why,
        });
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
