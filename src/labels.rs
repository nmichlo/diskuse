//! Labels for directories that tools rebuild on demand, shown after the
//! name: `node_modules/  [cache: npm]`. Only a label: disksweep deletes
//! nothing, so deleting one is done by hand, after a reveal.
//!
//! Every rule is decided from the listing the directory is in, so labels
//! cost no syscalls, except the venv rules: one `lstat` each.

/// What a rule needs besides the directory's name.
enum Needs {
    Nothing,
    /// A file of this name next to it, in the same listing.
    Sibling(&'static str),
    /// The directory it is in has this name.
    Parent(&'static str),
    /// A file of this name inside it: one `lstat`.
    Inside(&'static str),
}

/// `(name, what else it needs, label)`. The README lists the same table.
const RULES: [(&str, Needs, &str); 9] = [
    ("node_modules", Needs::Nothing, "cache: npm"),
    ("target", Needs::Sibling("Cargo.toml"), "cache: cargo"),
    (".gradle", Needs::Nothing, "cache: gradle"),
    ("__pycache__", Needs::Nothing, "cache: python"),
    (".venv", Needs::Inside("pyvenv.cfg"), "cache: venv"),
    ("venv", Needs::Inside("pyvenv.cfg"), "cache: venv"),
    ("DerivedData", Needs::Nothing, "cache: xcode"),
    (".cache", Needs::Nothing, "cache"),
    ("Caches", Needs::Parent("Library"), "cache"),
];

/// The label of a directory named `name`, inside one named `parent`, if a
/// rule matches. `sibling` says whether `parent` holds a file of a name,
/// and `inside` whether the directory itself does.
pub(crate) fn label(
    name: &[u8],
    parent: &[u8],
    sibling: impl Fn(&str) -> bool,
    inside: impl Fn(&str) -> bool,
) -> Option<&'static str> {
    let (_, needs, label) = RULES.iter().find(|(n, ..)| n.as_bytes() == name)?;
    let matched = match *needs {
        Needs::Nothing => true,
        Needs::Sibling(file) => sibling(file),
        Needs::Parent(dir) => parent == dir.as_bytes(),
        Needs::Inside(file) => inside(file),
    };
    matched.then_some(*label)
}
