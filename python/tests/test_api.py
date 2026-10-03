"""The Python API against a fixture whose sizes come from the filesystem,
so the expectations hold on APFS (0-byte dirs) and ext4 (4 KiB dirs)."""

import os
import subprocess
import sys
from pathlib import Path

import diskuse
import pyarrow as pa
import pytest


def blocks(path):
    return os.lstat(path).st_blocks * 512


@pytest.fixture
def root(tmp_path):
    """
    root/
      a/f       4096
      a/b/g     8192
      c/        empty
      top       1000
    """
    for d in ["a/b", "c"]:
        (tmp_path / d).mkdir(parents=True)
    for f, n in [("a/f", 4096), ("a/b/g", 8192), ("top", 1000)]:
        (tmp_path / f).write_bytes(b"x" * n)
    return tmp_path


def test_scan_gives_every_folders_total(root):
    t = diskuse.scan(root)
    b = blocks(root / "a/b") + blocks(root / "a/b/g")
    a = blocks(root / "a") + blocks(root / "a/f") + b
    c = blocks(root / "c")
    own = blocks(root) + blocks(root / "top")

    def summary(d):
        kids = [summary(k) for k in d.children()]
        return (d.name, d.size, d.own, d.error, d.partial, d.other_device, kids)

    assert summary(t.root) == (
        str(root),
        own + a + c,
        own,
        None,
        False,
        False,
        [
            ("a", a, a - b, None, False, False, [("b", b, b, None, False, False, [])]),
            ("c", c, c, None, False, False, []),
        ],
    )
    assert (t.root.path, t.find("a/b").path) == (root, root / "a/b")
    assert [t.find(p).size for p in ["a/b", root / "a", "."]] == [b, a, t.root.size]
    assert t.find("nope") is None


def test_largest_files(root):
    t = diskuse.scan(root)
    files = [root / "a/b/g", root / "a/f", root / "top"]
    expected = sorted(((f, blocks(f)) for f in files), key=lambda x: (-x[1], str(x[0])))
    assert t.largest_files() == expected
    assert t.largest_files(1) == expected[:1]


@pytest.mark.skipif(os.geteuid() == 0, reason="root reads every folder")
def test_a_folder_that_cannot_be_read(root):
    (root / "c").chmod(0)
    try:
        t = diskuse.scan(root)
        c = t.find("c")
        assert (c.error, c.size, t.root.partial, t.find("a").partial) == (
            "EACCES",
            blocks(root / "c"),
            True,
            False,
        )
    finally:
        (root / "c").chmod(0o755)


def test_to_arrow_has_a_row_per_folder(root):
    t = diskuse.scan(root)
    table = t.to_arrow()

    def rows(d, parent):
        yield {
            "parent": parent,
            "name": d.name,
            "size": d.size,
            "own": d.own,
            "denied": d.error is not None,
            "partial": d.partial,
            "other_device": d.other_device,
        }
        for k in d.children():
            yield from rows(k, d.name)

    names = table.column("name").to_pylist()
    got = [
        {
            **{k: v for k, v in r.items() if k != "id"},
            "parent": None if r["parent"] is None else names[r["parent"]],
        }
        for r in table.to_pylist()
    ]
    key = lambda r: r["name"]
    assert sorted(got, key=key) == sorted(rows(t.root, None), key=key)
    assert table.schema.field("name").type == pa.dictionary(
        pa.uint32(), pa.large_string()
    )
    assert table.column("id").to_pylist() == list(range(len(table)))


def test_the_console_script_is_the_cli(root):
    script = Path(sys.executable).parent / "diskuse"
    got = subprocess.run(
        [script, "scan", root], capture_output=True, text=True, check=True
    )
    want = subprocess.run(
        ["cargo", "run", "-q", "--bin", "diskuse", "--", "scan", root],
        capture_output=True,
        text=True,
        check=True,
    )
    assert got.stdout == want.stdout


def until_ready(events):
    """The events up to and including `Ready`, by kind."""
    kinds = []
    for e in events:
        kinds.append(type(e).__name__)
        if isinstance(e, diskuse.Ready):
            return kinds, e.tree
    raise AssertionError("no Ready")


@pytest.mark.skipif(sys.platform != "darwin", reason="only macOS reports changes")
def test_live_reports_ready_then_each_change(root):
    with diskuse.live(root, interval=0.2) as live:
        events = iter(live)
        kinds, tree = until_ready(events)
        assert (kinds[-1], tree.root.size) == ("Ready", diskuse.scan(root).root.size)
        (root / "a/new").write_bytes(b"x" * 4096)
        change = next(e for e in events if isinstance(e, diskuse.Changed))
        assert change.changes == [(root / "a", blocks(root / "a/new"))]
        assert change.tree.find("a").size == tree.find("a").size + blocks(
            root / "a/new"
        )


@pytest.mark.skipif(sys.platform != "darwin", reason="only macOS reports changes")
def test_live_async(root):
    async def run():
        async for e in diskuse.live(root, interval=0.2):
            if isinstance(e, diskuse.Ready):
                (root / "c/new").write_bytes(b"x" * 8192)
            if isinstance(e, diskuse.Changed):
                return e.changes

    import asyncio

    assert asyncio.run(run()) == [(root / "c", blocks(root / "c/new"))]


def test_live_ends_once_closed(root):
    live = diskuse.live(root, interval=0.1)
    kinds, _ = until_ready(live)
    live.close()
    assert (kinds[-1], list(live)) == ("Ready", [])
