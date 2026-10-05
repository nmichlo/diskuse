"""The Python API against a fixture whose sizes come from the filesystem,
so the expectations hold on APFS (0-byte dirs) and ext4 (4 KiB dirs)."""

import asyncio
import os
import re
import subprocess
import sys
import time
from pathlib import Path

import pyarrow as pa
import pytest

import diskuse


def blocks(path: Path) -> int:
    return os.lstat(path).st_blocks * 512


@pytest.fixture
def root(tmp_path: Path) -> Path:
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


Summary = tuple[str, Path, int, int, "str | None", bool, bool, list["Summary"]]


def summary(t: diskuse.Tree, k: int) -> Summary:
    """Folder `k` and everything below it, children by name."""
    kids = sorted(t.children(k), key=t.name)
    return (
        t.name(k),
        t.path(k),
        t.size(k),
        t.own(k),
        t.error(k),
        t.partial(k),
        t.other_device(k),
        [summary(t, c) for c in kids],
    )


def test_scan_gives_every_folders_total(root: Path) -> None:
    t = diskuse.scan(root)
    b = blocks(root / "a/b") + blocks(root / "a/b/g")
    a = blocks(root / "a") + blocks(root / "a/f") + b
    c = blocks(root / "c")
    own = blocks(root) + blocks(root / "top")
    assert summary(t, 0) == (
        str(root),
        root,
        own + a + c,
        own,
        None,
        False,
        False,
        [
            ("a", root / "a", a, a - b, None, False, False, [("b", root / "a/b", b, b, None, False, False, [])]),
            ("c", root / "c", c, c, None, False, False, []),
        ],
    )
    ab = t.find("a/b")
    assert ab is not None
    assert [t.find(p) for p in ["a/b", root / "a/b", ".", "nope"]] == [ab, ab, 0, None]
    assert t.stopped() is False


def test_a_folder_id_out_of_range_is_an_index_error(root: Path) -> None:
    t = diskuse.scan(root)
    with pytest.raises(IndexError, match="^no folder 99$"):
        t.size(99)


def test_files_and_largest_files(root: Path) -> None:
    t = diskuse.scan(root)
    files = [root / "a/b/g", root / "a/f", root / "top"]
    expected = sorted(((f, blocks(f)) for f in files), key=lambda x: (-x[1], str(x[0])))
    assert t.largest_files() == expected
    assert t.largest_files(1) == expected[:1]
    a = t.find("a")
    assert a is not None
    assert (t.files(0), t.files(a)) == ([("top", blocks(root / "top"))], [("f", blocks(root / "a/f"))])


def test_a_saved_scan_loads_as_it_was(root: Path, tmp_path_factory: pytest.TempPathFactory) -> None:
    # a loaded tree names its root by its real path
    t = diskuse.scan(root.resolve())
    file = tmp_path_factory.mktemp("saved") / "scan"
    t.save(file)
    loaded = diskuse.load(file)
    assert (summary(loaded, 0), loaded.largest_files(), loaded.stopped()) == (summary(t, 0), t.largest_files(), False)


def test_loading_what_is_no_saved_scan(tmp_path: Path) -> None:
    file = tmp_path / "notes"
    file.write_bytes(b"not a scan")
    with pytest.raises(ValueError, match=f"^{re.escape(str(file))}: not a scan saved by this version of diskuse$"):
        diskuse.load(file)
    with pytest.raises(FileNotFoundError):
        diskuse.load(tmp_path / "nope")


def test_mounts_has_the_root_filesystem() -> None:
    st = os.statvfs("/")
    at_root = [(m.point, m.total, m.hidden) for m in diskuse.mounts() if m.point == Path("/")]
    assert at_root == [(Path("/"), st.f_blocks * st.f_frsize, False)]


def test_label_says_what_a_folder_is(root: Path) -> None:
    (root / "node_modules").mkdir()
    t = diskuse.scan(root)
    modules, a = t.find("node_modules"), t.find("a")
    assert modules is not None and a is not None
    label = t.label(modules)
    assert label is not None
    assert ((label.tier, label.text, label.why), t.label(a)) == (
        (diskuse.Tier.Cache, "cache: npm", "npm install rebuilds it"),
        None,
    )


def test_a_missing_root_is_file_not_found(tmp_path: Path) -> None:
    with pytest.raises(FileNotFoundError):
        diskuse.scan(tmp_path / "nope")


