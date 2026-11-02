"""Independent RFC 9559 timing oracle for the trusted selected fixture corpus.

Whole-file Python/Fraction tooling intentionally differs from the streaming Rust
parser. It never supplies or repairs the candidate output. Unsupported structures
fail the case rather than quietly reducing the source-time assertion.
"""
from fractions import Fraction


def variable(data, at, identifier=False):
    first = data[at]
    if first == 0:
        raise ValueError("zero EBML marker")
    width = 9 - first.bit_length()
    end = at + width
    if end > len(data):
        raise ValueError("truncated VINT")
    value = int.from_bytes(data[at:end], "big")
    if not identifier:
        value -= 1 << (width * 7)
    return value, end, not identifier and value == (1 << (width * 7)) - 1


def elements(data, begin, end):
    while begin < end:
        identifier, at, _ = variable(data, begin, True)
        length, start, unknown = variable(data, at)
        stop = end if unknown else start + length
        if stop > end:
            raise ValueError("element exceeds parent")
        yield identifier, start, stop, unknown
        begin = stop


def integer(data, start, stop, signed=False):
    if stop - start > 8:
        raise ValueError("integer width")
    return int.from_bytes(data[start:stop], "big", signed=signed)


def rational(value):
    return None if value is None else {"numerator": value.numerator, "denominator": value.denominator}


def timing_trace(path):
    data = path.read_bytes()
    if len(data) > 64 * 1024 * 1024:
        raise ValueError("oracle input size limit")
    segment = next((start, stop) for identifier, start, stop, _ in elements(data, 0, len(data)) if identifier == 0x18538067)
    tracks = {}
    scale = 1_000_000
    packets = []
    blocks = []
    cluster_time = None
    position, segment_end = segment
    cluster_end = None
    top_ids = {0x1F43B675, 0x1549A966, 0x1654AE6B, 0x1C53BB6B, 0x114D9B74, 0x1254C367, 0x1043A770, 0x1941A469}
    for _ in range(1_000_000):
        if position == segment_end:
            break
        identifier, at, _ = variable(data, position, True)
        length, start, unknown = variable(data, at)
        stop = segment_end if unknown else start + length
        if stop > segment_end:
            raise ValueError("element boundary")
        if cluster_end is not None and (position >= cluster_end or identifier in top_ids):
            cluster_end = None
            cluster_time = None
        position = stop
        if identifier == 0x1F43B675:
            cluster_end = stop
            position = start
            cluster_time = None
        elif identifier == 0x1549A966:
            for key, a, b, _ in elements(data, start, stop):
                if key == 0x2AD7B1:
                    scale = integer(data, a, b)
        elif identifier == 0x1654AE6B:
            for entry, a, b, _ in elements(data, start, stop):
                if entry != 0xAE:
                    continue
                fields = {key: integer(data, x, y) for key, x, y, _ in elements(data, a, b) if key in (0xD7, 0x23E383, 0x56AA)}
                tracks[fields[0xD7]] = {"index": len(tracks), "delay": fields.get(0x56AA, 0), "duration": fields.get(0x23E383)}
        elif cluster_end is not None and identifier == 0xE7:
            cluster_time = integer(data, start, stop)
        elif cluster_end is not None and identifier in (0xA3, 0xA0):
            duration_ticks = None
            if identifier == 0xA0:
                block = None
                for key, a, b, _ in elements(data, start, stop):
                    if key == 0xA1:
                        block = (a, b)
                    elif key == 0x9B:
                        duration_ticks = integer(data, a, b)
                if block is None:
                    raise ValueError("BlockGroup lacks Block")
                start, stop = block
            number, payload, _ = variable(data, start)
            relative = integer(data, payload, payload + 2, True)
            lace_mode = (data[payload + 2] >> 1) & 3
            count = data[payload + 3] + 1 if lace_mode else 1
            if cluster_time is None:
                raise ValueError("missing Cluster timestamp")
            blocks.append((number,cluster_time,relative,count,duration_ticks))
    else:
        raise ValueError("oracle element work limit")
    # A whole-file oracle can gather metadata before resolving block times;
    # this deliberately differs from the candidate's bounded seek-head loader.
    for number,cluster_time,relative,count,duration_ticks in blocks:
        track=tracks[number]
        base=Fraction((cluster_time+relative)*scale-track["delay"])
        duration=Fraction(duration_ticks*scale,count) if duration_ticks is not None else None if track["duration"] is None else Fraction(track["duration"])
        for frame in range(count):
            pts=base+frame*duration if duration is not None else base if frame==0 else None
            packets.append({"track":track["index"],"pts":rational(pts),"duration":rational(duration)})
    return {"schema":1,"packets":packets}
