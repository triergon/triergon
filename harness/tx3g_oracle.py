"""ETSI TS 126 245 V18.0.0 §§5.1 and 5.16: text sample descriptions."""
from struct import Struct, error

STYLE = Struct(">HHHBB4B")
DEFAULTS = Struct(">Ibb4B4h")
WORD_MASK = (1 << 64) - 1


def row(kind, words=(), payload=b""):
    fields = [str(kind), *(str(word & WORD_MASK) for word in words)]
    if payload:
        fields.append(payload.hex())
    return " ".join(fields)


def read_font_table(data, at, end, unicode_fonts):
    (count,) = Struct(">H").unpack(data[at:at + 2])
    if count > 4096:
        raise ValueError("font count")
    at += 2
    for _ in range(count):
        font, length = Struct(">HB").unpack(data[at:min(end, at + 3)])
        first = at + 3
        at = first + length
        if at > end:
            raise ValueError("font extent")
        name = data[first:at]
        if unicode_fonts and name.startswith(b"\xfe\xff"):
            name[2:].decode("utf-16-be")
        else:
            name.decode("utf-8")
        yield row(3, (1, font, first, at), name)
    if at != end:
        raise ValueError("font table extent")


def description(data, mode):
    entry = mode == "sample-entry"
    first = 8 if entry else 0
    prefix = (1, int.from_bytes(data[6:8], "big")) if entry else (0,)
    if entry and data[:6] != bytes(6):
        raise ValueError("sample entry reserved bytes")
    defaults = DEFAULTS.unpack_from(data, first)
    style = STYLE.unpack_from(data, first + DEFAULTS.size)
    if any(just not in (-1, 0, 1) for just in defaults[1:3]) or any(style[:2]):
        raise ValueError("default range or justification")
    fonts, children = None, []
    at = first + DEFAULTS.size + STYLE.size
    while at < len(data):
        size, kind = Struct(">I4s").unpack_from(data, at)
        body = at + 8
        if size == 1:
            size, = Struct(">Q").unpack_from(data, body)
            body += 8
        elif size == 0:
            size = len(data) - at
        end = at + size
        if end < body + (16 if kind == b"uuid" else 0) or end > len(data):
            raise ValueError("child extent")
        if len(children) == 4096:
            raise ValueError("child count")
        children.append(row(3, (2, int.from_bytes(kind, "big"), at, end), data[at:end]))
        if kind == b"ftab":
            if fonts is not None:
                raise ValueError("duplicate font table")
            fonts = list(read_font_table(data, body, end, mode == "unicode"))
        at = end
    if fonts is None:
        raise ValueError("font table absent")
    return [row(3, (0, *prefix, *defaults, *style)), *fonts, *children, "7"]


def trace(path, mode):
    data = path.read_bytes()
    try:
        if len(data) > 1024 * 1024:
            raise ValueError("description length")
        return description(data, mode)
    except (ValueError, error):
        return ["8"]
