"""Prefix facts and the caller policy from the core signature API."""

from itertools import product


BOX_SETS = ((), (b"ftyp", b"moof"), (b"wide", b"\0foo", b"uuid"))


def grid(prefix, spacing, skip):
    """The three sync locations in H.222.0 §2.4.3.2 and the M2TS carrier."""
    end = skip + 2 * spacing + 1
    cells = prefix[skip:end:spacing]
    state = 1 if any(byte != 0x47 for byte in cells) else 2 if len(cells) == 3 else 0
    return end, state


def verdict(size, ebml, page, box, boxes, allow_ogg, tie, grids):
    """Apply the documented selection order to independent prefix observations."""
    if ebml:
        return 3, 0
    if box in boxes:
        return 4, 0
    if allow_ogg and page:
        return 5, 0
    if size < 4:
        return 0, 4
    if size < 8 and any(kind.startswith(box) for kind in boxes):
        return 0, 8

    (ts_end, ts), (m2ts_end, m2ts) = grids
    if tie:
        if size < m2ts_end:
            return 0, m2ts_end
        return {(2, 1): 6, (1, 2): 7, (2, 2): 2, (1, 1): 1}[ts, m2ts], 0
    if ts == 0:
        return 0, ts_end
    if ts == 2:
        return 6, 0
    return ((0, m2ts_end), (1, 0), (7, 0))[m2ts]


def trace(file):
    data = file.read_bytes()
    result = []
    for size in range(len(data) + 1):
        prefix = data[:size]
        ebml = int(prefix[:4] == bytes.fromhex("1a45dfa3"))  # RFC 8794 §11.2.1
        page = int(prefix[:5] == b"OggS\0")  # RFC 3533 §6
        box = prefix[4:8]  # ISO/IEC 14496-12 §4.2
        grids = (grid(prefix, 188, 0), grid(prefix, 192, 4))
        ts, m2ts = grids
        row = f"3 {size} {ebml} {page} {ts[0]} {ts[1]} {m2ts[0]} {m2ts[1]}"
        result.append(f"{row} {box.hex()}" if len(box) == 4 else row)
        for (index, boxes), allow_ogg, tie in product(enumerate(BOX_SETS), (0, 1), (0, 1)):
            kind, need = verdict(size, ebml, page, box, boxes, allow_ogg, tie, grids)
            result.append(f"4 {index} {allow_ogg} {tie} {kind} {need}")
    return result
