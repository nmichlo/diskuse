
<p align="center">
    <h1 align="center">🧹 diskuse</h1>
    <p align="center">
        <i>See what is eating your disk. Fast, read-only, in your terminal.</i>
    </p>
</p>

<p align="center">
    <a href="https://crates.io/crates/diskuse"><img alt="crates.io" src="https://img.shields.io/crates/v/diskuse?style=flat-square&color=orange"/></a>
    <a href="https://pypi.org/project/diskuse"><img alt="pypi" src="https://img.shields.io/pypi/v/diskuse?style=flat-square&color=blue"/></a>
    <a href="https://github.com/nmichlo/diskuse/actions/workflows/test.yaml"><img alt="tests" src="https://img.shields.io/github/actions/workflow/status/nmichlo/diskuse/test.yaml?style=flat-square&label=tests"/></a>
    <a href="#license"><img alt="license" src="https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-lightgrey?style=flat-square"/></a>
</p>

```text
/Users/me  412.0 GiB  (+2.1 GiB in 12 min)
  41.2 GiB   +1.3 GiB ####...... Library/           38.2 GiB ########.. Developer/
  12.0 GiB            #......... Movies/             1.9 GiB #......... Caches/  [cache]
   8.4 GiB            #......... node_modules/  [cache: npm]
   3.4 GiB   -200 MiB #......... Downloads/  [downloads]
[cache: npm] npm install rebuilds it.  r reveal to delete  space pick
hjkl move  r reveal  o open  space pick  p picks  s/S rescan  / filter  t top  ? help  q quit
```

<br/>

## ⚡️ &nbsp;Quickstart

1. Run it, no install needed: `uvx diskuse`

2. Pick a volume, or go straight to a folder: `uvx diskuse ~`

3. Move with the arrows, `r` to reveal an item in Finder, `space` to pick it for later, `?` for every key

Install it for good with any of:

```sh
uv tool install diskuse      # or: pipx install diskuse
cargo install diskuse        # or: cargo binstall diskuse
```

