"""Independent typed Matroska views from RFC 9559 §§5.1.2, 5.1.3.5, 5.1.4, 5.1.6–8."""

from .spec_oracle import integer, variable


def children(data, begin, end):
    """Yield (ID, element start, payload start, end) within an authored parent."""
    while begin < end:
        key, size_at, _ = variable(data, begin, identifier=True)
        size, payload, unknown = variable(data, size_at)
        stop = end if unknown else payload + size
        if not begin < payload <= stop <= end:
            raise ValueError("element exceeds parent")
        yield key, begin, payload, stop
        begin = stop


def segment(path):
    data = path.read_bytes()
    _, _, start, end = next(
        node for node in children(data, 0, len(data)) if node[0] == 0x18538067
    )
    return data, start, list(children(data, start, end))


def matching(data, parent, key):
    return (node for node in children(data, *parent[2:]) if node[0] == key)


def field(data, parent, key):
    return next(matching(data, parent, key), None)


def payload(data, node):
    return None if node is None else data[slice(*node[2:])]


def number(data, parent, key, default=None, empty=0):
    item = field(data, parent, key)
    if item is None:
        return default
    return integer(data, *item[2:]) if item[2] != item[3] else empty


def text(data, parent, key, default=None):
    value = payload(data, field(data, parent, key))
    return default if value is None else value.split(b"\0", 1)[0]


def optional(kind, label, data):
    return row(kind, label, int(data is not None), len(data or b""), data=data)


def row(kind, *words, data=None):
    values = [str(kind), *map(str, words)]
    if data:
        values.append(data.hex())
    return " ".join(values)


def chapter(data, atom, depth):
    end = number(data, atom, 0x92)
    yield row(3, 4, depth, number(data, atom, 0x73C4), number(data, atom, 0x91),
              int(end is not None), end or 0, number(data, atom, 0x98, 0),
              number(data, atom, 0x4598, 1, 1))
    for display in matching(data, atom, 0x80):
        yield optional(3, 5, text(data, display, 0x85))
        ietf = list(matching(data, display, 0x437D))
        legacy = list(matching(data, display, 0x437C))
        languages = [payload(data, lang).split(b"\0", 1)[0]
                     for lang in (ietf or legacy)]
        if not ietf:
            languages = [lang or b"eng" for lang in languages]
        for language in languages or [b"eng"]:
            yield optional(3, 6, language)
    for child in matching(data, atom, 0xB6):
        yield from chapter(data, child, depth + 1)


def simple_tag(data, tag, depth):
    yield row(3, 9, depth, number(data, tag, 0x4484, 1, 1))
    yield optional(3, 10, text(data, tag, 0x45A3))
    legacy = text(data, tag, 0x447A) or b"und"
    yield optional(3, 11, text(data, tag, 0x447B, legacy))
    yield optional(3, 12, text(data, tag, 0x4487))
    yield optional(3, 13, payload(data, field(data, tag, 0x4485)))
    for child in matching(data, tag, 0x67C8):
        yield from simple_tag(data, child, depth + 1)


def metadata_rows(data, documents):
    for doc in documents:
        if doc[0] == 0x1549A966:
            for label, key in enumerate((0x7BA9, 0x4D80, 0x5741)):
                yield optional(3, label, text(data, doc, key))
    for doc in documents:
        if doc[0] != 0x1043A770:
            continue
        for edition in matching(data, doc, 0x45B9):
            uid = number(data, edition, 0x45BC)
            yield row(3, 3, int(uid is not None), uid or 0,
                      number(data, edition, 0x45DB, 0), number(data, edition, 0x45DD, 0))
            for atom in matching(data, edition, 0xB6):
                yield from chapter(data, atom, 0)
    for doc in documents:
        if doc[0] != 0x1254C367:
            continue
        for tag in matching(data, doc, 0x7373):
            targets = field(data, tag, 0x63C0)
            yield row(3, 7, int(targets is not None))
            if targets:
                yield row(3, 8, number(data, targets, 0x68CA, 50, 50))
                yield optional(3, 14, text(data, targets, 0x63CA))
                for kind in (0x63C5, 0x63C9, 0x63C4, 0x63C6):
                    for target in matching(data, targets, kind):
                        yield row(3, 15, kind, integer(data, *target[2:]))
            for value in matching(data, tag, 0x67C8):
                yield from simple_tag(data, value, 0)
    for doc in documents:
        if doc[0] != 0x1941A469:
            continue
        for attachment in matching(data, doc, 0x61A7):
            yield row(2, 4, number(data, attachment, 0x46AE))
            body = payload(data, field(data, attachment, 0x465C))
            yield row(2, 0, len(body), data=body)
            for label, key in enumerate((0x466E, 0x4660, 0x467E), 1):
                yield optional(2, label, text(data, attachment, key))
    yield "7"


