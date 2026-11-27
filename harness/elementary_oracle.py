"""Elementary stream metadata from wire fields.

RFC 9639 sections 7 and 8, ISO/IEC 11172-3 and 13818-3 with Xing/LAME and
VBRI extensions, ISO/IEC 13818-7 ADTS, ATSC A/52 sections 5.3 and E.1.3,
ID3v2.4 and APEv2 tag envelopes.
"""

from struct import unpack

ABSENT = 2**64 - 1


def line(kind, *words, data=b""):
    return " ".join([str(kind), *map(str, words)] + ([data.hex()] if data else []))


def int_at(data, at, width, order="big"):
    span = data[at:at + width]
    if len(span) != width:
        raise ValueError("truncated integer")
    return int.from_bytes(span, order)


def tags(data):
    """Yield original envelope coordinates and payloads in reader order."""
    start = 0
    while data[start:start + 3] == b"ID3":
        size = 0
        for value in data[start + 6:start + 10]:
            if value >= 128:
                raise ValueError("invalid ID3 tag size")
            size = (size << 7) | value
        end = start + 10 + size + (10 if data[start + 3] >= 4 and data[start + 5] & 16 else 0)
        yield (2, start, data[start:end])
        start = end
    end = len(data)
    if data[-128:-125] == b"TAG":
        end -= 128
    footer = end - 32
    if data[footer:footer + 8] == b"APETAGEX":
        ape_bytes = int_at(data, footer + 12, 4, "little")
        header = end - ape_bytes - 32
        if header >= start and data[header:header + 8] == b"APETAGEX":
            yield (3, header, data[header:])
            return
    if end != len(data):
        yield (1, end, data[end:])


def flac(data):
    if not data.startswith(b"fLaC"):
        raise ValueError("expected native FLAC metadata")
    offset = 4
    records, points = [], []
    stream = None
    while True:
        kind = data[offset] & 127
        last = data[offset] & 128
        width = int_at(data, offset + 1, 3)
        body = data[offset + 4:offset + 4 + width]
        if len(body) != width:
            raise ValueError("truncated FLAC metadata")
        if kind == 0:
            low, high = unpack(">HH", body[:4])
            rate = int_at(body, 10, 3) >> 4
            channels = ((body[12] >> 1) & 7) + 1
            bits = ((int_at(body, 12, 2) >> 4) & 31) + 1
            total = int_at(body, 13, 5) & ((1 << 36) - 1)
            stream = (rate, channels, total or ABSENT)
            yield line(3, low, high, int_at(body, 4, 3), int_at(body, 7, 3),
                       rate, channels, bits, total, data=body[18:34])
        if kind == 3:
            for at in range(0, len(body) - 17, 18):
                sample = int_at(body, at, 8)
                if sample != ABSENT:
                    points.append(line(4, sample, int_at(body, at + 8, 8), int_at(body, at + 16, 2)))
        payload = b"" if kind == 1 else body
        records.append(line(6, kind, width, len(payload), data=payload))
        offset += 4 + width
        if last:
            break
    yield from records
    yield from points
    return stream


MPEG1_RATES = {
    1: (0, 32, 64, 96, 128, 160, 192, 224, 256, 288, 320, 352, 384, 416, 448),
    2: (0, 32, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384),
    3: (0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320),
}
MPEG2_LAYER1 = (0, 32, 48, 56, 64, 80, 96, 112, 128, 144, 160, 176, 192, 224, 256)
MPEG2_LAYER23 = (0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160)


def mpeg_header(data, offset):
    a, b, c, d = data[offset:offset + 4]
    if a != 255 or b >> 5 != 7:
        raise ValueError("expected MPEG audio frame sync")
    version, layer, crc = (b >> 3) & 3, 4 - ((b >> 1) & 3), 1 - (b & 1)
    rates = MPEG1_RATES[layer] if version == 3 else MPEG2_LAYER1 if layer == 1 else MPEG2_LAYER23
    kbps = rates[c >> 4]
    rate = (44100, 48000, 32000)[(c >> 2) & 3] // {0: 4, 2: 2, 3: 1}[version]
    pad = (c >> 1) & 1
    mode = d >> 6
    fields = (version, layer, crc, kbps, rate, pad, mode,
              (d >> 4) & 3, (d >> 3) & 1, (d >> 2) & 1, d & 3)
    return fields


