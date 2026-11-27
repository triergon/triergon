"""AV1 temporal-unit framing from ISO BMFF samples and WebM SimpleBlocks."""

from struct import iter_unpack, unpack_from

from .codec_stream_oracle import leb_size, row
from .spec_oracle import elements, variable


def boxes(data):
    offset = 0
    while offset < len(data):
        length, kind = unpack_from(">I4s", data, offset)
        if not 8 <= length <= len(data) - offset:
            raise ValueError("unsupported or truncated AV1 fixture box")
        yield kind, data[offset + 8:offset + length]
        offset += length


def child(data, kind):
    return next(body for tag, body in boxes(data) if tag == kind)


def mp4_track(data, stream):
    tracks = [body for tag, body in boxes(child(data, b"moov")) if tag == b"trak"]
    table = child(child(child(tracks[stream], b"mdia"), b"minf"), b"stbl")
    fields = dict(boxes(table))
    entry_count, = unpack_from(">I", fields[b"stsd"], 4)
    if entry_count != 1:
        raise ValueError("AV1 oracle fixture needs one sample entry")
    tag, entry = next(boxes(fields[b"stsd"][8:]))
    if tag not in (b"av01", b"dav1"):
        raise ValueError("missing AV1 visual sample entry")
    record = child(entry[78:], b"av1C")
    fixed_size, count = unpack_from(">II", fields[b"stsz"], 4)
    sizes = [fixed_size] * count if fixed_size else [
        value for value, in iter_unpack(">I", fields[b"stsz"][12:])]
    if len(sizes) != count:
        raise ValueError("sample size table disagrees with its count")
    chunks = [value for value, in iter_unpack(">I", fields[b"stco"][8:])]
    runs = list(iter_unpack(">III", fields[b"stsc"][8:]))
    sync = None
    if b"stss" in fields:
        sync = {value for value, in iter_unpack(">I", fields[b"stss"][8:])}

    def samples():
        sample = 0
        run = 0
        for chunk, offset in enumerate(chunks, 1):
            if run + 1 < len(runs) and runs[run + 1][0] == chunk:
                run += 1
            first, samples_per_chunk, description = runs[run]
            if first > chunk or description != 1:
                raise ValueError("invalid AV1 sample-to-chunk entry")
            for _ in range(samples_per_chunk):
                end = offset + sizes[sample]
                if end > len(data):
                    raise ValueError("AV1 sample extends beyond its file")
                sample += 1
                yield data[offset:end], sync is None or sample in sync
                offset = end
        if sample != count:
            raise ValueError("AV1 sample-to-chunk entries miss samples")

    return record, samples()


def webm_track(data, stream):
    start, end = next((a, b) for tag, a, b, _ in elements(data, 0, len(data))
                      if tag == 0x18538067)
    segment = list(elements(data, start, end))
    tracks_start, tracks_end = next((a, b) for tag, a, b, _ in segment if tag == 0x1654AE6B)
    tracks = [(a, b) for tag, a, b, _ in elements(data, tracks_start, tracks_end)
              if tag == 0xAE]
    fields = {tag: data[a:b] for tag, a, b, _ in elements(data, *tracks[stream])}
    if fields[0x86] != b"V_AV1":
        raise ValueError("wrong WebM CodecID")
    number = int.from_bytes(fields[0xD7], "big")

    def samples():
        for tag, a, b, _ in segment:
            if tag != 0x1F43B675:
                continue
            for tag, start, end, _ in elements(data, a, b):
                if tag == 0xA0:
                    raise ValueError("AV1 oracle fixture uses SimpleBlocks")
                if tag != 0xA3:
                    continue
                track, header, _ = variable(data, start)
                if track != number:
                    continue
                if end <= header + 3 or data[header + 2] & 6:
                    raise ValueError("AV1 oracle fixture needs one frame in each block")
                yield data[header + 3:end], bool(data[header + 2] & 0x80)

    return fields[0x63A2], samples()


def leb128(value):
    parts = []
    while value >= 128:
        parts.append(128 | (value & 127))
        value >>= 7
    return bytes([*parts, value])


def obus(data, bounded_sample=False):
    """AV1 §§4.10.5 and 5.2–5.3, with implicit size allowed at a sample end."""
    at = 0
    while at < len(data):
        header = data[at]
        kind = (header >> 3) & 15
        payload = at + 1 + bool(header & 4)
        if header & 0x81 or payload > len(data) or (header & 4 and data[at + 1] & 7):
            raise ValueError("invalid OBU header")
        if header & 2:
            width, size = leb_size(data[payload:])
            end = payload + width + size
            if end > len(data):
                raise ValueError("short OBU")
            wire = data[at:end]
        elif bounded_sample:
            end = len(data)
            size = end - payload
            wire = bytes([header | 2]) + data[at + 1:payload] + leb128(size) + data[payload:]
        else:
            raise ValueError("configuration OBU needs an explicit size")
        yield kind, size, wire
        at = end


def temporal_unit(sample, sync, initialization):
    units = list(obus(sample, bounded_sample=True))
    delimiter = b"\x12\x00"
    if units and units[0][0] == 2:
        _, size, delimiter = units.pop(0)
        if size:
            raise ValueError("nonempty temporal delimiter")
    if not units or any(kind == 2 for kind, _, _ in units):
        raise ValueError("invalid temporal unit boundary")
    prefix = (initialization if sync and all(kind != 1 for kind, _, _ in units)
              else b"")
    return delimiter + prefix + b"".join(wire for _, _, wire in units)


def trace(path, container, stream, codec):
    if codec != "av1" or container not in ("mp4", "mkv"):
        raise ValueError("expected an AV1 stream in MP4 or WebM")
    data = path.read_bytes()
    record, samples = (mp4_track if container == "mp4" else webm_track)(data, int(stream))
    if len(record) < 4 or record[0] != 0x81:
        raise ValueError("invalid AV1 codec configuration record")
    config = list(obus(record[4:]))
    initialization = record[4:] if any(kind == 1 for kind, _, _ in config) else b""
    rows = [row(1, [], temporal_unit(sample, sync, initialization)) for sample, sync in samples]
    if not rows:
        raise ValueError("AV1 oracle input has no temporal units")
    return rows + ["7"]
