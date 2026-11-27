"""Carrier syntax from ITU-T H.222.0 (08/2023), 2.4.3.2–2.4.3.5."""

from fractions import Fraction
from itertools import islice


class InvalidCarrier(ValueError):
    pass


def clock(octets):
    if len(octets) != 6:
        raise InvalidCarrier()
    word = int.from_bytes(octets, "big")
    base, reserved, extension = word >> 15, (word >> 9) % 64, word % 512
    if reserved != 63 or extension >= 300:
        raise InvalidCarrier()
    ticks = 300 * base + extension
    time = Fraction(ticks, 27_000_000)
    return [1, base, extension, ticks, time.numerator, time.denominator]


def row(*words):
    return " ".join(map(str, words))


def carrier(wire, offset):
    prefix, ts = wire[:-188], wire[-188:]
    header = int.from_bytes(ts[:4], "big")
    sync, tei, start, priority, pid, scrambling, control, count = [
        header >> shift & (1 << width) - 1
        for shift, width in [(24, 8), (23, 1), (22, 1), (21, 1), (8, 13), (6, 2), (4, 2), (0, 4)]
    ]
    if sync != 0x47 or control == 0:
        raise InvalidCarrier()
    payload_start = 4
    fields = []
    blocks = [(0, prefix)] if prefix else []
    if control in (2, 3):
        length = ts[4]
        if not (length == 183 if control == 2 else length <= 182):
            raise InvalidCarrier()
        payload_start = 5 + length
        adaptation = iter(ts[5:payload_start])
        flags = next(adaptation, 0)
        has_pcr, has_opcr = flags & 16, flags & 8
        if has_opcr and not has_pcr:
            raise InvalidCarrier()

        def take(size):
            data = bytes(islice(adaptation, size))
            if len(data) != size:
                raise InvalidCarrier()
            return data

        # Each optional field appears in table order inside adaptation_field().
        fields = [length, flags]
        fields.extend(clock(take(6)) + [63] if has_pcr else [0])
        fields.extend(clock(take(6)) if has_opcr else [0])
        if flags & 4:
            signed = int.from_bytes(take(1), "big", signed=True)
            fields.append(signed % (1 << 64))
        for flag, index in [(2, 1), (1, 2)]:
            if flags & flag:
                blocks.append((index, take(next(adaptation))))
        if any(value != 255 for value in adaptation):
            raise InvalidCarrier()
    payload_length = 188 - payload_start if control & 1 else 0
    result = [row(6, offset, len(wire), sync, tei, start, priority, pid,
                  scrambling, control, count, offset + len(prefix) + payload_start,
                  payload_length, *fields)]
    for kind, data in blocks:
        result.append(row(3, offset, kind, len(data), data.hex()) if data
                      else row(3, offset, kind, 0))
    return result


def trace(path, size, window, mode):
    size = int(size)
    if size not in (188, 192) or window not in ("1", "7", "4096") or mode not in ("fields", "compact"):
        raise ValueError("packet view requires FORMAT WINDOW fields|compact")
    data = path.read_bytes()
    rows = []
    complete = len(data) // size * size
    for offset in range(0, complete, size):
        try:
            rows.extend(carrier(data[offset:offset + size], offset))
        except (InvalidCarrier, StopIteration):
            rows.append(row(8, offset, size))
    rows.append(row(8, complete, len(data) - complete) if complete < len(data) else "7")
    return rows
