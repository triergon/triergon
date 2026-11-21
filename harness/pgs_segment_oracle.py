"""Independent HDMV Presentation Graphics segment table reader.

The envelope tables follow the public Presentation Graphics description cited
in the contract. Container demux is an input adapter, using the pinned formats
ffprobe for its public packet bytes. No candidate code is imported.
"""
import json
import subprocess
from struct import Struct, error

from harness.tx3g_oracle import row


def packets(path, data, mode, *args):
    if mode in ("sup", "packet"):
        return [data]
    if mode == "capsule":
        if not data.startswith(b"SMBMO001\x01"):
            raise ValueError("capsule marker")
        length, = Struct(">I").unpack_from(data, 21)
        count, = Struct(">I").unpack_from(data, 25 + length)
        at = 29 + length
        result = []
        for _ in range(count):
            reset, _, _, _, _, size = Struct(">BBqBqI").unpack_from(data, at)
            start = at + 23
            at = start + size
            if at > len(data):
                raise ValueError("capsule record extent")
            if not reset:
                result.append(data[start:at])
        if at != len(data):
            raise ValueError("capsule trailing bytes")
        return result
    probe = ".research/reference-formats/install/bin/ffprobe"
    process = subprocess.run([
        probe, "-v", "error", "-show_entries", "stream=index,codec_name:packet=stream_index,data",
        "-show_packets", "-show_streams", "-show_data", "-of", "json", str(path)
    ], capture_output=True, check=False)
    output = json.loads(process.stdout)
    streams = [stream["index"] for stream in output.get("streams", [])
               if stream.get("codec_name") == "hdmv_pgs_subtitle"]
    ordinal = int(args[-1])
    if ordinal >= len(streams):
        raise ValueError("PGS stream absent")
    return [
        bytes.fromhex(" ".join(line.split(": ", 1)[1].split("  ", 1)[0]
                               for line in packet["data"].splitlines() if line))
        for packet in output.get("packets", []) if packet["stream_index"] == streams[ordinal]
    ]


def typed(kind, body):
    if kind == 0x14:
        palette, version = Struct(">BB").unpack_from(body)
        values = list(Struct(">5B").iter_unpack(body[2:]))
        return [row(3, (1, palette, version, len(values))),
                *(row(3, (2, *entry)) for entry in values)]
    if kind == 0x15:
        ident, version, flags = Struct(">HBB").unpack_from(body)
        # The trace retains both the raw flags and their two decoded bits.
        fields = (3, ident, version, flags, bool(flags & 64), bool(flags & 128))
        at = 4
        if flags & 128:
            _, width, height = Struct(">3sHH").unpack_from(body, at)
            length = int.from_bytes(body[at:at + 3], "big")
            if length < 4:
                raise ValueError("object extent")
            fields += (length, width, height)
            at += 7
        return [row(3, (*fields, len(body) - at), body[at:])]
    if kind == 0x16:
        head = Struct(">HHBHBBBB")
        values = head.unpack_from(body)
        rows = [row(3, (4, *values))]
        at = head.size
        for _ in range(values[-1]):
            ident, window, flags, x, y = Struct(">HBBHH").unpack_from(body, at)
            at += 8
            fields = (5, ident, window, flags, x, y, bool(flags & 64), bool(flags & 128))
            if flags & 128:
                fields += Struct(">4H").unpack_from(body, at)
                at += 8
            rows.append(row(3, fields))
        if at != len(body):
            raise ValueError("composition extent")
        return rows
    if kind == 0x17:
        count, = Struct(">B").unpack_from(body)
        windows = list(Struct(">B4H").iter_unpack(body[1:]))
        if count != len(windows):
            raise ValueError("window count")
        return [row(3, (6, count)), *(row(3, (7, *window)) for window in windows)]
    return []


def segment_rows(data, framed):
    sup_clock = Struct(">2sII")
    body_header = Struct(">BH")
    start = 0
    while start < len(data):
        try:
            at = start
            if framed:
                marker, pts, dts = sup_clock.unpack_from(data, at)
                if marker != b"PG":
                    raise ValueError("SUP marker")
                clock = 1, pts, dts
                at += sup_clock.size
            else:
                clock = (0,)
            kind, length = body_header.unpack_from(data, at)
            at += body_header.size
            end = at + length
            if end > len(data):
                raise ValueError("segment extent")
        except (ValueError, error):
            yield "8"
            return
        yield row(3, (0, start, kind, end - start, length, *clock), data[start:end])
        try:
            yield from typed(kind, data[at:end])
        except (ValueError, error):
            yield row(8, (start, kind))
        start = end


def trace(path, mode, *args):
    try:
        inputs = packets(path, path.read_bytes(), mode, *args)
    except (ValueError, error):
        return ["8"]
    rows = []
    for index, data in enumerate(inputs):
        rows.append(row(2, (index, len(data))))
        rows.extend(segment_rows(data, mode == "sup"))
    return [*rows, "7"]
