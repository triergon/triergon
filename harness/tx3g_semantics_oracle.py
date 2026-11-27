"""ETSI TS 126 245 §§5.16–5.18, resolved per Unicode character.

The model expands source ranges into character membership arrays. It does not
simulate the incremental job, merge equal styles into runs, or use its output.
"""
from dataclasses import dataclass, field
from struct import Struct, error

from harness.tx3g_oracle import DEFAULTS, STYLE, description, row


@dataclass
class Track:
    style: tuple | None = None
    flags: int = 0
    textbox: tuple | None = None
    disparity: int | None = None
    fonts: dict = field(default_factory=dict)
    font_rows: list = field(default_factory=list)


def children(data, start):
    while start < len(data):
        size, typ = Struct(">I4s").unpack_from(data, start)
        first = start + 8
        if size == 1:
            size, = Struct(">Q").unpack_from(data, first)
            first += 8
        elif size == 0:
            size = len(data) - start
        if typ == b"uuid":
            first += 16
        end = start + size
        if not first <= end <= len(data):
            raise ValueError("child extent")
        yield typ, data[first:end]
        start = end


def read_track(data):
    if data is None:
        return Track()
    checked = description(data, "unicode")
    fixed = DEFAULTS.unpack_from(data)
    track = Track(STYLE.unpack_from(data, DEFAULTS.size)[2:], fixed[0], fixed[7:])
    for entry in checked[1:]:
        fields = entry.split()
        if fields[:2] == ["3", "1"]:
            ident, start, end = map(int, fields[2:5])
            if ident in track.fonts:
                raise ValueError("duplicate font ID")
            name = data[start:end]
            bom = name.startswith(b"\xfe\xff")
            utf8 = name[2:].decode("utf-16-be").encode() if bom else name
            track.fonts[ident] = len(track.fonts)
            track.font_rows.append(row(3, (2, ident, start, end, bom, len(utf8)), utf8))
    if track.style[0] not in track.fonts:
        raise ValueError("missing default font")
    for typ, body in children(data, 30):
        if typ == b"disp":
            if track.disparity is not None:
                raise ValueError("duplicate track disparity")
            track.disparity, = Struct(">h").unpack(body)
    return track


def read_href(body):
    start, end, size = Struct(">HHB").unpack_from(body)
    stop = 5 + size
    url = body[5:stop]
    alt_size = body[stop]
    alt = body[stop+1:]
    if len(alt) != alt_size:
        raise ValueError("href length")
    url.decode("utf-8")
    alt.decode("utf-8")
    return start, end, url, alt


