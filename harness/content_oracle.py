"""Independent whole-file content oracle for the selected finite compression corpus.

Uses Python's EBML traversal and system zlib, never candidate output. Exact source
frame payloads and CodecPrivate are checked separately from decoder packet views.
Unsupported source structures fail explicitly rather than reducing assertions.
"""
import zlib
from .spec_oracle import elements, variable


def unsigned(data, begin, end):
    if end - begin > 8:
        raise ValueError('integer width')
    return int.from_bytes(data[begin:end], 'big')


def encoding_plan(data, begin, end):
    stages = []
    for identifier, a, b, unknown in elements(data, begin, end):
        if identifier != 0x6240 or unknown:
            raise ValueError('unsupported encoding structure')
        order, scope, kind, algorithm, prefix = 0, 1, 0, 0, b''
        for key, x, y, unknown in elements(data, a, b):
            if unknown:
                raise ValueError('unknown encoding length')
            if key == 0x5031:
                order = unsigned(data, x, y)
            elif key == 0x5032:
                scope = unsigned(data, x, y)
            elif key == 0x5033:
                kind = unsigned(data, x, y)
            elif key == 0x5035:
                kind = 1
            elif key == 0x5034:
                for field, p, q, unknown in elements(data, x, y):
                    if unknown:
                        raise ValueError('unknown compression length')
                    if field == 0x4254:
                        algorithm = unsigned(data, p, q)
                    elif field == 0x4255:
                        prefix = data[p:q]
        if kind or scope not in (1, 2, 3) or algorithm not in (0, 3):
            raise ValueError('unsupported content encoding')
        stages.append((order, scope, algorithm, prefix))
    if len(stages) > 8 or len({stage[0] for stage in stages}) != len(stages):
        raise ValueError('encoding order/count')
    return sorted(stages, reverse=True)


def restore(blob, plan, scope, limit):
    for _, selected, algorithm, prefix in plan:
        if not selected & scope:
            continue
        if algorithm == 0:
            decoder = zlib.decompressobj()
            blob = decoder.decompress(blob, limit + 1)
            if not decoder.eof or decoder.unused_data or decoder.unconsumed_tail:
                raise ValueError('incomplete or trailing compressed data')
        else:
            blob = prefix + blob
        if len(blob) > limit:
            raise ValueError('decoded oracle resource limit')
    return blob


