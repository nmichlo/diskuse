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
disksweep <path>
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

## Browse

`disksweep <path>` browses `<path>` full screen, in three columns: the parent
directory, the current one, and a preview of the selected item. Each lists
directories and files together, largest first.

```text
/opt/homebrew  12.0 GiB
  10.5 GiB  Cellar/                  1.4 GiB  llvm@20/                1.4 GiB  20.1.8/
 667.1 MiB  share/                 795.4 MiB  proj/
 439.5 MiB  Library/               490.3 MiB  go/
 132.1 MiB  .git/                  444.2 MiB  gcc/
```

It scans while you browse. The title says `scanning... at least <size>` and
the sizes grow, about once a second, until the scan is done and saved. If
`<path>` was scanned before, the saved scan shows at once, marked
`saved 5 min ago`, until the fresh scan is done.

| Key                  | Action                                                 |
| -------------------- | ------------------------------------------------------ |
| Up, Down, `k`, `j`   | Move                                                   |
| Right, Enter, `l`    | Go into the selected directory                         |
| Left, Backspace, `h` | Go to the parent directory, not above `<path>`         |
| `r`                  | Reveal the selected item in Finder or the file manager |
| `o`                  | Open the selected item                                 |
| `R`                  | Rescan all of `<path>`                                 |
| `/`                  | Filter the current column by text; Esc clears          |
| `q`, Esc             | Quit                                                   |

Reveal and open run `open -R` and `open` on macOS, and `xdg-open` on Linux,
which opens the folder of the item for reveal. Over SSH, or on Linux without
a display, they run nothing and show the full path instead. Only all of
`<path>` can be rescanned for now, not a single directory.

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
