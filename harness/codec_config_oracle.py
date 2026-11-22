"""Whole-buffer configuration oracles from the wire syntax.

The hvcC layout is ISO/IEC 14496-15's HEVCDecoderConfigurationRecord.
The VP9 layout is the VP Codec ISO Media File Format Binding v1.0.
E-AC-3 follows ATSC A/52:2018 Annex E Tables E1.2 and E2.5.
"""


def hvcc_prefix(path):
    data = path.read_bytes()
    if len(data) < 23:
        return ["8"]
    (version, profile, compatible, constraints, level, spatial, parallelism,
     chroma, luma, chroma_depth, average_rate, temporal, arrays) = (
        int.from_bytes(data[start:end], "big")
        for start, end in ((0, 1), (1, 2), (2, 6), (6, 12), (12, 13), (13, 15),
                           (15, 16), (16, 17), (17, 18), (18, 19), (19, 21),
                           (21, 22), (22, 23))
    )
    length_size = temporal % 4
    issues = sum(2 ** index for index, bad in enumerate((
        spatial < 0xf000, parallelism < 252, chroma < 252,
        luma < 248, chroma_depth < 248, length_size == 2,
    )) if bad)
    fields = (
        version, profile // 64, profile // 32 % 2, profile % 32, compatible, constraints,
        level, spatial % 4096, parallelism % 4, chroma % 4, luma % 8, chroma_depth % 8,
        average_rate, temporal // 64, temporal // 8 % 8, temporal // 4 % 2,
        length_size, arrays, issues,
    )
    return [f"3 {index} {value}" for index, value in enumerate(fields)]


def vpcc(path, flags=None):
    data = path.read_bytes()
    if len(data) < 12 or data[0] != 1:
        return ["8"]
    flags = int.from_bytes(data[1:4], "big") if flags is None else int(flags)
    if not 0 <= flags < 1 << 24:
        return ["8"]
    profile, level, video, primary, transfer, matrix, high, low = data[4:12]
    initialization = data[12:]
    size = high * 256 + low
    if len(initialization) != size:
        return ["8"]
    depth, chroma_and_range = divmod(video, 16)
    chroma, full_range = divmod(chroma_and_range, 2)
    issues = sum(2 ** index for index, bad in enumerate((
        flags != 0, size != 0, profile > 3, depth not in (8, 10, 12),
        chroma > 3, matrix == 0 and chroma != 3,
    )) if bad)
    fields = profile, level, depth, chroma, full_range, primary, transfer, matrix, size, issues
    rows = [f"3 {index} {value}" for index, value in enumerate(fields)]
    rows.append(f"2 {size}" + (f" {initialization.hex()}" if size else ""))
    return rows


class Refused(ValueError):
    """Record syntax or the documented inspection policy refused this frame."""


class Fields:
    def __init__(self, data, limit):
        self.data = int.from_bytes(data, "big")
        self.size = len(data) * 8
        self.end = min(self.size, limit)
        self.pos = 0

    def read(self, width):
        self.pos += width
        if self.pos > self.end:
            raise Refused()
        return (self.data >> (self.size - self.pos)) & ((1 << width) - 1)

    def optional(self, width):
        return self.read(width) if self.read(1) else None


def mixing(read, stream, mode, lfe, blocks):
    start = read.pos
    if not read.read(1):
        return None, None
    read.read(2 * (mode > 2) + 6 * (mode in (3, 5, 7)) + 6 * (mode >= 4))
    if lfe:
        read.optional(5)
    definition = None
    if stream == 0:
        read.optional(6)
        if mode == 0:
            read.optional(6)
        read.optional(6)
        definition = read.read(2)
        if definition == 3:
            # Flexible mixing is outside the documented inspection policy.
            raise Refused()
        read.read((0, 5, 12)[definition])
        for _ in range(max(0, 2 - mode)):
            read.optional(14)
        if read.read(1):
            if blocks == 1:
                read.read(5)
            else:
                for _ in range(blocks):
                    read.optional(5)
    return (start, read.pos - start), definition