def sample_rows(data, track):
    length, = Struct(">H").unpack_from(data)
    if length+2 > len(data):
        raise ValueError("text length")
    encoded = data[2:2+length]
    text = (encoded[2:].decode("utf-16-be") if encoded.startswith(b"\xfe\xff")
            else encoded.decode("utf-8"))
    count = len(text)
    layers = {kind: [None] * count for kind in (b"styl",b"hlit",b"blnk",b"href",b"krok")}
    last = dict.fromkeys(layers, 0)
    counts = dict.fromkeys(layers, 0)
    single = {b"hclr":">4B", b"dlay":">I", b"tbox":">4h", b"twrp":">B", b"disp":">h"}
    globals_, links, events = {}, [], []
    seen_karaoke = False

    def apply(kind, start, end, payload):
        allowed = count + (kind in (b"hlit",b"krok"))
        if not last[kind] <= start <= end <= allowed or start > count:
            raise ValueError("range order or extent")
        last[kind] = end
        index = counts[kind]
        counts[kind] += 1
        for char in range(start, min(count,end)):
            layers[kind][char] = index, payload

    for typ, body in children(data, 2+length):
        if typ in single:
            if typ in globals_:
                raise ValueError("duplicate sample modifier")
            values = Struct(single[typ]).unpack(body)
            if typ == b"twrp" and values[0] not in (0,1):
                raise ValueError("reserved wrap")
            globals_[typ] = values
        elif typ == b"styl":
            total, = Struct(">H").unpack_from(body)
            styles = list(STYLE.iter_unpack(body[2:]))
            if len(styles) != total:
                raise ValueError("style count")
            for start, end, *style in styles:
                if track.style is not None and style[0] not in track.fonts:
                    raise ValueError("missing style font")
                apply(typ,start,end,style)
        elif typ in (b"hlit",b"blnk"):
            start, end = Struct(">HH").unpack(body)
            apply(typ,start,end,None)
        elif typ == b"href":
            start, end, url, alt = read_href(body)
            apply(typ,start,end,None)
            links.append((start,end,url,alt))
        elif typ == b"krok":
            if seen_karaoke:
                raise ValueError("duplicate karaoke")
            seen_karaoke = True
            tick, total = Struct(">IH").unpack_from(body)
            entries = list(Struct(">IHH").iter_unpack(body[6:]))
            if len(entries) != total:
                raise ValueError("karaoke count")
            for finish, start, end in entries:
                if finish < tick:
                    raise ValueError("karaoke time order")
                apply(typ,start,end,None)
                events.append((start,end,tick,finish))
                tick = finish

    active_events = [(start,min(end,count),begin,finish) for start,end,begin,finish in events
                     if begin < finish]
    highlight = layers[b"hlit"]
    hyperlink = layers[b"href"]
    display = track.style is not None
    continuous = bool(track.flags & 0x800)
    conditional = True
    for start, end, _, _ in active_events:
        if any(highlight[i] or hyperlink[i] for i in range(0 if continuous else start,end)):
            raise ValueError("overlapping karaoke and static intent")
        if not display and any(highlight[i] or hyperlink[i] for i in range(start)):
            conditional = False

    utf8 = text.encode()
    rows = [row(3,(0,count,len(utf8)),utf8)]
    delay, = globals_.get(b"dlay",(0,))
    wrap, = globals_.get(b"twrp",(-1,))
    fields = (1,delay,1 if b"dlay" in globals_ else 3,wrap+1,display,track.flags,display,conditional)
    if b"tbox" in globals_:
        fields += (1,*globals_[b"tbox"])
    elif display:
        fields += (2,*track.textbox)
    else:
        fields += (0,)
    if b"disp" in globals_ or display:
        shift, = globals_.get(b"disp",(track.disparity or 0,))
        origin = 1 if b"disp" in globals_ else 2 if track.disparity is not None else 3
        pixels = int(shift/16)
        fields += (origin,shift,-pixels,pixels)
    else:
        fields += (0,)
    rows.extend([row(3,fields),*track.font_rows])
    for index,(start,end,url,alt) in enumerate(links):
        rows.extend([row(3,(3,index,start,end,len(url)),url),row(3,(4,index,len(alt)),alt)])
    ticks = {0}
    for start,end,begin,finish in events:
        ticks.update((begin,finish))
        if finish:
            ticks.add(finish-1)
        rows.append(row(3,(5,start,end,end==count+1,begin,finish)))

    def optional(value):
        return (0,) if value is None else (1,value)

    colour = globals_.get(b"hclr")
    for char in range(count):
        style = layers[b"styl"][char]
        requested = style[1] if style else track.style
        source = (2,style[0]) if style else (1,) if display else (0,)
        font = track.fonts.get(requested[0]) if requested else None
        link = hyperlink[char]
        has_highlight = highlight[char] is not None
        blink = layers[b"blnk"][char] is not None
        for tick in (None,*sorted(ticks)):
            karaoke = 0
            for start,end,begin,finish in active_events:
                if char < end and (char >= start or continuous or not display):
                    if tick is None:
                        karaoke = 2
                    elif begin <= tick < finish:
                        karaoke = 1 if char >= start or continuous else 2
            if karaoke == 2:
                paint = (5,)
            elif link:
                paint = (4,)
            elif karaoke:
                paint = (3,1,*colour) if colour else (3,0)
            elif has_highlight:
                paint = (1,*colour) if colour else (2,)
            else:
                paint = (0,)
            style_fields = (1,*requested) if requested is not None else (0,)
            rows.append(row(3,(6,char,tick is not None,tick or 0,*style_fields,*source,
                               *optional(font),*optional(link[0] if link else None),
                               blink,has_highlight,karaoke,*paint)))
    return [*rows,"7"]


def trace(path, timescale):
    data = path.read_bytes()
    length, = Struct(">I").unpack_from(data)
    size = 0 if length == 0xffffffff else length
    try:
        if timescale != "-" and not int(timescale):
            raise ValueError("zero timescale")
        return sample_rows(data[4+size:],read_track(data[4:4+size] if size or length == 0 else None))
    except (ValueError,error,IndexError):
        return ["8"]
