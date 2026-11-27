"""Bit-order and NAL payload models from H.264 §7.2/7.4.1, Vorbis I §2.1 and RFC 9639 §9.2.7."""
import re
import struct


def bit_readers(path, reader):
    head, body = path.read_bytes().split(b"\n", 1)
    size = int(head)
    data, script = body[:size], body[size:].decode("ascii")
    stream = "".join(
        format(byte, "08b")[::-1] if reader == "lsb" else format(byte, "08b")
        for byte in data
    )
    limit = len(stream)
    position = 0
    overrun = False
    rows = []

    def value_at(start, width):
        field = stream[start:start + width].ljust(width, "0")
        if reader == "lsb":
            field = field[::-1]
        return int(field or "0", 2)

    for line in script.splitlines():
        cmd, *args = line.split()
        width = int(args[0]) if args else 0
        value = None
        if cmd == "position":
            value = position
        elif cmd == "align":
            position = (position + 7) // 8 * 8
            value = 0
        elif cmd == "skip":
            if reader == "lsb" and width <= 32:
                overrun |= position + width > limit
                position = min(limit, position + width)
                value = 0
            elif reader in ("direct", "cached", "rice") or (
                reader == "checked" and position + width <= limit
            ):
                position += width
                value = 0
        elif cmd == "rice" and reader == "rice" and width < 32:
            stop = stream.find("1", position)
            if stop >= 0 and stop + 1 + width <= limit:
                quotient = stop - position
                coded = quotient * (1 << width) + value_at(stop + 1, width)
                # The public reader keeps the low word of codes fitting the current
                # cache. Fixtures with larger values force the refill path.
                if coded < 1 << 32:
                    position = stop + 1 + width
                    value = coded
        elif cmd in ("read", "peek"):
            maximum = 64 if (reader, cmd) == ("lsb", "read") else 32
            minimum = int(cmd == "peek" and reader in ("direct", "cached"))
            if minimum <= width <= maximum:
                if reader == "lsb" and cmd == "read":
                    first = min(width, 32)
                    value = value_at(position, first) if position + first <= limit and not overrun else 0
                    if width > first and position + width <= limit and not overrun:
                        value |= value_at(position + first, width - first) << first
                    overrun |= position + width > limit
                    position = min(position + width, limit)
                elif reader != "checked" or position + width <= limit:
                    value = value_at(position, width)
                    if cmd == "read":
                        position += width
        if reader in ("direct", "cached", "rice"):
            overrun = position > limit
        kind = 8 if value is None else 3
        rows.append(f"{kind} {value or 0} {position} {int(overrun)}")
    return rows


def rbsp(path):
    data = path.read_bytes()
    escape = rb"(?<=\x00\x00)\x03"
    rows = []
    index = 0
    at = 0
    while at < len(data):
        size, = struct.unpack_from(">I", data, at)
        start = at + 4
        at = start + size
        if at > len(data):
            raise ValueError("short NAL payload")
        payload = data[start:at]
        for mode in range(2):
            # Segment boundaries are the actual escape markers in the EBSP,
            # so removal never creates a new marker or a new validation context.
            pieces = re.split(escape, payload)
            result = bytearray()
            reason = 0
            for part, segment in enumerate(pieces):
                if part and not segment:
                    break
                if part and segment[0] > 3:
                    reason = 2
                    break
                illegal = re.search(rb"\x00{2}[\x00-\x02]", segment) if mode == 0 else None
                if illegal:
                    result.extend(segment[:illegal.start() + 2])
                    reason = 1
                    break
                result.extend(segment)
            suffix = f" {result.hex()}" if result else ""
            rows.append(f"{8 if reason else 3} {index} {mode} {reason} {len(result)}{suffix}")
        for part, segment in enumerate(re.split(escape, payload)):
            suffix = f" {segment.hex()}" if segment else ""
            rows.append(f"3 {index} 2 {part} {len(segment)}{suffix}")
        index += 1
    return rows
