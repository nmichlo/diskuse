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
disksweep scan <path> [--full] [--threads N] [-r] [--json [--depth N]] [--top N]
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

## Incremental scans

On macOS, `scan` starts from the saved scan of `<path>`, if there is one. macOS
records every change to a volume (FSEvents), so disksweep asks for the changes
below `<path>` since the saved scan, lists only the directories they touched
again, and scans any new subdirectories. A file that grows in place counts as
a change too. The output is the same as a full scan's, and the result is saved
again.

```text
disksweep scan ~/src          # the first time: a full scan
disksweep scan ~/src          # later: only what changed since
disksweep scan ~/src --full   # always a full scan
```

It falls back to a full scan when:

- there is no saved scan, or it is damaged or from another disksweep version;
- the saved scan was made with `-r` and this one is not, or the other way;
- macOS no longer has the changes since then, lost some, or its record was
  reset, as after erasing the disk;
- `<path>` itself was moved or deleted, or macOS says to scan all of it again.

If macOS says to rescan only a directory below `<path>`, only that directory
is scanned again. Changes inside the cache directory are ignored.

The largest files list (`--top`, `t`) keeps the 1000 largest files from the
full scan, plus those of the directories listed again. A full scan finds the
others again.

On Linux every `scan` is a full scan.

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
`saved 5 min ago`, until the fresh scan is done. On macOS that saved scan is
brought up to date like an [incremental scan](#incremental-scans), usually in
under a second, instead of scanning again. Then, while disksweep is open,
sizes follow changes on disk, about once a second.

Linux keeps no record of changes, so there the saved scan shows until a full
scan is done, and then only the directories on screen follow changes: those
of the three columns. inotify reports their changes, so they show within
about a second. Changes anywhere else show after a rescan (`R`), so the title
says how old the scan is:

```text
/home/me  41.2 GiB  scanned 4 min ago
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
| `R`                  | Rescan all of `<path>`                                     |
| `/`                  | Filter the current column by text; Enter keeps, Esc clears |
| `d`                  | List the directories that could not be read, and why       |
| `t`                  | Show or hide the largest files under `<path>`              |
| `?`                  | Show or hide every key                                     |
| `q`, Esc             | Quit                                                       |
| Click                | Select; in the parent or preview column, go there too      |
| Double click         | Go into the directory, or scan the volume                  |
| Wheel                | Move the cursor, or scroll the parent or preview column    |

The move keys, the mouse and `?` work the same on the volume list, the `d`
list and the `t` list. disksweep takes the mouse, so to select text on screen,
hold Shift (Option in Terminal and iTerm) while dragging.

Reveal and open run `open -R` and `open` on macOS, and `xdg-open` on Linux,
which opens the folder of the item for reveal. Over SSH, or on Linux without
a display, they run nothing and show the full path instead. Only all of
`<path>` can be rescanned for now, not a single directory.

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

Directories that tools rebuild on demand get a label after the name:

```text
 412.3 MiB  node_modules/  [cache: npm]
```

| Directory            | Label           | Only when                               |
| -------------------- | --------------- | --------------------------------------- |
| `node_modules`       | `cache: npm`    |                                         |
| `target`             | `cache: cargo`  | a `Cargo.toml` is next to it            |
| `.gradle`            | `cache: gradle` |                                         |
| `__pycache__`        | `cache: python` |                                         |
| `.venv`, `venv`      | `cache: venv`   | it holds a `pyvenv.cfg`                 |
| `DerivedData`        | `cache: xcode`  |                                         |
| `.cache`             | `cache`         |                                         |
| `Caches`             | `cache`         | it is in a directory named `Library`    |

A label never deletes anything. To delete a labelled directory, reveal it
with `r` and delete it by hand. Labels come from the listing a directory is
in, so they cost no extra reads, except the venv rule: one `lstat` of
`pyvenv.cfg` per venv shown.

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
volume and real path). The files contain directory names and the names of the
largest files, so they are created with mode 0600, and the directory, if
missing, with mode 0700. To clear the cache, delete the directory. This cache
directory is the only place disksweep ever writes.

## Benchmarks

Results land before 0.1.0. The method is in
[bench/README.md](https://github.com/nmichlo/disksweep/blob/main/bench/README.md).

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
