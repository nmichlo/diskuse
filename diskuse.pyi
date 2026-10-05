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
    to `Live.follow`, not every folder. `close()` it, or use `with`.
    """

def load(path: str | PathLike[str]) -> Tree:
    """The scan `Tree.save` wrote to `path` (`~` expanded).

    `ValueError` if the file is not one, or is of another version of
    diskuse. The tree names its root by its real path.
    """

def mounts() -> list[Mount]:
    """Every mounted filesystem, with its sizes."""

@final
class Mount:
    """A mounted filesystem, as the OS lists it."""

    point: Path
    fs: str
    """The filesystem type: `apfs`, `ext4`, `proc`..."""
    hidden: bool
    """Hidden from Finder (macOS `nobrowse`). Never set on Linux."""
    total: int
    """Bytes in all."""
    used: int
    """Bytes in use. On APFS, of the whole container, all its volumes."""
    free: int
    """Bytes free for users other than root."""

@final
class Tier:
    """How safe deleting a labelled folder is."""

    System: ClassVar[Tier]
    """macOS protects it: do not delete."""
    Cache: ClassVar[Tier]
    """A tool rebuilds it on demand."""
    Known: ClassVar[Tier]
    """A big folder macOS or an app keeps, to clean up from that app."""

@final
class Label:
    """What a folder is. diskuse deletes nothing: this only says."""

    tier: Tier
    text: str
    """In a word or two: `cache: npm`."""
    why: str
    """What it is and how to clean it up: `npm install rebuilds it`."""

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
    def label(self, id: int) -> Label | None:
        """What folder `id` is, if a rule knows it: a cache a tool rebuilds,
        a folder macOS protects, a big folder an app keeps. By its name and
        a look at the disk now, so for the folders someone looks at, not
        for every folder of a tree."""
    def save(self, path: str | PathLike[str]) -> None:
        """Saves the scan as the file `path` (`~` expanded), over any file
        there, for `diskuse.load`. Only the owner can read it."""
    def largest_files(self, n: int = 100) -> list[tuple[Path, int]]:
        """The `n` largest files, largest first. Only 1000 are kept."""
    def stopped(self) -> bool:
        """The scan was stopped early, so sizes are lower bounds."""
    def __arrow_c_stream__(self, requested_schema: object | None = None) -> object:
        """Every folder as a row: id, parent, name, size, own, denied,
        partial, other_device. For `pyarrow.table(tree)`, polars, duckdb."""

@final
class Reason:
    """Why changes were missed. `str()` says it in words."""

    Dropped: ClassVar[Reason]
    """The OS dropped change events."""
    IdsWrapped: ClassVar[Reason]
    """macOS's change ids wrapped around."""
    RootMoved: ClassVar[Reason]
    """The scanned folder itself moved."""
    NoReplay: ClassVar[Reason]
    """macOS did not replay the changes made while scanning within 30 s."""
    MustScan: ClassVar[Reason]
    """The OS merged the changes below a folder into "scan it again"."""

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
    class Missed(Event):
        """Changes at or below `path` were missed, so the tree may be out of
        date there until `Live.rescan`. Nothing is scanned again by itself."""

        __match_args__ = ("reason", "path")
        reason: Reason
        path: Path
        def __init__(self, reason: Reason, path: Path) -> None: ...

@final
class Live:
    """A live scan: iterate it, sync or async, then `close()` it, or use it
    as a context manager."""

    def __enter__(self) -> Live: ...
    def __exit__(self, *args: object) -> None: ...
    def close(self) -> None:
        """Stops the scan and ends every loop over it."""

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
    def relist(self, ids: list[int]) -> None:
        """Lists folders `ids` of the latest tree again, and reports what
        changed in them: for folders looked at after `Missed`."""
    def rescan(self, id: int) -> None:
        """Scans folder `id` again, then reports `Changed`; 0 rescans all."""
