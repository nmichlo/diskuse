from os import PathLike
from pathlib import Path
from typing import TypeAlias

import pyarrow

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

def scan(path: str | PathLike, threads: int | None = None) -> Tree: ...

class Tree:
    @property
    def root(self) -> Dir: ...
    def find(self, path: str | PathLike) -> Dir | None: ...
    def largest_files(self, n: int = 100) -> list[tuple[Path, int]]: ...
    def to_arrow(self) -> pyarrow.Table: ...

class Dir:
    @property
    def name(self) -> str: ...
    @property
    def path(self) -> Path: ...
    @property
    def size(self) -> int: ...
    @property
    def own(self) -> int: ...
    @property
    def error(self) -> str | None: ...
    @property
    def partial(self) -> bool: ...
    @property
    def other_device(self) -> bool: ...
    def children(self) -> list[Dir]: ...

class Scanning:
    tree: Tree

class Ready:
    tree: Tree

class Changed:
    tree: Tree
    changes: list[tuple[Path, int]]

class Rescanning:
    reason: str

Event: TypeAlias = Scanning | Ready | Changed | Rescanning

class Live:
    def __init__(
        self,
        path: str | PathLike,
        interval: float = 0.5,
        threads: Optional[int] = None,
    ) -> None: ...
    def __iter__(self) -> Iterator[Event]: ...
    def __aiter__(self) -> AsyncIterator[Event]: ...
    def close(self) -> None: ...
    def __enter__(self) -> Live: ...
    def __exit__(self, *exc: object) -> None: ...

def live(
    path: str | PathLike, interval: float = 0.5, threads: Optional[int] = None
) -> Live: ...
