"""Source sound entries from Apple's QuickTime specification and ETSI TS 102 114 Annex E."""

from struct import Struct, unpack_from

SOUND = Struct(">8xHHIHHhHI")


def atoms(data):
    """Walk a sequence of ISO boxes without a candidate parser or recovery policy."""
    while data:
        size, kind = unpack_from(">I4s", data)
        header = 8
        if size == 1:
            size, = unpack_from(">Q", data, 8)
            header = 16
        if size == 0:
            size = len(data)
        if not header <= size <= len(data):
            raise ValueError("invalid box extent")
        yield kind, data[header:size]
        data = data[size:]


def child(data, kind):
    return next(payload for tag, payload in atoms(data) if tag == kind)


def tracks(path):
    return [
        child(track, b"mdia")
        for tag, track in atoms(child(path.read_bytes(), b"moov"))
        if tag == b"trak"
    ]


def sound_entries(media):
    if child(media, b"hdlr")[8:12] != b"soun":
        return ()
    stsd = child(child(child(media, b"minf"), b"stbl"), b"stsd")
    entries = list(atoms(stsd[8:]))
    if len(entries) != int.from_bytes(stsd[4:8], "big"):
        raise ValueError("description count")
    return entries


def row(*fields):
    return " ".join(map(str, fields))


def qt_sound(path, _window, _quantum):
    rows = []
    for track, media in enumerate(tracks(path)):
        for index, (tag, data) in enumerate(sound_entries(media), 1):
            if len(data) < SOUND.size or data[8:10] != b"\0\0":
                rows.append(row(8, track, index))
                continue
            ref, _, revision, vendor, channels, bits, compression, packet, rate = sound(data)
            if ref == 0:
                rows.append(row(8, track, index))
                continue
            codec = {(b"raw ", 8): 1, (b"twos", 16): 2}.get((tag, bits), 0)
            if not (channels in (1, 2) and not compression and not packet and rate):
                codec = 0
            rows.append(row(3, track, index, int.from_bytes(tag, "big"), revision, vendor,
                            channels, bits, compression % (1 << 64), packet, rate, codec,
                            channels * (bits // 8) if codec else 0))
    return rows


def sound(data):
    return (int.from_bytes(data[6:8], "big"), *SOUND.unpack_from(data))


def descriptors(data):
    while data:
        tag = data[0]
        size = 0
        for offset in range(1, 5):
            byte = data[offset]
            size = size * 128 + byte % 128
            if byte < 128:
                break
        else:
            raise ValueError("descriptor size")
        start = offset + 1
        if start + size > len(data):
            raise ValueError("truncated descriptor")
        yield tag, data[start:start + size]
        data = data[start + size:]


def object_type(data, version):
    start = {0: 28, 1: 44, 2: 64}[version]
    children = dict(atoms(data[start:]))
    wave = b"wave" in children
    esds = children.get(b"esds")
    if esds is None and wave:
        esds = child(children[b"wave"], b"esds")
    es = dict(descriptors(esds[4:]))[3]
    # ES_Descriptor optional dependency, URL and OCR identifiers precede tag 4.
    flags = es[2]
    start = 3 + (2 if flags & 128 else 0)
    if flags & 64:
        start += 1 + es[start]
    if flags & 32:
        start += 2
    config = dict(descriptors(es[start:]))[4]
    return config[0], config[1] >> 2, wave


def dts_fields(tag, data):
    if tag in (b"dtse", b"dtsx"):
        raise ValueError("no declared core")
    if tag not in (b"dtsc", b"dtsh", b"dtsl", b"mp4a"):
        return 0, 0, 0, 0
    ref, version, revision, vendor, channels, bits, compression, packet, rate = sound(data)
    if tag == b"mp4a":
        oti, stream_type, wave = object_type(data, version)
        if oti in (0, 0xAB, 0xAC, 0xB2, 0xB3):
            raise ValueError("unsupported object type")
        if oti not in (0xA9, 0xAA):
            return 0, 0, 0, 0
        if version or wave or stream_type != 5:
            raise ValueError("DTS DecoderConfigDescriptor or sound version")
    if ref != 1 or not channels or bits != 16 or not rate or rate % 65536:
        raise ValueError("sample entry clock or geometry")
    rate //= 65536
    if tag == b"mp4a":
        return 1, rate, 0, 0
    if revision or vendor or packet:
        raise ValueError("reserved AudioSampleEntry fields")
    if tag == b"dtsc" and version == 1:
        if compression != -2 or data[32:44] != bytes.fromhex("000000000000000000000002"):
            raise ValueError("QuickTime DTS extension")
        if any(name == b"ddts" for name, _ in atoms(data[44:])):
            raise ValueError("QuickTime DTS has a second clock")
        return 1, rate, 0, 0
    if version or compression:
        raise ValueError("DTS AudioSampleEntry version")
    config, = [payload for name, payload in atoms(data[28:]) if name == b"ddts"]
    if len(config) != 20:
        raise ValueError("fixed DTS configuration")
    frequency, _, _, depth = unpack_from(">IIIB", config)
    # E.2.2.2 maps the maximum coded frequency to the sample entry's base rate.
    base = {base * ratio // 2: base for base in (32000, 44100, 48000)
            for ratio in (1, 2, 4, 8)}
    duration_code, mode, tail = config[13] >> 6, (config[13] // 2) % 32, config[19]
    core, size, representation = (int.from_bytes(config[14:17], "big") >> shift & mask
                                  for shift, mask in ((18, 63), (4, 16383), (0, 7)))
    layout = int.from_bytes(config[17:19], "big")
    # Table E-2 lists the core and core-plus-extension rows under dtsc/dtsh.
    # The compatibility view also accepts the dtsl label for explicit core + XLL.
    modes = {b"dtsc": {1, 2, 4}, b"dtsh": {3, *range(5, 17)}, b"dtsl": {14, 15, 16}}
    if not (base.get(frequency) == rate and depth in (16, 24) and mode in modes[tag]
            and core in (0, 2, 4, 5, 6, 7, 8, 9, 31) and size and tail == 0
            and (layout or representation in (0, 2, 3))):
        raise ValueError("DTS fields outside the core profile")
    return 1, rate, frequency, (512, 1024, 2048, 4096)[duration_code]


def ddts(path, _window, _quantum, profile=""):
    media_list = tracks(path)
    rows = [row(2, len(media_list))]
    for track, media in enumerate(media_list):
        for index, (tag, data) in enumerate(sound_entries(media), 1):
            try:
                if profile == "mismatched-format":
                    raise ValueError("typed format differs from wire format")
                fields = dts_fields(tag, data)
            except (ValueError, KeyError, IndexError, StopIteration):
                fields = (2, 0, 0, 0)
            rows.append(row(2, track, index, *fields))
    return rows