Prebuilt for macOS and Linux, on arm64 and x86_64. Or download a `.tar.gz`
from [Releases](https://github.com/nmichlo/diskuse/releases).

<br/>

## 📋 &nbsp;Features

**Fast**
- As fast as the fastest tools (dua, dumac), faster than dust, pdu, gdu and ncdu, while building a tree you can browse ([benchmarks](#-benchmarks))
- Shows sizes as they grow, from the first second of the scan
- Low memory: about 300 bytes per directory, even for whole disks

**Safe**
- **Read-only**: it never deletes, moves or writes your files. You delete by hand, after `r` reveals the item in Finder
- Labels say what is safe to delete: `[cache: npm]` (rebuilt on demand), `[downloads]` (yours to judge), `[system]` (protected by macOS)
- Pick items with `space` as you browse, then review them with `p`

**Live**
- Follows changes on disk while it is open: sizes update about once a second
- A change column shows what grew or shrank since you opened it. `c` sorts by it

**Familiar**
- Three columns like OmniDiskSweeper: parent, current, preview
- Arrow keys or `hjkl`, mouse, filter with `/`, the largest files with `t`
- Works on macOS and Linux, local disks, network shares and external drives

<br/>

## ⌨️ &nbsp;Keys

| Key | Action | Key | Action |
| --- | --- | --- | --- |
| arrows, `hjkl` | move, enter, go back | `space` | pick or unpick |
| `r` | reveal in Finder or the file manager | `p` | list the picks and their total |
| `o` | open | `c` | sort by change, or by size again |
| `/` | filter the column | `s` / `S` | rescan this folder / everything |
| `t` | the largest files | `d` | folders that could not be read, and why |
| `?` | every key | `q` | quit |

<br/>

## 💻 &nbsp;Command Line

```sh
diskuse scan ~/src           # print sizes, largest first
diskuse scan ~/src --top 10  # and the 10 largest files
diskuse scan ~/src --json    # as JSON, for scripts
diskuse show ~/src           # print the last scan again, without scanning
```

```text
 225.4 MiB  /Users/me/src
 225.4 MiB  target/
  44.0 KiB  [files]
  36.0 KiB  src/
```

<br/>

## 🏁 &nbsp;Benchmarks

**diskuse ties the fastest tools (dua, dumac) and beats the rest, while building a tree you can browse.** Every tool's total matched `du`. Apple M4, 10 cores, 24 GB, macOS 27, warm cache, 3 timed runs each, so leads under about 6% are within noise.

**S1: the dumac README tree, 4,095 folders and 409,500 files.**

| tool | kind | time | memory |
| --- | --- | --- | --- |
| **diskuse** | tree | **0.342 s** | **7.7 MiB** |
| dua | tree | 0.350 s | 9.2 MiB |
| dumac | totals only | 0.446 s | - |

**A Homebrew prefix: 42,700 folders, 12 GiB.** "Best" is each tool at its fastest thread count.

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

Versions: dua 2.45.1, gdu 5.37.0, dust 1.2.6, pdu 0.24.0, ncdu 2.9.2, DiskScour 0.5.0, diskus 0.9.0, dumac 1ffbe3c3, du (macOS). How to reproduce: [bench/README.md](bench/README.md#reproducing-the-readme-numbers).

<br/>

## 📖 &nbsp;Details

<details>
<summary>How sizes are counted</summary>

- Sizes are allocated bytes, like `du`, not file lengths.
- A scan stays on one disk, like `du -x`, never follows symlinks, and counts hard links once.
- `[files]` is the files directly inside a folder.
- A folder that could not be read is marked `(denied: EACCES)`, and the sizes above it get a `+`: they are lower bounds.
- On macOS, scanning `/` covers your files once, through `/Users` and the other firmlinks, and skips the system's hidden volumes.
- After a scan of a whole volume, `not accounted for: 41.2 GiB` shows what the volume uses beyond what the scan found: on macOS mostly snapshots, purgeable space and denied folders.
- On macOS, `-r` adds a second size: the space deleting the item alone frees, while clones of its files (`cp -c`, Finder's Duplicate) elsewhere remain.

</details>

<details>
<summary>Every scan is fresh, and how live updates work</summary>

- A saved scan shows at once, marked `saved 5 min ago, rescanning...`, until the fresh scan is done. It is never trusted as current: Apple calls the macOS change history "advisory", so a disk changed by another OS or Mac could be wrong.
- While diskuse is open, macOS reports every change below the folder, including those made while it scanned. If macOS drops changes, diskuse rescans and the title says why.
- On Linux only the folders on screen follow changes (inotify). The title says how old the rest is. On network and FUSE filesystems, the folders on screen are listed again every 2 s instead.
- Ctrl-C, `q` or SIGTERM stops a scan, saves what it found, and marks it `(incomplete: scan stopped)`. The next scan starts over.

</details>

<details>
<summary>Labels: what is safe to delete</summary>

| Label | Colour | Means |
| --- | --- | --- |
| `[cache: <tool>]`, `[cache]` | green | a tool rebuilds it on demand |
| `[downloads]`, `[simulators]`, ... | yellow | a big folder macOS or an app keeps: clean it up from that app |
| `[system]` | red | macOS protects it: it cannot be deleted |

Caches it knows: `node_modules`, `bower_components`, `.next`, `.nuxt`,
`.svelte-kit`, `.turbo`, `.parcel-cache`, `.angular`, `.docusaurus`, `.expo`,
`target` (beside `Cargo.toml` or `pom.xml`), `.gradle`, `build` (beside
`build.gradle`), `__pycache__`, `.pytest_cache`, `.mypy_cache`, `.ruff_cache`,
`.tox`, `.nox`, `.ipynb_checkpoints`, `.venv` and `venv` (with `pyvenv.cfg`),
`DerivedData`, `Pods` (beside `Podfile`), `.build` (beside `Package.swift`),
`.dart_tool`, `_build` and `deps` (beside `mix.exs`), `.stack-work`,
`dist-newstyle`, `.terraform`, `.zig-cache`, `.cache`, `Library/Caches`, and
any folder holding a `CACHEDIR.TAG`.

Big folders it knows: the Trash, Downloads, Xcode archives and device
support, iOS simulators, iPhone backups, Docker Desktop, Mail, Messages,
Photos, the Android SDK, Rust toolchains, the cargo, npm and Go caches,
Ollama models, swap and temporary files. The full list is in
[src/labels.rs](src/labels.rs).

</details>

<details>
<summary>Full Disk Access (macOS)</summary>

macOS hides some folders (Mail, Messages, Safari, Time Machine) from apps
without Full Disk Access. diskuse checks once before the first scan, says
which terminal app needs it, and `o` opens the settings pane: turn on your
terminal in System Settings > Privacy & Security > Full Disk Access, then
restart it. `c` scans without it. diskuse never asks for `sudo`.

</details>

<details>
<summary>JSON output</summary>

`--json` prints the root folder as one JSON object, with subfolders nested
`--depth` levels deep (default 1). Keys always come in this order:

```text
{
  "name": "<path as given, for the root; else the folder name>",
  "size": <bytes, the folder and everything below it>,
  "reclaimable": <bytes>,        only with -r
  "own": <bytes, the folder itself and its files>,
  "denied": "EACCES" | "EPERM" | "errno N",   only if it could not be read
  "other_device": true,          only for a mount point, not entered
  "partial": true,               only if something below it was denied
  "name_lossy": true,            only if the name is not UTF-8
  "incomplete": true,            root only, if the scan was stopped
  "children": [ <node>, ... ],   largest first; absent at the depth limit
  "largest_files": [ {"path": "...", "size": <bytes>}, ... ]   only with --top N
}
```

</details>

<details>
<summary>Saved scans and privacy</summary>

Every scan is saved for `show` and for the instant start next time, with
your picks, in `~/Library/Caches/diskuse` on macOS and
`$XDG_CACHE_HOME/diskuse` (else `~/.cache/diskuse`) on Linux.
`DISKUSE_CACHE_DIR` overrides both. The files hold folder and file names,
so they are created owner-only (0600, folder 0700). This is the only place
diskuse ever writes. Delete the folder to clear it.

</details>

<br/>

## 🛠 &nbsp;Development

```sh
just check   # format, lint, test, cargo deny
just bench-gen S1 /tmp/S1 && just bench S1 /tmp/S1   # vs other tools, see bench/README.md
```

<br/>

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