def metadata(path, mode):
    if mode != "source":
        raise ValueError("expected source view")
    data, _, documents = segment(path)
    return list(metadata_rows(data, documents))


def presence(node):
    return 0 if node is None else 1 if node[2] == node[3] else 2


def extension_rows(data, tracks):
    for index, entry in enumerate(matching(data, tracks, 0xAE)):
        maximum = field(data, entry, 0x55EE)
        track = payload(data, entry)
        yield row(1, index)
        yield row(2, 0, presence(maximum), number(data, entry, 0x55EE, 0),
                  len(track), data=track)
        unknown = []
        for mapping_index, mapping in enumerate(matching(data, entry, 0x41E4)):
            value = field(data, mapping, 0x41F0)
            value_num = number(data, mapping, 0x41F0, 0)
            if value is not None and value_num < 2:
                raise ValueError("BlockAddIDValue below two")
            kind = field(data, mapping, 0x41E7)
            kind_num = number(data, mapping, 0x41E7, 0)
            raw = data[mapping[1]:mapping[3]]
            yield row(2, 1, presence(value), value_num, presence(kind), kind_num,
                      int(kind_num == 0 and value is not None), len(raw), data=raw)
            yield optional(2, 2, text(data, mapping, 0x41A4))
            yield optional(2, 3, payload(data, field(data, mapping, 0x41A4)))
            yield optional(2, 4, payload(data, field(data, mapping, 0x41ED)))
            unknown.extend((mapping_index, node) for node in children(data, *mapping[2:])
                           if node[0] not in (0x41F0, 0x41E7, 0x41A4, 0x41ED))
        for mapping_index, node in unknown:
            raw = data[node[1]:node[3]]
            body = payload(data, node)
            yield row(2, 5, mapping_index, node[0], len(raw), data=raw)
            yield row(2, 6, len(body), data=body)
    yield "7"


def extensions(path, mode):
    if mode != "source":
        raise ValueError("expected source view")
    data, _, roots = segment(path)
    tracks = next(doc for doc in roots if doc[0] == 0x1654AE6B)
    # The track extension job must finish before the candidate emits the track row.
    try:
        return list(extension_rows(data, tracks))
    except ValueError as error:
        if str(error) == "BlockAddIDValue below two":
            return ["8"]
        raise


def codec_state(path):
    data, start, roots = segment(path)
    tracks = next(doc for doc in roots if doc[0] == 0x1654AE6B)
    indexes = {number(data, entry, 0xD7): index
               for index, entry in enumerate(matching(data, tracks, 0xAE))}
    active = [None] * len(indexes)
    rows = []

    def state_row(kind, index):
        state = active[index]
        if state is None:
            return row(kind, index, 0)
        _, header, at, end = state
        return row(kind, index, 1, end - at, 1, 1, header, at,
                   end - at, header - start, data=data[at:end])

    def configuration():
        rows.extend(state_row(2, index) for index in range(len(active)))

    configuration()
    for cluster in (doc for doc in roots if doc[0] == 0x1F43B675):
        for node in children(data, *cluster[2:]):
            if node[0] == 0xA3:
                block, state = node, None
            elif node[0] == 0xA0:
                block, state = field(data, node, 0xA1), field(data, node, 0xA4)
            else:
                continue
            track, header_end, _ = variable(data, block[2])
            index = indexes[track]
            if state is not None:
                active[index] = state
                configuration()
            flags = data[header_end + 2]
            count = data[header_end + 3] + 1 if flags & 6 else 1
            rows.extend(state_row(1, index) for _ in range(count))
    return rows + ["7"]
