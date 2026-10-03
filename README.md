# disksweep

A read-only disk usage browser for macOS and Linux, built for speed.

## Read-only

disksweep never deletes, renames or writes your files. It can only show you
where the space went. Deleting is done by hand, in Finder or your file manager.

## Install

Prebuilt for macOS and Linux, on x86_64 and arm64:

```sh
uvx disksweep scan .          # run once, without installing
uv tool install disksweep
pipx install disksweep
cargo binstall disksweep      # prebuilt, from GitHub Releases
cargo install disksweep       # builds from source
```

Or download a `.tar.gz` from
[GitHub Releases](https://github.com/nmichlo/disksweep/releases).

## Status

Pre-release. The commands so far:

```sh
disksweep [-r]
disksweep [-r] <path>
disksweep scan <path> [--threads N] [-r] [--json [--depth N]] [--top N]
disksweep show <path> [-r] [--json [--depth N]] [--top N]
```

`scan` prints the total size of `<path>`, then each direct child largest
first, and saves the result. Sizes are allocated bytes, like `du`. It stays on
one device (like `du -x`), never follows symlinks, and counts hard links once.
On macOS it also skips mount points that share the device id of `/`, like
`/System/Volumes/Data`, so scanning `/` counts the Data volume once, through
`/Users` and the other firmlinks.

```text
 225.4 MiB  .
 225.4 MiB  target/
  44.0 KiB  [files]
  36.0 KiB  src/
```

`[files]` is the files directly inside `<path>`. Directories it cannot read
are marked `(denied: EACCES)`, and their parents `(partial)`.

On macOS, `-r` adds a second column: reclaimable bytes, the space freed by
deleting that item alone, while clones of its files elsewhere remain. Copies
made with `cp -c` or Finder's Duplicate are clones. A directory's value sums
its files, so it leaves out blocks shared between two of its own files.

`--top N` (1 to 1000) adds the N largest files, by allocated bytes, after an
empty line:

```text
largest files:
   1.2 GiB  ./target/release/deps/libfoo.rlib
```

`show <path>` prints the last saved scan of `<path>` without scanning, exactly
as `scan` printed it with the same flags. `show -r` needs a scan made with
`-r`.

## Every scan is a full scan

A saved scan is never brought up to date from a record of changes. macOS
records every change to a volume (FSEvents), but Apple calls that record
"advisory rather than a definitive list of all changes to the volume": a disk
changed by another OS, from Recovery or by another Mac can miss changes with
no sign. So `scan`, and every launch of the browser, scans afresh. Changes
seen while disksweep is open are applied live (see [Browse](#browse)), as
the kernel reports them and says when it drops any.

## Stop

Ctrl-C stops a `scan`, and so does SIGTERM. In the browser, `q`, Esc and
Ctrl-C stop the scan when quitting. Either way disksweep lists no more
directories, saves what it found, and marks the result incomplete:

```text
 182.4 GiB  /  (incomplete: scan stopped)
```

`scan` prints that and exits with 130 after Ctrl-C, 143 after SIGTERM.
`show` prints it the same way. With `--json`, the root has
`"incomplete": true`. The next scan starts over.

## Browse

`disksweep` with no path lists the volumes, one per row:

```text
volumes
 800.6 GiB used of  926.4 GiB   125.7 GiB free  #########.  /
  10.0 GiB used of   64.0 GiB    54.0 GiB free  ##........  /Volumes/USB
```

Enter browses the selected volume. Esc, or Backspace at its top, goes back to
the list. Hidden and pseudo filesystems are left out: on macOS every
`nobrowse` mount, like `/dev` and the system's helper volumes; on Linux every
mount of size 0, like `/proc`, and `tmpfs`, `devtmpfs`, `overlay`, `squashfs`
and `efivarfs`. `/` is always listed. On macOS it stands for the whole main
disk: a scan of `/` also covers the hidden Data volume, where your files are,
and the used bytes of an APFS volume are those of its whole container.

`disksweep <path>` browses `<path>` full screen, in three columns: the parent
directory, the current one, and a preview of the selected item. At the top of
`<path>` there is no parent, so the current directory takes the first column.
Each lists directories and files together, largest first. In a column at
least 40 cells wide, a bar shows each item's share of its directory, in
tenths, rounded, and at least one `#` from 1%:

```text
/opt/homebrew  12.0 GiB
  10.5 GiB  #########. Cellar/                 1.4 GiB  #......... llvm@20/
 667.1 MiB  #......... share/                795.4 MiB  #......... proj/
 439.5 MiB  #......... Library/              490.3 MiB  #......... go/
```

Sizes are coloured by magnitude, like OmniDiskSweeper: red from 1 GiB, yellow
from 1 MiB, green from 1 KiB, grey below. Directories are bold. With
[`NO_COLOR`](https://no-color.org) set, nothing is coloured; the selection is
reversed instead.

It scans while you browse. The title says `scanning... at least <size>` and
the sizes grow, about once a second, until the scan is done and saved. If
`<path>` was scanned before, the saved scan shows at once, marked
`saved 5 min ago, rescanning...`, until the fresh scan is done. Then, while
disksweep is open, sizes follow changes on disk, about once a second: on
macOS every change below `<path>`, including those made while it scanned.

On Linux only the directories on screen follow changes: those
of the three columns. inotify reports their changes, so they show within
about a second. Changes anywhere else show after a rescan (`s` or `S`), so the title
says how old the scan is:

```text
/home/me  41.2 GiB  scanned 4 min ago
```

A column after the size shows how much each item grew (red) or shrank
(green) since the first scan of the session finished, and the title shows
the same for `<path>`, with since when. The column appears once something
in it changed. Rescans keep counting from that first scan. A file's change
is known once its directory has been shown; a directory's always is. `c`
sorts by change, to find what grew, and again by size:

```text
/Users/me  412.0 GiB  (+2.1 GiB in 12 min)
  41.2 GiB   +1.3 GiB ####...... Library/
  12.0 GiB            #......... Movies/
   3.4 GiB   -200 MiB #......... Downloads/
```

On network filesystems inotify misses the changes other machines make, and
on FUSE those made behind the kernel's back. On these, a directory on screen
is listed again every 2 s instead, or if longer, after 10 times as long as
the last listing took, so listing takes at most a tenth of a core: NFS,
SMB/CIFS, FUSE (sshfs, rclone, virtiofs VM shares), 9p, Ceph, AFS, Coda,
Lustre, GPFS, GFS2, OCFS2 and OrangeFS. The same goes for a directory inotify
cannot watch, as once `fs.inotify.max_user_watches` is reached. On macOS, a volume
macOS records no changes for, like a network share, is followed the same way.

### Keys

| Key                  | Action                                                     |
| -------------------- | ---------------------------------------------------------- |
| Up, Down, `k`, `j`   | Move                                                       |
| PageUp, PageDown     | Move by a screen                                           |
| Home, End, `g`, `G`  | Go to the first or the last row                            |
| Right, Enter, `l`    | Go into the selected directory                             |
| Left, Backspace, `h` | Go to the parent directory, not above `<path>`             |
| `r`                  | Reveal the selected item in Finder or the file manager     |
| `o`                  | Open the selected item                                     |
| Space                | Pick or unpick the selected item, marked `*`               |
| `p`                  | List the picks, with their sizes now and a total           |
| `c`                  | Sort by change this session, or by size again              |
| `s`                  | Rescan the selected directory (at a file, the current one) |
| `S`                  | Rescan all of `<path>`                                     |
| `/`                  | Filter the current column by text; Enter keeps, Esc clears |
| `d`                  | List the directories that could not be read, and why       |
| `t`                  | Show or hide the largest files under `<path>`              |
| `?`                  | Show or hide every key                                     |
| `q`, Esc             | Quit; a running scan stops and saves what it found         |
| Click                | Select; in the parent or preview column, go there too      |
| Double click         | Go into the directory, or scan the volume                  |
| Wheel                | Move the cursor, or scroll the parent or preview column    |

The move keys, the mouse and `?` work the same on the volume list, the `d`
list, the `t` list and the `p` list.

Picks collect what to delete by hand later, since disksweep deletes
nothing. In the `p` list, `r` and `o` reveal and open a pick, and Space
unpicks it. Picks are saved per scanned path, next to the saved scan, so
they are there next time; one deleted since shows as `gone`:

```text
3 picked, 41.5 GiB in all:
  38.2 GiB  Library/Developer/Xcode/DerivedData
   3.3 GiB  Downloads/old.dmg
      gone  Movies/export.mov
``` disksweep takes the mouse, so to select text on screen,
hold Shift (Option in Terminal and iTerm) while dragging.

Reveal and open run `open -R` and `open` on macOS, and `xdg-open` on Linux,
which opens the folder of the item for reveal. Over SSH, or on Linux without
a display, they run nothing and show the full path instead.

A directory that could not be read is marked `(denied: EACCES)`, and every
directory above it has a `+` after its size, since the size is then a lower
bound:

```text
  36.0 KiB+ a/
   4.0 KiB  locked/ (denied: EACCES)
```

`t` lists the largest files the scan found under `<path>`, up to 1000, one
per row with its path below `<path>`. Up and Down move, and `r` and `o`
reveal and open the selected file. Each file is checked with one `lstat`
when first shown, and left out if it is gone. Closing and reopening the list
checks again. During a scan it lists the largest files found so far.

```text
   1.2 GiB  target/release/deps/libfoo.rlib
 640.0 MiB  .git/objects/pack/pack-1a2b.pack
```

On macOS, `disksweep -r` scans with reclaimable sizes, like `scan -r`, and
shows them in a second column after each size. A saved scan made without
`-r` is not shown first in this mode; the fresh scan is.

```text
   1.0 MiB        0 B  clone/
   1.0 MiB    1.0 MiB  solo/
```

Directories get a label after the name, coloured by how safe deleting them
is, and the footer explains the label of the selected one:

```text
 412.3 MiB  node_modules/  [cache: npm]       green: a tool rebuilds it
  38.2 GiB  CoreSimulator/  [simulators]      yellow: clean up from its app
  12.1 GiB  System/  [system]                 red: macOS protects it
```

| Tier   | Colour | When                                                                 |
| ------ | ------ | -------------------------------------------------------------------- |
| system | red    | macOS protects it (System Integrity Protection's `restricted` flag)  |
| cache  | green  | a rule below matches, or it holds a `CACHEDIR.TAG` (cargo, pip, ...) |
| known  | yellow | a big folder macOS or a common app keeps, listed in `src/labels.rs`  |

| Directory                                   | Label                       | Only when                                  |
| ------------------------------------------- | --------------------------- | ------------------------------------------ |
| `node_modules`, `bower_components`          | `cache: npm`, `cache: bower` |                                           |
| `.next`, `.nuxt`, `.svelte-kit`, `.turbo`, `.parcel-cache`, `.angular`, `.docusaurus`, `.expo` | `cache: <tool>` |      |
| `target`                                    | `cache: cargo` or `maven`   | a `Cargo.toml` or `pom.xml` is next to it  |
| `.gradle`; `build`                          | `cache: gradle`             | `build`: a `build.gradle(.kts)` beside it  |
| `__pycache__`, `.pytest_cache`, `.mypy_cache`, `.ruff_cache`, `.tox`, `.nox`, `.ipynb_checkpoints` | `cache: <tool>` | |
| `.venv`, `venv`                             | `cache: venv`               | it holds a `pyvenv.cfg`                    |
| `DerivedData`                               | `cache: xcode`              |                                            |
| `Pods`; `.build`                            | `cache: cocoapods`, `swiftpm` | a `Podfile` or `Package.swift` beside it |
| `.dart_tool`, `.stack-work`, `dist-newstyle`, `.terraform`, `.zig-cache` | `cache: <tool>` |               |
| `_build`, `deps`                            | `cache: elixir`             | a `mix.exs` is next to it                  |
| `.cache`                                    | `cache`                     |                                            |
| `Caches`                                    | `cache`                     | it is in a directory named `Library`       |

The footer explains the selected directory's label and says what to do:
`[cache: npm] npm install rebuilds it.  r reveal to delete  space pick`.

Known folders include the Trash, Downloads, Xcode archives and device
support, iOS simulators, iPhone backups, Docker Desktop's disk image, Mail,
Messages, the Photos library, the Android SDK, Rust toolchains, the cargo,
npm and Go module caches, Ollama models, swap and per-user temporary files.

A label never deletes anything. To delete a labelled directory, reveal it
with `r` and delete it by hand. Labels are worked out only for the
directories on screen, so they cost the scan nothing: one `lstat` for the
system flag, and one more for the venv rule or one small read for
`CACHEDIR.TAG`.

Once a scan of a volume's root is done, a line under the columns says how
many of the volume's used bytes the scan did not find, if any:
`not accounted for: 41.2 GiB`. On macOS these are mostly snapshots,
purgeable space, the system's hidden volumes and denied directories.

## Full Disk Access

macOS keeps some folders from every app without Full Disk Access: Mail,
Messages, Safari, Time Machine and other apps' data. A scan without it lists
them as denied, with `needs Full Disk Access for <your terminal>` in the `d`
list. `EACCES` instead means plain Unix permissions; `sudo` can read those.

Before its first scan of a volume, or of a path that holds or is in your home
directory, disksweep reads `~/Desktop`, `~/Documents` and `~/Downloads` once,
so any macOS popups asking for them appear together, before the scan. Then it
tries a folder only Full Disk Access can read. If that fails, it says which
terminal app needs access, and `o` opens the settings pane. To grant it, turn
on your terminal app in System Settings > Privacy & Security > Full Disk
Access, then quit and reopen it. `c` scans without it. disksweep never asks
for `sudo`.

## JSON

`--json` prints one JSON object on one line: the root directory, with
subdirectories nested `--depth` levels deep (default 1, 0 for the root only).
Files are not listed; their bytes are in `own`.

```text
{
  "name": "<path as given, for the root; else the directory name>",
  "size": <bytes, the directory and everything below it>,
  "reclaimable": <bytes>,        only with -r
  "own": <bytes, the directory itself and its files>,
  "denied": "EACCES" | "EPERM" | "errno N",   only if it could not be read
  "other_device": true,          only for a mount point, not descended into
  "partial": true,               only if something below it was denied
  "name_lossy": true,            only if the name is not UTF-8 and was
                                 converted with U+FFFD
  "incomplete": true,            root only, only if the scan was stopped
                                 before it was done
  "children": [ <node>, ... ],   largest first, ties by name; absent at the
                                 depth limit
  "largest_files": [ {"path": "...", "size": <bytes>}, ... ]
                                 root only, only with --top N
}
```

Keys always come in this order.

## Cache

Every `scan` saves its result, which `show` reads. Saved scans live in:

| OS    | Directory                                                    |
| ----- | ------------------------------------------------------------ |
| macOS | `~/Library/Caches/disksweep`                                 |
| Linux | `$XDG_CACHE_HOME/disksweep`, else `~/.cache/disksweep`       |

`DISKSWEEP_CACHE_DIR` overrides both. There is one file per scanned path (per
volume and real path), and one of its picks if any. The files contain
directory names, the names of the largest files and picked paths, so they are created with mode 0600, and the directory, if
missing, with mode 0700. To clear the cache, delete the directory. This cache
directory is the only place disksweep ever writes.

`show` reads a saved file in place, memory-mapped, without copying it. Each
file ends in a CRC-32 of its bytes. A file that fails that check, or comes from
another disksweep version, counts as no saved scan, and the next `scan`
replaces it.

## Benchmarks

**disksweep had the fastest mean in both runs below, and builds a browsable tree while doing it.** These are quick runs (3 timed runs each), so its lead over dua and dumac (2-6%) is within the noise; the gap to every other tool is clear. Every tool's total matched `du`.

Apple M4, 10 cores, 24 GB, macOS 27, warm cache, 2026-10-02. How to reproduce: [bench/README.md](bench/README.md#reproducing-the-readme-numbers).

**S1: the dumac README tree, 4,095 dirs and 409,500 files.**

| tool | kind | warm time | peak memory |
| --- | --- | --- | --- |
| **disksweep** | tree | **0.342 s** | **7.7 MiB** |
| dua | tree | 0.350 s | 9.2 MiB |
| dumac | totals only | 0.446 s | - |

**A Homebrew prefix: 42,700 dirs, 12 GiB.** "Best" is each tool at its fastest thread count.

| tool | kind | warm, default threads | warm, best | peak memory |
| --- | --- | --- | --- | --- |
| **disksweep** | tree | **0.610 s** | **0.610 s** | 13.8 MiB |
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

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
