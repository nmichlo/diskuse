"""A fast, read-only disk usage scanner.

    tree = diskuse.scan("~/src")
    tree.root.size                      # allocated bytes, like du
    for d in tree.root.children():      # largest first
        print(d.size, d.name)
    tree.largest_files(10)              # [(Path, bytes), ...]
    tree.to_arrow()                     # every folder as a row (needs pyarrow)
"""

import os
import sys

from ._diskuse import Dir, Tree
from ._diskuse import main as _main
from ._diskuse import scan as _scan

__all__ = ["Dir", "Tree", "scan"]


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
