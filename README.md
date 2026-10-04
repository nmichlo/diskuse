
<p align="center">
    <h1 align="center">🧹 diskuse</h1>
    <p align="center">
        <i>A fast, read-only disk usage browser for macOS and Linux,<br/>inspired by OmniDiskSweeper</i>
    </p>
</p>

<p align="center">
    <a href="https://crates.io/crates/diskuse"><img alt="crates.io" src="https://img.shields.io/crates/v/diskuse?style=flat-square&color=orange"/></a>
    <a href="https://pypi.org/project/diskuse"><img alt="pypi" src="https://img.shields.io/pypi/v/diskuse?style=flat-square&color=blue"/></a>
    <a href="https://github.com/nmichlo/diskuse/actions/workflows/test.yaml"><img alt="tests" src="https://img.shields.io/github/actions/workflow/status/nmichlo/diskuse/test.yaml?style=flat-square&label=tests"/></a>
    <a href="#license"><img alt="license" src="https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-lightgrey?style=flat-square"/></a>
</p>

<p align="center">
    <img src="https://raw.githubusercontent.com/nmichlo/diskuse/main/demo/demo.gif" alt="diskuse browsing a home folder" width="900"/>
</p>

<br/>

## ⚡️ &nbsp;Quickstart

1. Run diskuse without installing it using `uvx diskuse`

2. Or browse a specific folder with `uvx diskuse ~`

3. Press `?` to see all the keys

