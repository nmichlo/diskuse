"""A fast, read-only disk usage scanner. Folders are ids, the root is 0.

tree = diskuse.scan("~/src")
for k in sorted(tree.children(0), key=tree.size, reverse=True):
    print(tree.size(k), tree.name(k))
pyarrow.table(tree)                 # every folder as a row

for event in diskuse.live("~/src"):  # or: async for
    match event:
        case diskuse.Event.Changed(tree, changes): ...
"""

from collections.abc import AsyncIterator
from collections.abc import Iterator
from os import PathLike
from pathlib import Path
from typing import ClassVar
from typing import final

def scan(path: str | PathLike[str], threads: int | None = None) -> Tree:
    """Scans `path` (`~` expanded) with the GIL released.

    It stays on one disk, never follows symlinks, and counts hard links
    once. `threads` fixes the thread count; by default it adapts.
    """

def live(
    path: str | PathLike[str],
    interval: float = 0.5,
    threads: int | None = None,
    shown_only: bool = False,
) -> Live:
    """Scans `path` and keeps following it: see `Event`.

    `interval` is how often, in seconds, a snapshot comes while scanning,
    and changes after. `shown_only`: on Linux, follow only the folders given
    to `Live.follow`, not every folder. Leaving the loop stops it.
    """

@final
class Tree:
    """A scanned tree. Every method takes a folder id; the root is 0."""

    def name(self, id: int) -> str:
        """Its name; for the root, the path as scanned."""
    def path(self, id: int) -> Path: ...
    def size(self, id: int) -> int:
        """Allocated bytes of it and everything below it."""
    def own(self, id: int) -> int:
        """Allocated bytes of the folder itself and its files."""
    def error(self, id: int) -> str | None:
        """Why it could not be read (`EACCES`, `EPERM`, `errno N`)."""
    def partial(self, id: int) -> bool:
        """Something below it could not be read: `size` is a lower bound."""
    def other_device(self, id: int) -> bool:
        """A mount point, not scanned into."""
    def children(self, id: int) -> list[int]:
        """Its subfolders, in no particular order."""
    def find(self, path: str | PathLike[str]) -> int | None:
        """The folder at `path`, relative to the root or absolute below it."""
    def files(self, id: int) -> list[tuple[str, int]]:
        """The files of folder `id` and their allocated bytes, listed from
        disk now, as the tree keeps only folder totals. None for a folder on
        another device."""
    def largest_files(self, n: int = 100) -> list[tuple[Path, int]]:
        """The `n` largest files, largest first. Only 1000 are kept."""
    def stopped(self) -> bool:
        """The scan was stopped early, so sizes are lower bounds."""
    def __arrow_c_stream__(self, requested_schema: object | None = None) -> object:
        """Every folder as a row: id, parent, name, size, own, denied,
        partial, other_device. For `pyarrow.table(tree)`, polars, duckdb."""

@final
class Reason:
    """Why a live scan scans the whole tree again. `str()` says it in words."""

    Dropped: ClassVar[Reason]
    """The OS dropped change events."""
    IdsWrapped: ClassVar[Reason]
    """macOS's change ids wrapped around."""
    RootMoved: ClassVar[Reason]
    """The scanned folder itself moved."""
    NoReplay: ClassVar[Reason]
    """macOS did not replay the changes made while scanning within 30 s."""
    MustScanAll: ClassVar[Reason]
    """macOS asked for the whole tree to be scanned again."""

class Event:
    """What a live scan reports."""

    @final
    class Scanning(Event):
        """The tree scanned so far: every size is a lower bound."""

        __match_args__ = ("tree",)
        tree: Tree
        def __init__(self, tree: Tree) -> None: ...

    @final
    class Ready(Event):
        """The scan is done."""

        __match_args__ = ("tree",)
        tree: Tree
        def __init__(self, tree: Tree) -> None: ...

    @final
    class Changed(Event):
        """The tree changed: each folder whose own bytes changed, or whole
        subtree appeared or went, with by how many bytes."""

        __match_args__ = ("tree", "changes")
        tree: Tree
        changes: list[tuple[Path, int]]
        def __init__(self, tree: Tree, changes: list[tuple[Path, int]]) -> None: ...

    @final
    class Rescanning(Event):
        """Changes were lost, so the tree is scanned again."""

        __match_args__ = ("reason",)
        reason: Reason
        def __init__(self, reason: Reason) -> None: ...

@final
class Live:
    """A live scan: iterate it, sync or async."""

    def __iter__(self) -> Iterator[Event]: ...
    def __next__(self) -> Event: ...
    def __aiter__(self) -> AsyncIterator[Event]: ...
    async def __anext__(self) -> Event: ...
    def follows_all(self) -> bool:
        """Every change below the root is followed, not only those in the
        folders given to `follow`."""
    def follow(self, ids: list[int]) -> None:
        """With `shown_only`, or once inotify watches run out (Linux),
        follows only folders `ids` of the latest tree."""
    def rescan(self, id: int) -> None:
        """Scans folder `id` again, then reports `Changed`; 0 rescans all."""