@pytest.mark.skipif(os.geteuid() == 0, reason="root reads every folder")
def test_a_folder_that_cannot_be_read(root: Path) -> None:
    (root / "c").chmod(0)
    try:
        t = diskuse.scan(root)
        a, c = t.find("a"), t.find("c")
        assert a is not None and c is not None
        assert (t.error(c), t.size(c), t.partial(0), t.partial(a)) == (
            "EACCES",
            blocks(root / "c"),
            True,
            False,
        )
    finally:
        (root / "c").chmod(0o755)


def test_arrow_has_a_row_per_folder(root: Path) -> None:
    t = diskuse.scan(root)
    table = pa.table(t)
    ids = [0, *(k for c in t.children(0) for k in [c, *t.children(c)])]
    parent = {k: p for p in ids for k in t.children(p)}
    expected = [
        {
            "id": k,
            "parent": parent.get(k),
            "name": t.name(k),
            "size": t.size(k),
            "own": t.own(k),
            "denied": False,
            "partial": False,
            "other_device": False,
        }
        for k in sorted(ids)
    ]
    assert table.to_pylist() == expected
    assert table.schema.field("name").type == pa.dictionary(pa.uint32(), pa.large_string())


def test_the_console_script_is_the_cli(root: Path) -> None:
    script = Path(sys.executable).parent / "diskuse"
    got = subprocess.run([script, "scan", root], capture_output=True, text=True, check=True)
    want = subprocess.run(
        ["cargo", "run", "-q", "-p", "diskuse", "--bin", "diskuse", "--", "scan", root],
        capture_output=True,
        text=True,
        check=True,
    )
    assert got.stdout == want.stdout


def until_ready(live: diskuse.Live) -> tuple[list[str], diskuse.Tree]:
    """The events up to and including `Ready`, by kind, and its tree."""
    kinds = []
    for e in live:
        kinds.append(type(e).__name__)
        if isinstance(e, diskuse.Event.Ready):
            return kinds, e.tree
    raise AssertionError("no Ready")


def test_live_reports_ready_then_each_change(root: Path) -> None:
    live = diskuse.live(root, interval=0.2)
    kinds, tree = until_ready(live)
    assert (kinds[-1], tree.size(0), live.follows_all()) == ("Ready", diskuse.scan(root).size(0), True)
    (root / "a/new").write_bytes(b"x" * 4096)
    change = next(e for e in live if isinstance(e, diskuse.Event.Changed))
    assert change.changes == [(root / "a", blocks(root / "a/new"))]
    assert summary(change.tree, 0) == summary(diskuse.scan(root), 0)


def test_live_async(root: Path) -> None:
    async def run() -> list[tuple[Path, int]]:
        live = diskuse.live(root, interval=0.2)
        async for e in live:
            match e:
                case diskuse.Event.Ready(_):
                    (root / "c/new").write_bytes(b"x" * 8192)
                case diskuse.Event.Changed(_, changes):
                    return changes
        raise AssertionError("ended")

    assert asyncio.run(run()) == [(root / "c", blocks(root / "c/new"))]


def test_cancelling_an_async_wait_stops_it_at_once(root: Path) -> None:
    async def run() -> None:
        live = diskuse.live(root, interval=0.2)
        async for e in live:
            if isinstance(e, diskuse.Event.Ready):
                break
        # nothing changes, so the wait is cancelled, and nothing is left waiting
        with pytest.raises(TimeoutError):
            await asyncio.wait_for(anext(live), 0.3)

    start = time.monotonic()
    asyncio.run(run())
    assert time.monotonic() - start < 2


def test_closing_ends_the_loops(root: Path) -> None:
    with diskuse.live(root, interval=0.2) as live:
        until_ready(live)
    assert list(live) == []

    async def run() -> str:
        live = diskuse.live(root, interval=0.2)
        async for e in live:
            if isinstance(e, diskuse.Event.Ready):
                live.close()
        return "ended"

    assert asyncio.run(run()) == "ended"


def test_a_rescan_reports_the_folder_again(root: Path) -> None:
    live = diskuse.live(root, interval=0.2)
    _, tree = until_ready(live)
    c = tree.find("c")
    assert c is not None
    (root / "c/new").write_bytes(b"x" * 8192)
    live.rescan(c)
    change = next(e for e in live if isinstance(e, diskuse.Event.Changed))
    assert change.changes == [(root / "c", blocks(root / "c/new"))]
