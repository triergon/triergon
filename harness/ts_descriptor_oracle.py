"""AVC and HEVC PMT signalling, ITU-T H.222.0 (08/2023) Tables 2-92 and 2-113."""

from collections import defaultdict


def sections(data, stride):
    pending = defaultdict(bytearray)
    prefix = stride - 188
    for start in range(prefix, len(data) - 187, stride):
        packet = data[start:start + 188]
        pid = int.from_bytes(packet[1:3], "big") & 8191
        control = (packet[3] >> 4) & 3
        if packet[0] != 0x47 or packet[1] & 128 or packet[3] & 192 or not control & 1:
            continue
        offset = 4 if control == 1 else 5 + packet[4]
        payload = packet[offset:]
        if not payload:
            continue
        buffer = pending[pid]
        if packet[1] & 64:
            first = 1 + payload[0]
            if first > len(payload):
                buffer.clear()
                continue
            if buffer:
                buffer.extend(payload[1:first])
                yield from complete(pid, buffer)
            buffer.clear()
            payload = payload[first:]
        elif not buffer:
            continue
        buffer.extend(payload)
        yield from complete(pid, buffer)


def complete(pid, buffer):
    while len(buffer) >= 3 and buffer[0] != 255:
        end = 3 + (int.from_bytes(buffer[1:3], "big") & 4095)
        if end > len(buffer):
            return
        section = bytes(buffer[:end])
        del buffer[:end]
        if len(section) >= 12 and valid_crc(section):
            yield pid, section
    if buffer and buffer[0] == 255:
        buffer.clear()


def valid_crc(section):
    # Annex A polynomial remainder, with the initial register of all ones.
    message = int.from_bytes(section, "big") ^ (0xffffffff << (8 * (len(section) - 4)))
    polynomial = 0x104c11db7
    while message.bit_length() > 32:
        message ^= polynomial << (message.bit_length() - 33)
    return message == 0


def syntax(data, widths):
    word = int.from_bytes(data, "big")
    left = len(data) * 8
    for width in widths:
        left -= width
        if left < 0:
            raise ValueError("short descriptor")
        yield (word >> left) % (1 << width)


def descriptor_fields(tag, data):
    if tag == 0x28:
        if len(data) != 4 or data[3] % 32 != 31:
            raise ValueError("malformed AVC video descriptor")
        return list(syntax(data, [8, 1, 1, 1, 1, 1, 1, 2, 8, 1, 1, 1]))
    if len(data) < 13:
        raise ValueError("short HEVC video descriptor")
    fields = list(syntax(data, [2, 1, 5, 32, 1, 1, 1, 1, 44, 8, 1, 1, 1, 1, 2, 2]))
    subset, still, picture, sub_hrd, reserved, hdr = fields[10:]
    if reserved != 3 or len(data) != 13 + 2 * subset:
        raise ValueError("malformed HEVC video descriptor")
    result = fields[:10] + [still, picture, sub_hrd, hdr, subset]
    if subset:
        low, low_reserved, high, high_reserved = syntax(data[13:], [3, 5, 3, 5])
        if low_reserved != 31 or high_reserved != 31 or low > high:
            raise ValueError("malformed temporal ID range")
        result.extend([low, high])
    return result


def pmt_descriptors(section, program):
    version = section[5] >> 1 & 31
    end = len(section) - 4
    pos = 12 + (int.from_bytes(section[10:12], "big") & 4095)
    rows = []
    while pos < end:
        if pos + 5 > end:
            raise ValueError("short stream header")
        pid = int.from_bytes(section[pos + 1:pos + 3], "big") & 8191
        count = int.from_bytes(section[pos + 3:pos + 5], "big") & 4095
        pos += 5
        limit = pos + count
        if limit > end:
            raise ValueError("truncated descriptor loop")
        while pos < limit:
            if pos + 2 > limit:
                raise ValueError("short descriptor header")
            tag, length = section[pos:pos + 2]
            pos += 2
            stop = pos + length
            if stop > limit:
                raise ValueError("short descriptor body")
            if tag in (0x28, 0x38):
                fields = descriptor_fields(tag, section[pos:stop])
                rows.append(" ".join(map(str, [3, program, version, pid, tag, *fields])))
            pos = stop
    return rows


def trace(path, stride, program, namespace, policy):
    stride = int(stride)
    if stride not in (188, 192) or program != "1" or namespace != "unspecified" or policy != "strict":
        raise ValueError("unexpected video-descriptor profile")
    data = path.read_bytes()
    routes = {}
    rows = []
    for pid, section in sections(data, stride):
        if not section[5] & 1:
            continue
        if pid == 0 and section[0] == 0 and section[6:8] == b"\x00\x00":
            routes = {
                int.from_bytes(section[i:i + 2], "big"): int.from_bytes(section[i + 2:i + 4], "big") & 8191
                for i in range(8, len(section) - 4, 4)
            }
        if section[0] == 2 and section[6:8] == b"\x00\x00":
            number = int.from_bytes(section[3:5], "big")
            if pid == routes.get(number):
                try:
                    rows.extend(pmt_descriptors(section, number))
                except ValueError:
                    continue
    rows.append("8" if len(data) % stride else "7")
    return rows
