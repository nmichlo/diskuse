# /// script
# requires-python = ">=3.10"
# dependencies = ["diskuse", "textual>=6"]
#
# [tool.uv.sources]
# diskuse = { path = "../.." }
# ///
"""A small disk usage browser on diskuse's public API, as an example.

    uv run python/examples/tui.py ~/src

Enter goes into a folder, Backspace goes back, s scans the folder at the
cursor again, q quits. Sizes fill in while it scans, then follow changes on
disk. The change column is the change since the scan finished. Files come
from `os.scandir`: diskuse only keeps folder totals.
"""

import os
import sys
from pathlib import Path

from textual import work
from textual.app import App
from textual.app import ComposeResult
from textual.widgets import DataTable
from textual.widgets import Footer
from textual.widgets import Header

import diskuse


def human(n: int) -> str:
    sign, size = ("-" if n < 0 else ""), float(abs(n))
    for unit in ["B", "KiB", "MiB", "GiB"]:
        if size < 1024:
            return f"{sign}{size:.0f} {unit}" if unit == "B" else f"{sign}{size:.1f} {unit}"
        size /= 1024
    return f"{sign}{size:.1f} TiB"


class Browse(App[None]):
    BINDINGS = [("q", "quit", "quit"), ("backspace", "back", "back"), ("s", "rescan", "rescan")]

    def __init__(self, path: str) -> None:
        super().__init__()
        self.live = diskuse.live(path, interval=0.25)
        self.latest: diskuse.Tree | None = None
        # the tree when the scan finished, for the change column
        self.ready: diskuse.Tree | None = None
        self.trail: list[str] = []
        self.status = "scanning..."

    def compose(self) -> ComposeResult:
        yield Header()
        yield DataTable(cursor_type="row")
        yield Footer()

    def on_mount(self) -> None:
        self.query_one(DataTable).add_columns("size", "change", "name")
        self.follow()

    @work()
    async def follow(self) -> None:
        async for event in self.live:
            match event:
                case diskuse.Event.Scanning(tree):
                    self.latest, self.status = tree, "scanning..."
                case diskuse.Event.Ready(tree):
                    self.latest = self.ready = tree
                    self.status = ""
                case diskuse.Event.Changed(tree, _):
                    self.latest = tree
                case diskuse.Event.Rescanning(reason):
                    self.status = f"rescanning ({reason})"
            self.show()

    def here(self) -> int | None:
        return self.latest.find(Path(*self.trail)) if self.latest else None

    def show(self) -> None:
        tree, d = self.latest, self.here()
        if tree is None or d is None:
            return
        rows = []
        for k in tree.children(d):
            was = self.ready.find(Path(*self.trail, tree.name(k))) if self.ready else None
            change = tree.size(k) - self.ready.size(was) if self.ready and was is not None else 0
            rows.append((tree.size(k), change, tree.name(k) + "/"))
        try:
            with os.scandir(tree.path(d)) as entries:
                for f in entries:
                    if not f.is_dir(follow_symlinks=False):
                        rows.append((f.stat(follow_symlinks=False).st_blocks * 512, 0, f.name))
        except OSError:
            pass
        table = self.query_one(DataTable)
        at = table.cursor_row
        table.clear()
        for size, change, name in sorted(rows, key=lambda r: (-r[0], r[2])):
            table.add_row(human(size), f"{'+' if change > 0 else ''}{human(change)}" if change else "", name, key=name)
        table.move_cursor(row=at)
        self.title = f"{tree.path(d)}  {human(tree.size(d))}  {self.status}"
        # on Linux, only the folders followed report changes
        self.live.follow([d])

    def selected(self) -> str | None:
        table = self.query_one(DataTable)
        if table.row_count == 0:
            return None
        return table.coordinate_to_cell_key(table.cursor_coordinate).row_key.value

    def on_data_table_row_selected(self) -> None:
        name = self.selected()
        if name and name.endswith("/"):
            self.trail.append(name[:-1])
            self.query_one(DataTable).move_cursor(row=0)
            self.show()

    def action_back(self) -> None:
        if self.trail:
            self.trail.pop()
            self.query_one(DataTable).move_cursor(row=0)
            self.show()

    def action_rescan(self) -> None:
        name, tree = self.selected(), self.latest
        if name and name.endswith("/") and tree:
            k = tree.find(Path(*self.trail, name[:-1]))
            if k is not None:
                self.live.rescan(k)


if __name__ == "__main__":
    Browse(os.path.expanduser(sys.argv[1] if len(sys.argv) > 1 else ".")).run()
