# disksweep

A read-only disk usage browser for macOS and Linux, built for speed.

## Read-only

disksweep never deletes, renames or writes your files. It can only show you
where the space went. Deleting is done by hand, in Finder or your file manager.

## Status

Pre-release. The only command so far:

```sh
disksweep scan <path> [--threads N]
```

It prints the total size of `<path>`, then each direct child largest first.
Sizes are allocated bytes, like `du`. It stays on one device (like `du -x`),
never follows symlinks, and counts hard links once.

```text
 225.4 MiB  .
 225.4 MiB  target/
  44.0 KiB  [files]
  36.0 KiB  src/
```

`[files]` is the files directly inside `<path>`. Directories it cannot read
are marked `(denied: EACCES)`, and their parents `(partial)`.

## Planned: cache

disksweep will cache scan results (including directory names) under your user
cache dir with file mode 0600; delete that dir to clear it.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
