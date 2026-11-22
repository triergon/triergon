"""Whole-buffer AV1 OBU and AAC ADTS wire syntax."""

from struct import unpack_from


def row(kind, words, payload=None):
    parts = [str(kind), *(str(word) for word in words)]
    if payload is not None:
        parts.append(str(len(payload)))
        if payload:
            parts.append(payload.hex())
    return " ".join(parts)


def ivf(data):
    if len(data) < 32 or data[:12] != b"DKIF\0\0\x20\0AV01":
        raise ValueError("invalid AV1 IVF envelope")
    at = 32
    while at < len(data):
        size, _timestamp = unpack_from("<IQ", data, at)
        start = at + 12
        at = start + size
        if at > len(data):
            raise ValueError("short IVF frame")
        yield data[start:at]


class Refusal(ValueError):
    """A source unit is incomplete or exceeds the declared reader policy."""


def leb_size(data):
    spelling = next((n for n, octet in enumerate(data[:8], 1) if octet < 128), 0)
    if not spelling:
        raise Refusal("unterminated obu_size")
    size = sum((byte % 128) * 128**n for n, byte in enumerate(data[:spelling]))
    if size >= 2**32:
        raise Refusal("obu_size exceeds the AV1 range")
    return spelling, size


def obu_rows(data, index, single):
    """AV1 v1.0.0 Errata 1, sections 4.10.5 and 5.2–5.3."""
    if single and not data:
        raise Refusal("empty external unit")
    at = 0
    count = 0
    while at < len(data):
        header = data[at]
        has_extension = bool(header & 4)
        size_start = at + 1 + has_extension
        if count == 64 or size_start > len(data):
            raise Refusal("OBU count limit or short header")
        if header & 2:
            width, length = leb_size(data[size_start:])
        elif single:
            width, length = 0, len(data) - size_start
        else:
            raise Refusal("missing size in low-overhead bitstream")
        payload = size_start + width
        end = payload + length
        if end - at > 16 << 20 or end > len(data) or (single and end != len(data)):
            raise Refusal("OBU extent")
        extension = data[at + 1] if has_extension else 256
        issues = (header >> 7) + 2 * (header % 2) + 4 * bool(extension % 8)
        yield row(3, [index, at, payload, end - at, length, header, extension,
                      width, length if width else 0, issues, len(data) if single else 0],
                  data[at:end])
        yield row(1, [index, at], data[payload:end])
        at = end
        count += 1
    yield row(7, [index, count])


def av1_obu(path, framing, window, excluded):
    if framing not in ("low-overhead", "single") or window not in ("1", "7", "4096") or excluded not in ("0", "14"):
        raise ValueError("invalid OBU arguments")
    result = []
    for index, data in enumerate(ivf(path.read_bytes())):
        try:
            result.extend(obu_rows(data, index, framing == "single"))
        except Refusal:
            result.append(row(8, [index]))
    return result


# ISO/IEC 14496-3 ADTS fixed and variable header syntax in wire order.
ADTS_WIDTHS = (12, 1, 2, 1, 2, 4, 1, 3, 1, 1, 1, 1, 13, 11, 2)
FREQUENCIES = (96000, 88200, 64000, 48000, 44100, 32000, 24000,
               22050, 16000, 12000, 11025, 8000, 7350, 0, 0, 0)
CHANNELS = (0, 1, 2, 3, 4, 5, 6, 8)


def adts_fields(header):
    packed = int.from_bytes(header, "big")
    fields = []
    for width in reversed(ADTS_WIDTHS):
        packed, field = divmod(packed, 2**width)
        fields.append(field)
    return list(reversed(fields))


def adts(path):
    data = path.read_bytes()
    if not data:
        return [row(8, [0])]
    at = 0
    count = 0
    rows = []
    while at < len(data):
        header = data[at:at + 7]
        if len(header) != 7:
            return rows + [row(8, [at])]
        fields = adts_fields(header)
        sync, _, layer, protection, profile, rate, _, channel, *_, length, _, blocks = fields
        # Seven fixed bytes plus one optional 16-bit CRC for a single block.
        header_length = 0 if blocks else 9 - 2 * protection
        issues = sum(1 << bit for bit, present in enumerate((
            sync != 0xfff, layer != 0, FREQUENCIES[rate] == 0, profile != 1,
            blocks != 0, header_length != 0 and length <= header_length,
        )) if present)
        rows.append(row(3, [at, *fields, issues, header_length, FREQUENCIES[rate],
                            CHANNELS[channel]], header))
        if issues & 31 or length < header_length or length > len(data) - at:
            return rows + [row(8, [at])]
        crc = 0 if protection else unpack_from(">H", data, at + 7)[0]
        rows.append(row(1, [at, 1 - protection, crc], data[at + header_length:at + length]))
        at += length
        count += 1
    return rows + [row(7, [count])]
