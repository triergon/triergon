"""Whole-buffer DTS core syntax and word packing (ETSI TS 102 114 §5.3)."""
from fractions import Fraction

# Table 5-1, in stream order. HCRC is conditional on CPF.
WIDTHS = (32, 1, 5, 1, 7, 14, 6, 4, 5, 1, 1, 1, 1, 1, 3, 1, 1, 2, 1,
          16, 1, 4, 2, 3, 1, 1, 4)
CORE = 0x7FFE8001
EXTENSION = 0x64582025
RATES = {1: 8000, 2: 16000, 3: 32000, 6: 11025, 7: 22050, 8: 44100,
         11: 12000, 12: 24000, 13: 48000}
BITRATES = (32000, 56000, 64000, 96000, 112000, 128000, 192000, 224000, 256000,
            320000, 384000, 448000, 512000, 576000, 640000, 768000, 960000,
            1024000, 1152000, 1280000, 1344000, 1408000, 1411200, 1472000, 1536000)
CHANNELS = (1, 2, 2, 2, 2, 3, 3, 4, 4, 5, 6, 6, 6, 7, 8, 8)
NORMAL_BLOCKS = (7, 15, 31, 63, 127)


def header_fields(data):
    bits = "".join(f"{byte:08b}" for byte in data)
    fields = []
    values = {}
    offset = 0
    for index, width in enumerate(WIDTHS):
        if index == 19 and values.get(3) == 0:
            fields.append((1, 0, 0, 0, 0))
        elif (index and values.get(0) != CORE) or offset >= len(bits):
            fields.append((0, 0, 0, 0, 0))
        else:
            observed = bits[offset:offset + width]
            raw = int(observed, 2)
            fields.append((2, offset, width, len(observed), raw))
            if len(observed) == width:
                values[index] = raw
            offset += width
    return fields, values


def optional(value):
    return "-" if value is None else str(value)


def header_views(fields):
    def value(index):
        return fields.get(index)

    sync, normal, short, cpf = map(value, (0, 1, 2, 3))
    blocks, fsize, amode, sfreq, bitrate = map(value, (4, 5, 6, 7, 8))
    fixed, ext_id, ext, lfe = map(value, (9, 14, 15, 17))
    version, pcmr, dialog = map(value, (21, 23, 26))
    rate = RATES.get(sfreq)
    nominal = BITRATES[bitrate] if bitrate is not None and bitrate < len(BITRATES) else None
    count = CHANNELS[amode] if amode is not None and amode < len(CHANNELS) else None
    samples = (blocks + 1) * 32 if blocks is not None and blocks >= 5 else None
    extent = None if fsize is None else fsize + 1
    duration = None
    if normal == 1 and short == 31 and blocks in NORMAL_BLOCKS and version in range(8) and rate:
        span = Fraction(samples, rate)
        duration = f"{span.numerator}/{span.denominator}"
    gain = None if dialog is None or version not in (6, 7) else -dialog - 16 * (version == 6)
    size = 104 + 16 * cpf if sync == CORE and cpf is not None else None

    violations = (
        sync in (None, CORE) and dialog is None,
        sync not in (None, CORE),
        fixed == 1,
        fsize is not None and fsize < 95,
        blocks is not None and (blocks < 5 or normal == 1 and blocks not in NORMAL_BLOCKS),
        sfreq is not None and not rate,
        bitrate not in (None, 29) and nominal is None,
        lfe == 3,
        pcmr in (4, 7),
        normal == 1 and short not in (31, None),
        normal == 0,
        version is not None and version > 7,
        ext == 1 and ext_id not in (0, 2, 6, None),
        sync == EXTENSION,
    )
    if violations[13]:
        status = "Unsupported"
    elif violations[1]:
        status = "Invalid"
    elif violations[0]:
        status = "Truncated"
    elif any(violations[2:10]):
        status = "Invalid"
    else:
        status = "Unsupported" if any(violations[10:13]) else "NormalHeader"
    issue_mask = sum(1 << bit for bit, active in enumerate(violations) if active)
    return (issue_mask, extent, rate, samples, count, nominal,
            None if bitrate is None else int(bitrate == 29), duration, gain, size, status)


def header(path, mode):
    data = path.read_bytes()
    offset = 0
    rows = []
    while mode == "prefix" or offset < len(data):
        fields, values = header_fields(data[offset:offset + 15])
        views = header_views(values)
        rows.append(f"3 header {offset}")
        for inspector in ("owned", "packed"):
            rows.append(f"3 {inspector}")
            rows.extend(f"3 field {index} " + " ".join(map(str, field))
                        for index, field in enumerate(fields))
            rows.append("3 views " + " ".join(map(optional, views)))
        extent = views[1]
        if mode == "prefix" or extent is None or extent < 96:
            break
        offset += extent
    return rows


def unpack(path, packing, mode="payload"):
    raw = path.read_bytes()
    if mode == "frames":
        return sync_frames(raw, packing)
    if packing != "be16" and len(raw) % 2:
        return ["8 odd source byte"]
    little = packing.startswith("le")
    width = 14 if packing.endswith("14") else 16
    if packing == "be16":
        bits = "".join(f"{byte:08b}" for byte in raw)
    else:
        order = "little" if little else "big"
        mask = (1 << width) - 1
        words = (int.from_bytes(raw[index:index + 2], order) & mask
                 for index in range(0, len(raw), 2))
        bits = "".join(f"{word:0{width}b}" for word in words)
    storage = bytes(int(bits[i:i + 8].ljust(8, "0"), 2) for i in range(0, len(bits), 8))
    rows = [f"1 {len(bits)} {storage.hex()}"]
    header_length = 32
    if len(bits) >= 39 and int(bits[:32], 2) == CORE:
        header_length = 104 + 16 * int(bits[38])
    for bit in range(min(header_length, len(bits))):
        if packing == "be16":
            rows.append(f"3 bit {bit} 1 0 {bit // 8} {bit % 8}")
            continue
        word, within = divmod(bit, width)
        byte, in_byte = divmod(16 - width + within, 8)
        rows.append(f"3 bit {bit} 1 0 {2 * word + (1 - byte if little else byte)} {in_byte}")
    return rows


def sync_frames(raw, packing):
    """Byte-exact 14-bit frames with no residual bits in these inputs."""
    order = "little" if packing == "le14" else "big"
    rows = []
    offset = 0
    while offset < len(raw):
        prefix = raw[offset:offset + 10]
        bits = "".join(f"{int.from_bytes(prefix[i:i + 2], order) & 0x3fff:014b}"
                       for i in range(0, len(prefix), 2))
        if len(bits) < 60 or int(bits[:32], 2) != CORE:
            raise ValueError("oracle input needs an aligned DTS core header")
        size = int(bits[46:60], 2) + 1
        words, remaining = divmod(size * 8, 14)
        if remaining or offset + words * 2 > len(raw):
            raise ValueError("oracle input needs a whole number of payload words")
        frame = raw[offset:offset + words * 2]
        packed = 0
        for i in range(0, len(frame), 2):
            packed = (packed << 14) | (int.from_bytes(frame[i:i + 2], order) & 0x3fff)
        rows.append(f"1 frame {offset} {len(frame)} {packed.to_bytes(size, 'big').hex()}")
        offset += len(frame)
    return rows + ["7 end"]
