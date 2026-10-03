"""A fast, read-only disk usage scanner.

tree = diskuse.scan("~/src")
tree.root.size                      # allocated bytes, like du
for d in tree.root.children():      # largest first
    print(d.size, d.name)
tree.largest_files(10)              # [(Path, bytes), ...]
tree.to_arrow()                     # every folder as a row (needs pyarrow)
"""

import asyncio
import os
import sys
from dataclasses import dataclass
from pathlib import Path

from ._diskuse import Dir, Tree, _Live
from ._diskuse import main as _main
from ._diskuse import scan as _scan

__all__ = [
    "Changed",
    "Dir",
    "Live",
    "Ready",
    "Rescanning",
    "Scanning",
    "Tree",
    "live",
    "scan",
]


def scan(path, threads=None):
    """Scans `path` (`~` expanded) and returns its `Tree`.

    It stays on one disk, never follows symlinks, and counts hard links
    once. `threads` fixes the thread count; by default it adapts. The GIL
    is released while it runs.
    """
    return _scan(os.path.expanduser(os.fspath(path)), threads)


def _cli():
    """The `diskuse` console script."""
    sys.exit(_main(sys.argv))


@dataclass(frozen=True)
class Scanning:
    """The tree scanned so far: every size is a lower bound."""

    tree: Tree


@dataclass(frozen=True)
class Ready:
    """The scan is done."""

    tree: Tree


@dataclass(frozen=True)
class Changed:
    """The disk changed. `changes` holds each folder whose own bytes changed,
    or whole subtree appeared or went, with by how many bytes."""

    tree: Tree
    changes: "list[tuple[Path, int]]"


@dataclass(frozen=True)
class Rescanning:
    """Changes were lost, so the tree is scanned again."""

    reason: str


_EVENTS = {
    "scanning": Scanning,
    "ready": Ready,
    "changed": Changed,
    "rescanning": Rescanning,
}


class Live:
    """Events of a live scan: `Scanning` every `interval` seconds, `Ready`,
    then `Changed` as the disk changes (macOS), or `Rescanning`.

        for e in diskuse.live("~"):           # or: async for e in ...
            match e:
                case diskuse.Ready(tree): ...
                case diskuse.Changed(tree, changes): ...

    Leaving the loop stops the scan, as does `close()` or a `with` block.
    """

    def __init__(self, path, interval=0.5, threads=None):
        self._live = _Live(os.path.expanduser(os.fspath(path)), threads)
        self._interval = interval

    def _event(self, raw):
        return _EVENTS[raw[0]](*raw[1:])

    def __iter__(self):
        try:
            while True:
                raw = self._live.next(self._interval)
                if raw == ("closed",):
                    return
                if raw is not None:
                    yield self._event(raw)
        finally:
            self.close()

    async def __aiter__(self):
        try:
            while True:
                raw = await asyncio.to_thread(self._live.next, self._interval)
                if raw == ("closed",):
                    return
                if raw is not None:
                    yield self._event(raw)
        finally:
            self.close()

    def close(self):
        self._live.close()

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        self.close()


def live(path, interval=0.5, threads=None):
    """Scans `path` and keeps following it: see `Live`."""
    return Live(path, interval, threads)