def frames(data, begin, end):
    number, at, _ = variable(data, begin)
    if at + 3 > end:
        raise ValueError('truncated block header')
    mode = (data[at + 2] >> 1) & 3
    at += 3
    if not mode:
        return number, [data[at:end]]
    if at >= end:
        raise ValueError('missing lace count')
    count = data[at] + 1
    at += 1
    if count < 2:
        raise ValueError('single frame lacing')
    sizes = []
    if mode == 1:
        for _ in range(count - 1):
            size = 0
            while True:
                if at >= end:
                    raise ValueError('truncated Xiph size')
                byte = data[at]
                at += 1
                size += byte
                if byte < 255:
                    break
            sizes.append(size)
    elif mode == 2:
        if (end - at) % count:
            raise ValueError('unequal fixed lace')
        sizes = [(end - at) // count] * (count - 1)
    else:
        first, at, _ = variable(data, at)
        sizes.append(first)
        for _ in range(count - 2):
            start = at
            value, at, _ = variable(data, at)
            delta = value - ((1 << (7 * (at - start) - 1)) - 1)
            sizes.append(sizes[-1] + delta)
    sizes.append(end - at - sum(sizes))
    if any(size < 0 for size in sizes) or sum(sizes) != end - at:
        raise ValueError('lace boundaries')
    decoded = []
    for size in sizes:
        decoded.append(data[at:at + size])
        at += size
    return number, decoded


def content_trace(path):
    data = path.read_bytes()
    if len(data) > 64 * 1024 * 1024:
        raise ValueError('oracle input limit')
    _, a, b, _ = next(e for e in elements(data, 0, len(data)) if e[0] == 0x18538067)
    top = list(elements(data, a, b))
    tracks = {}
    for identifier, a, b, unknown in top:
        if identifier != 0x1654ae6b:
            continue
        if unknown:
            raise ValueError('unknown Tracks length')
        for identifier, x, y, unknown in elements(data, a, b):
            if identifier != 0xae or unknown:
                raise ValueError('TrackEntry structure')
            number, private, plan = None, b'', []
            for key, p, q, unknown in elements(data, x, y):
                if unknown:
                    raise ValueError('unknown track field length')
                if key == 0xd7:
                    number = unsigned(data, p, q)
                elif key == 0x63a2:
                    private = data[p:q]
                elif key == 0x6d80:
                    plan = encoding_plan(data, p, q)
            if number is None or number in tracks:
                raise ValueError('track identity')
            tracks[number] = (len(tracks), restore(private, plan, 2, 1 << 20), plan)
    output = []
    for identifier, a, b, unknown in top:
        if identifier != 0x1f43b675:
            continue
        if unknown:
            raise ValueError('unknown Cluster outside selected content oracle')
        for key, x, y, unknown in elements(data, a, b):
            if unknown:
                raise ValueError('unknown Cluster child')
            if key == 0xa0:
                _, x, y, _ = next(e for e in elements(data, x, y) if e[0] == 0xa1)
            elif key != 0xa3:
                continue
            number, stored = frames(data, x, y)
            index, _, plan = tracks[number]
            output.extend({'track': index, 'payload': restore(frame, plan, 1, 16 << 20).hex()} for frame in stored)
    return {'private': [private.hex() for _, private, _ in tracks.values()], 'frames': output}


def projected_content(trace):
    return {'private': [track['private'] for track in trace['tracks']],
            'frames': [{'track': packet['track'], 'payload': packet['payload']} for packet in trace['packets']]}


def encoding_chain(path):
    """CodecPrivate and the single frame of the first track, each decoded through
    the track's ContentEncodings in descending order with Python's zlib.

    Covers the authored zlib and header-stripping chains (`cargo xtask fixture zlib`),
    whose one Cluster holds one SimpleBlock without lacing."""
    data = path.read_bytes()
    _, start, stop, _ = next(x for x in elements(data, 0, len(data)) if x[0] == 0x18538067)
    top = list(elements(data, start, stop)); encodings = []; private = b''
    _, a, b, _ = next(x for x in top if x[0] == 0x1654ae6b)
    _, a, b, _ = next(x for x in elements(data, a, b) if x[0] == 0xae)
    for key, x, y, _ in elements(data, a, b):
        if key == 0x63a2: private = data[x:y]
        if key != 0x6d80: continue
        for _, c, d, _ in elements(data, x, y):
            order, scope, algorithm, header = 0, 1, 0, b''
            for key, c, d, _ in elements(data, c, d):
                if key == 0x5031: order = int.from_bytes(data[c:d], 'big')
                if key == 0x5032: scope = int.from_bytes(data[c:d], 'big')
                if key == 0x5034:
                    for key, c, d, _ in elements(data, c, d):
                        if key == 0x4254: algorithm = int.from_bytes(data[c:d], 'big')
                        if key == 0x4255: header = data[c:d]
            encodings.append((order, scope, algorithm, header))
    _, a, b, _ = next(x for x in top if x[0] == 0x1f43b675)
    _, a, b, _ = next(x for x in elements(data, a, b) if x[0] == 0xa3)
    _, header, _ = variable(data, a); frame = data[header + 3:b]
    for _, scope, algorithm, prefix in sorted(encodings, reverse=True):
        decode = lambda encoded: zlib.decompress(encoded) if algorithm == 0 else prefix + encoded
        if scope & 1: frame = decode(frame)
        if scope & 2: private = decode(private)
    return {'private': private.hex(), 'frames': [frame.hex()]}