def informational(read, mode, rate):
    start = read.pos
    if not read.read(1):
        return [None] * 10
    service, copyright_bit, original = read.read(3), read.read(1), read.read(1)
    surround, headphone = (read.read(2), read.read(2)) if mode == 2 else (None, None)
    surround_ex = read.read(2) if mode >= 6 else None
    production = read.optional(8)
    second = read.optional(8) if mode == 0 else None
    source_rate = read.read(1) if rate != 3 else None
    return [(start, read.pos - start), service, copyright_bit, original, surround,
            headphone, surround_ex, production, second, source_rate]


def eac3_frame(frame, bsi_limit, frame_limit):
    if not 6 <= len(frame) <= frame_limit or bsi_limit <= 0:
        raise Refused()
    aux = (frame[-3] >> 1) & 1
    length = (int.from_bytes(frame[-4:-2], "big") >> 2) if aux else None
    tail = [aux, length, frame[-3] & 1, int.from_bytes(frame[-2:], "big")]
    limit = min(len(frame) * 8 - (32 if aux else 18), 16 + bsi_limit)
    read = Fields(frame, limit)
    sync, stream, _sid, _words = (read.read(n) for n in (16, 2, 3, 11))
    rate, block_code, mode, lfe, bsid = (read.read(n) for n in (2, 2, 3, 1, 5))
    if sync != 0x0b77 or stream == 3 or bsid not in range(11, 17):
        raise Refused()
    if rate == 3 and block_code == 3:
        raise Refused()
    blocks = 6 if rate == 3 else (1, 2, 3, 6)[block_code]
    for _ in range(2 if mode == 0 else 1):
        read.read(5)
        read.optional(8)
    channel_map = read.optional(16) if stream == 1 else None
    channels = None
    consistent = None
    if channel_map is not None:
        pairs = (5, 6, 9, 10, 11, 13)
        channels = channel_map.bit_count() + sum(bool(channel_map & (0x8000 >> b)) for b in pairs)
        consistent = int(channels == (2, 1, 2, 3, 3, 4, 4, 5)[mode] + lfe)
    prefix = read.pos
    mix, definition = mixing(read, stream, mode, lfe, blocks)
    info = informational(read, mode, rate)
    converter_sync = read.read(1) if stream == 0 and blocks != 6 else None
    block = read.read(1) if stream == 2 and blocks != 6 else None
    effective = 1 if stream == 2 and blocks == 6 else block
    converted_size = read.read(6) if effective else None
    extra = None
    if read.read(1):
        count = 8 * (1 + read.read(6))
        extra = read.pos, count
        read.read(count)
    rows = [read.pos, prefix, mix, definition, *info, converter_sync, block,
            effective, converted_size, extra, *tail, channels, consistent]
    extra_bytes = frame[extra[0] // 8 : (sum(extra) + 7) // 8] if extra else b""
    return rows, extra_bytes


def eac3_bsi(path, bsi_bits, frame_bytes):
    source = path.read_bytes()
    bsi_bits, frame_bytes = int(bsi_bits), int(frame_bytes)
    rows = []
    offset, count = 0, 0
    while offset < len(source):
        prefix = source[offset:offset + 6]
        if len(prefix) != 6:
            return rows + [f"8 {offset}"]
        size = 2 * (1 + int.from_bytes(prefix[2:4], "big") % 2048)
        frame = source[offset:offset + size]
        if len(frame) != size:
            return rows + [f"8 {offset}"]
        try:
            fields, extra = eac3_frame(frame, bsi_bits, frame_bytes)
        except Refused:
            return rows + [f"8 {offset}"]
        rows.append(f"1 {offset} {size}")
        for index, value in enumerate(fields):
            if value is None:
                rows.append(f"3 {index} 0 0 0")
            else:
                first, last = value if isinstance(value, tuple) else (value, 0)
                rows.append(f"3 {index} 1 {first} {last}")
        if extra:
            rows.append(f"2 {len(extra)} {extra.hex()}")
        offset += size
        count += 1
    return rows + [f"7 {count}"]