def mpeg(data, start):
    h = mpeg_header(data, start)
    version, layer, crc, kbps, rate, pad, mode = h[:7]
    mono = mode == 3
    samples = 384 if layer == 1 else 576 if layer == 3 and version != 3 else 1152
    length = (samples // 8 * kbps * 1000 // rate + pad) if layer != 1 else (12000 * kbps // rate + pad) * 4
    offset = start + 4 + 2 * crc + (17 if mono else 32) // (1 if version == 3 else 2)
    if version != 3 and mono:
        offset += 1
    kind = {b"Xing": 1, b"Info": 2}.get(data[offset:offset + 4]) if layer == 3 else None
    xing = kind is not None
    if not xing and layer == 3 and data[start + 36:start + 40] == b"VBRI":
        kind = 3
        offset = start + 36
    frames = size = ABSENT
    toc = b""
    lame = None
    consistent = True
    if xing:
        flags = int_at(data, offset + 4, 4)
        offset += 8
        if flags & 1:
            frames = int_at(data, offset, 4)
            offset += 4
        if flags & 2:
            size = int_at(data, offset, 4)
            offset += 4
        if flags & 4:
            toc = data[offset:offset + 100]
            offset += 100
        if flags & 8:
            offset += 4
        if offset + 24 <= start + length:
            name = data[offset:offset + 9]
            if all(v == 0 or 32 <= v < 127 for v in name):
                gapless = int_at(data, offset + 21, 3)
                lame = (gapless >> 12, gapless & 4095, name)
    elif kind == 3:
        size = int_at(data, offset + 10, 4)
        frames = int_at(data, offset + 14, 4)
    if kind:
        consistent = size == ABSENT or len(data) - start - 4 <= size + size // 16
    yield line(3, *h)
    if kind:
        yield line(6, kind, start, length, int(consistent), frames, size, len(toc), data=toc)
    if lame:
        yield line(4, *lame[:2], data=lame[2])
    total = ABSENT
    if frames != ABSENT and consistent:
        trim = sum(lame[:2]) if lame and lame[2].startswith((b"LAME", b"Lavf", b"Lavc")) else 0
        total = max(0, frames * samples - trim)
    return (h[4], 1 if h[6] == 3 else 2, total)


def adts(data):
    h = data[:7]
    a, b, c, d, e, f, g = h
    if a != 255 or b & 0xf6 != 0xf0:
        raise ValueError("expected ADTS header")
    index = (c >> 2) & 15
    channel = ((c & 1) << 2) | (d >> 6)
    yield line(3, (b >> 3) & 1, (b >> 1) & 3, b & 1, c >> 6, index, (c >> 1) & 1,
               channel, (d >> 5) & 1, (d >> 4) & 1, (d >> 3) & 1, (d >> 2) & 1,
               ((d & 3) << 11) | (e << 3) | (f >> 5), ((f & 31) << 6) | (g >> 2),
               g & 3, data=h)
    rate = (96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350)[index]
    return (rate, 8 if channel == 7 else channel, ABSENT)


def ac3(data):
    if data[:2] != b"\x0b\x77":
        raise ValueError("expected AC-3 sync")
    bsid = data[5] >> 3
    rates = (48000, 44100, 32000)
    if bsid <= 8:
        fscod = data[4] >> 6
        sizecode = data[4] & 63
        kbps = (32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384,
                448, 512, 576, 640)[sizecode // 2]
        rate = rates[fscod]
        length = (kbps * 4, 2 * (320 * kbps // 147 + (sizecode & 1)), kbps * 6)[fscod]
        blocks = 6
        mode = data[6] >> 5
        offset = 3 + 2 * ((mode & 1 != 0 and mode != 1) + (mode & 4 != 0) + (mode == 2))
        lfe = (int_at(data, 6, 2) >> (15 - offset)) & 1
    else:
        length = 2 * ((int_at(data, 2, 2) & 2047) + 1)
        fscod, secondary = data[4] >> 6, (data[4] >> 4) & 3
        rate = rates[secondary] // 2 if fscod == 3 else rates[fscod]
        blocks = 6 if fscod == 3 else (1, 2, 3, 6)[secondary]
        mode, lfe = (data[4] >> 1) & 7, data[4] & 1
    yield line(3, length, rate, blocks, bsid, mode, lfe)
    return (rate, (2, 1, 2, 3, 3, 4, 4, 5)[mode] + lfe, ABSENT)


def trace(path, family, mode):
    if mode != "wire":
        raise ValueError("expected elementary metadata wire mode")
    data = path.read_bytes()
    envelopes = list(tags(data))
    start = sum(len(payload) for kind, _, payload in envelopes if kind == 2)
    if family == "mp3":
        parser = mpeg(data, start)
    else:
        parser = {"flac": flac, "aac": adts, "ac3": ac3, "eac3": ac3}[family](data[start:])
    rows = []
    while True:
        try:
            rows.append(next(parser))
        except StopIteration as result:
            track = result.value
            break
    rows.extend(line(5, kind, offset, len(payload), len(payload), data=payload)
                for kind, offset, payload in envelopes)
    if track:
        rows.append(line(2, *track))
    rows.append(line(7))
    return rows
