"""A tiny disk usage browser in pure Python, on diskuse's public API only.

    uv run python python/examples/tui.py ~/src

Up/Down move, Right/Enter go in, Left/Backspace go back, q quits. Sizes fill
in while it scans, then follow changes on disk (macOS). The change column is
the change since the scan finished. Files come from `os.scandir`, which is
fast enough for one folder; diskuse only keeps folder totals.

An example, not part of the package: curses only, no dependencies.
"""

import curses
import os
import sys
import threading

import diskuse


def human(n):
    sign, n = ("-" if n < 0 else ""), abs(n)
    if n < 1024:
        return f"{sign}{n} B"
    for unit in ["KiB", "MiB", "GiB", "TiB"]:
        n /= 1024
        if n < 1024 or unit == "TiB":
            return f"{sign}{n:.1f} {unit}"


class State:
    """The latest tree, from the live scan's thread."""

    def __init__(self):
        self.lock = threading.Lock()
        self.tree = None
        self.ready = None  # the tree when the scan finished
        self.status = "scanning..."


def follow(path, state, stop):
    with diskuse.live(path, interval=0.25) as live:
        for e in live:
            if stop.is_set():
                return
            with state.lock:
                if isinstance(e, diskuse.Rescanning):
                    state.status = f"rescanning ({e.reason})"
                    continue
                state.tree = e.tree
                if isinstance(e, diskuse.Ready):
                    state.ready, state.status = e.tree, ""
                elif isinstance(e, diskuse.Scanning):
                    state.status = "scanning..."


def rows(state, below):
    """(size, change, name, is_dir) of the folder `below`, largest first."""
    d = state.tree.find(below) if state.tree else None
    if d is None:
        return []
    out = []
    for k in d.children():
        was = state.ready.find(os.path.join(below, k.name)) if state.ready else None
        out.append((k.size, k.size - was.size if was else 0, k.name, True))
    try:
        with os.scandir(d.path) as it:
            for f in it:
                if not f.is_dir(follow_symlinks=False):
                    size = f.stat(follow_symlinks=False).st_blocks * 512
                    out.append((size, 0, f.name, False))
    except OSError:
        pass
    return sorted(out, key=lambda r: (-r[0], r[2]))


def main(screen, path):
    curses.curs_set(0)
    screen.timeout(250)
    state, stop = State(), threading.Event()
    threading.Thread(target=follow, args=(path, state, stop), daemon=True).start()
    trail, cursor = [], 0
    while True:
        below = os.path.join(*trail) if trail else "."
        with state.lock:
            items = rows(state, below)
            total = (
                state.tree.find(below).size
                if state.tree and state.tree.find(below)
                else 0
            )
            status = state.status
        height, width = screen.getmaxyx()
        cursor = max(0, min(cursor, len(items) - 1))
        screen.erase()
        title = f"{os.path.join(path, *trail)}  {human(total)}  {status}"
        screen.addnstr(0, 0, title, width - 1, curses.A_BOLD)
        top = max(0, cursor - (height - 3))
        for y, (size, change, name, is_dir) in enumerate(
            items[top : top + height - 2], 1
        ):
            delta = f"{'+' if change > 0 else ''}{human(change)}" if change else ""
            line = f"{human(size):>11} {delta:>11}  {name}{'/' if is_dir else ''}"
            attr = curses.A_REVERSE if top + y - 1 == cursor else 0
            screen.addnstr(
                y, 0, line, width - 1, attr | (curses.A_BOLD if is_dir else 0)
            )
        screen.addnstr(
            height - 1, 0, "arrows move  enter in  backspace out  q quit", width - 1
        )
        key = screen.getch()
        if key in (ord("q"), 27):
            stop.set()
            return
        if key in (curses.KEY_UP, ord("k")):
            cursor -= 1
        elif key in (curses.KEY_DOWN, ord("j")):
            cursor += 1
        elif key in (curses.KEY_RIGHT, curses.KEY_ENTER, 10, ord("l")) and items:
            if items[cursor][3]:
                trail.append(items[cursor][2])
                cursor = 0
        elif key in (curses.KEY_LEFT, curses.KEY_BACKSPACE, 127, ord("h")) and trail:
            trail.pop()
            cursor = 0


if __name__ == "__main__":
    curses.wrapper(main, os.path.expanduser(sys.argv[1] if len(sys.argv) > 1 else "."))