To install it instead, use `uv tool install diskuse`, `pipx install diskuse` or `cargo install diskuse`.
Prebuilt binaries for macOS and Linux are also on the [releases](https://github.com/nmichlo/diskuse/releases) page.

<br/>

## 📋 &nbsp;Features

- As fast as dua and dumac, and faster than dust, pdu, gdu and ncdu (see [benchmarks](#-benchmarks))
- Read-only, it never deletes anything. Reveal an item in Finder with `r` and delete it yourself
- Three columns like OmniDiskSweeper, with sizes filling in while it scans, and how far a whole-disk scan has got
- Updates live while open, with a column showing what grew or shrank
- Labels for caches and other large folders, eg. `node_modules`, `target`, `.venv`, Xcode, Docker
- Pick items with `space` to come back to later
- Works on macOS and Linux

<br/>

## ⌨️ &nbsp;Keys

| Key | Action | Key | Action |
| --- | --- | --- | --- |
| arrows, `hjkl` | move | `space` | pick an item |
| `r` | reveal in Finder | `p` | list picked items |
| `o` | open | `c` | sort by change |
| `/` | filter | `s` / `S` | rescan folder / everything |
| `t` | largest files | `d` | folders that could not be read |
| `i` | everything about the selected item | `u` | sizes in GiB or GB |
| `?` | help | `q` | quit |

<br/>

## 💻 &nbsp;Command Line

```sh
diskuse scan ~/src           # print sizes, largest first
diskuse scan ~/src --top 10  # also list the 10 largest files
diskuse scan ~/src --json    # json output for scripts
diskuse show ~/src           # print the last scan again
diskuse scan ~/src --si      # sizes in kB, MB, GB (powers of 1000)
```

Sizes are allocated bytes like `du -x`. Scans stay on one disk, never follow symlinks, and count hard links once.

<br/>

## 🦀 &nbsp;Rust

The scanner is a library too (`cargo add diskuse-core`). Folders are ids of one tree, and the root is 0:

```rust
use diskuse_core::{Event, LiveOptions, ReadTree, ScanOptions};

let tree = diskuse_core::scan(path, &ScanOptions::default())?;
for &k in tree.children(0) {
    println!("{} {}", tree.size(k), tree.path(k).display()); // allocated bytes, like du
}
tree.find(Path::new("a/b")); // Some(id)
tree.largest_files(10); // [(path, bytes), ...]
tree.files(0, false)?; // the root's files, listed from disk now

for event in diskuse_core::live(path, LiveOptions::default()) {
    match event? {
        Event::Ready(tree) => {} // the scan finished
        Event::Changed(tree, changes) => {} // [(path, delta_bytes), ...]
        _ => {}
    }
}
```

<br/>

## 🐍 &nbsp;Python

The same API from Python (`pip install diskuse`):

```python
import diskuse

tree = diskuse.scan("~/src")
for k in tree.children(0):
    print(tree.size(k), tree.path(k))  # allocated bytes, like du
tree.find("a/b")  # id or None
tree.largest_files(10)  # [(path, bytes), ...]
tree.files(0)  # the root's files, listed from disk now
pyarrow.table(tree)  # every folder as a row, also polars and duckdb

for event in diskuse.live("~/src"):  # or: async for
    match event:
        case diskuse.Event.Ready(tree):
            ...  # the scan finished
        case diskuse.Event.Changed(tree, changes):
            ...  # [(path, delta_bytes), ...]
```

See [crates/diskuse-python/examples/tui.py](crates/diskuse-python/examples/tui.py) for a small browser written in Python with this API.

<br/>

## 🔐 &nbsp;Full Disk Access (macOS)

macOS hides some folders like Mail, Messages and Safari from apps without Full Disk Access.
diskuse checks this before your first scan and tells you which terminal app needs it.
To enable it, turn on your terminal in System Settings > Privacy & Security > Full Disk Access, then restart the terminal.
diskuse never asks for `sudo`.

<br/>

## 🏁 &nbsp;Benchmarks

Warm cache on an Apple M4 (10 cores, 24 GB, macOS 27), 3 timed runs each, so differences under about 6% are noise.
Every tool's total matched `du`. See [bench/README.md](bench/README.md#reproducing-the-readme-numbers) to reproduce.

**S1:** the dumac README tree, 4,095 folders and 409,500 files

| tool | kind | time | memory |
| --- | --- | --- | --- |
| **diskuse** | tree | **0.342 s** | **7.7 MiB** |
| dua | tree | 0.350 s | 9.2 MiB |
| dumac | totals only | 0.446 s | - |

**Homebrew:** a homebrew prefix, 42,700 folders and 12 GiB. "Best" is each tool at its fastest thread count.

| tool | kind | default threads | best | memory |
| --- | --- | --- | --- | --- |
| **diskuse** | tree | **0.610 s** | **0.610 s** | 13.8 MiB |
| dumac | totals only | 0.636 s | 0.636 s | 31.8 MiB |
| dua | tree | 0.722 s | 0.647 s | **12.7 MiB** |
| diskus | totals only | 0.997 s | 0.950 s | 20.2 MiB |
| dust | tree | 1.086 s | 1.086 s | 156.7 MiB |
| pdu | tree | 1.087 s | 0.989 s | 36.5 MiB |
| gdu | tree | 1.844 s | 1.177 s | 41.9 MiB |
| ncdu | totals only (`-o -`) | 2.709 s | 1.030 s | 2.3 MiB |
| du | totals only | 2.703 s | 2.703 s | 5.5 MiB |
| DiskScour | tree | 3.581 s | 3.581 s | 248.8 MiB |

Versions: dua 2.45.1, gdu 5.37.0, dust 1.2.6, pdu 0.24.0, ncdu 2.9.2, DiskScour 0.5.0, diskus 0.9.0, dumac 1ffbe3c3, du (macOS).

<br/>

## 🛠 &nbsp;Development

Run `just check` to format, lint and test, and `just demo` to re-record the demo above. Benchmarks are run with `just bench`, see [bench/README.md](bench/README.md).

Scans are cached in `~/Library/Caches/diskuse` (or `~/.cache/diskuse` on Linux), which can be changed with `DISKUSE_CACHE_DIR`.
This is the only place diskuse ever writes to.

<br/>

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT) at your option.

<br/>

## 🤖 &nbsp;AI Use

diskuse was written with the help of an AI coding assistant (Claude Code).
I designed the features, made the decisions, and reviewed and tested every change.
The benchmarks are real runs on real machines, see [bench/README.md](bench/README.md).
