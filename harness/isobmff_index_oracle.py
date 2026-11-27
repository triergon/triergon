"""ISO BMFF index traces from ETSI TS 26.244 §13.4 and F4V §§2.16.1–2.16.2."""

import struct


def _box(data, offset, size):
    if offset + size > len(data) or size < 8:
        raise ValueError("incomplete box")
    declared, kind = struct.unpack_from(">I4s", data, offset)
    header = 8
    if declared == 1:
        declared, = struct.unpack_from(">Q", data[offset + 8:offset + 16])
        header = 16
    if declared != size or size < header:
        raise ValueError("invalid box extent")
    return kind, data[offset + header:offset + size]


def _row(kind, *values):
    return " ".join(str(value) for value in (kind, *values))


def _segment(body, version, end):
    track, scale = struct.unpack_from(">II", body, 4)
    layout = ">IIHH" if version == 0 else ">QQHH"
    tick, first, reserved, count = struct.unpack_from(layout, body, 12)
    records = body[12 + struct.calcsize(layout):]
    if not track or not scale or reserved or len(records) != count * 12:
        raise ValueError("invalid sidx header or rows")
    start = end + first
    if start >= 1 << 64:
        raise ValueError("first offset overflows")
    yield _row(2, version, track, scale, tick, first, count)
    for ordinal, (size, duration, sap) in enumerate(struct.iter_unpack(">III", records), 1):
        limit = start + (size % (1 << 31))
        if limit >= 1 << 64:
            raise ValueError("reference range overflows")
        yield _row(3, ordinal, size >> 31, start, limit, *divmod(tick, 1 << 64),
                   duration, sap >> 31, (sap >> 28) % 8, sap % (1 << 28), size, sap)
        start = limit
        tick += duration


def _random_access(body, version):
    track, descriptor, count = struct.unpack_from(">III", body, 4)
    widths = [1 + (descriptor >> shift) % 4 for shift in (4, 2, 0)]
    coordinates = 4 if version == 0 else 8
    stride = coordinates * 2 + sum(widths)
    records = body[16:]
    if not track or descriptor >= 64 or len(records) != count * stride:
        raise ValueError("invalid tfra header or rows")
    yield _row(2, version, track, count, *widths)
    for ordinal, at in enumerate(range(0, len(records), stride), 1):
        values = []
        for width in [coordinates, coordinates, *widths]:
            values.append(int.from_bytes(records[at:at + width], "big"))
            at += width
        if not all(values[2:]):
            raise ValueError("zero tfra coordinate")
        yield _row(3, ordinal, *values)


def _tail(data, body, version, offset, length):
    size, = struct.unpack(">I", body[4:])
    end = offset + length
    if end != len(data) or not length + 8 <= size <= end:
        raise ValueError("invalid mfro parent range")
    start = end - size
    yield _row(2, version, size, start, end)
    kind, _ = _box(data, start, size)
    if kind != b"mfra":
        raise ValueError("mfro does not locate an mfra")


def index_trace(path, kind, offset, length, _quantum):
    data = path.read_bytes()
    offset, length = int(offset), int(length)
    rows = []
    try:
        box_kind, body = _box(data, offset, length)
        fullbox, = struct.unpack_from(">I", body)
        if box_kind != kind.encode() or fullbox not in (0, 1 << 24):
            raise ValueError("invalid index type, version or flags")
        version = fullbox >> 24
        if kind == "sidx":
            rows.extend(_segment(body, version, offset + length))
        elif kind == "tfra":
            rows.extend(_random_access(body, version))
        else:
            rows.extend(_tail(data, body, version, offset, length))
    except (ValueError, struct.error):
        return rows + ["8"]
    return rows + ["7"]
