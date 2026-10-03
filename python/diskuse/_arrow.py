"""Builds `Tree.to_arrow()` from raw column buffers, with no Python object
per folder. Row `i` is folder id `i`; `parent` is null for the root."""

import pyarrow as pa
import pyarrow.compute as pc

# Record flags, from src/tree.rs
_DENIED, _OTHER_DEVICE, _PARTIAL, _REMOVED = 1, 2, 4, 8


def table(columns):
    c = dict(columns)
    n = c["n"]

    def prim(kind, buf, valid=None):
        return pa.Array.from_buffers(kind, n, [valid, pa.py_buffer(buf)])

    names = pa.Array.from_buffers(
        pa.large_string(),
        len(c["offsets"]) // 8 - 1,
        [None, pa.py_buffer(c["offsets"]), pa.py_buffer(c["names"])],
    )
    flags = prim(pa.uint16(), c["flags"])

    def bit(b):
        return pc.not_equal(pc.bit_wise_and(flags, pa.scalar(b, pa.uint16())), 0)

    t = pa.table(
        {
            "id": prim(pa.uint32(), c["id"]),
            "parent": prim(pa.uint32(), c["parent"], pa.py_buffer(c["valid"])),
            "name": pa.DictionaryArray.from_arrays(prim(pa.uint32(), c["name"]), names),
            "size": prim(pa.uint64(), c["size"]),
            "own": prim(pa.uint64(), c["own"]),
            "denied": bit(_DENIED),
            "partial": bit(_PARTIAL),
            "other_device": bit(_OTHER_DEVICE),
        }
    )
    # folders gone since the scan, in a tree that followed changes
    return t.filter(pc.invert(bit(_REMOVED)))
