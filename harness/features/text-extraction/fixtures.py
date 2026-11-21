from pathlib import Path
import argparse
import hashlib
import json
import random
from collections import defaultdict
import struct
from harness.subtitles.pgs_streams import element as e, uint as u
from harness.subtitles import dvb_streams as dvb
from harness.subtitle_corpus import tx3g_description, tx3g_sample, tx3g_box


root = Path('crates/subtitles/tests/text-fixtures/fixtures')

def box(kind, data):
    return struct.pack('>I4s', len(data)+8, kind) + data

def ints(*values):
    return struct.pack('>'+'I'*len(values), *values)

def mov(samples, description, scale=1000):
    if not samples:
        samples = [(b"\x00\x00", 1000)]
    payload = b"".join(data for data, _ in samples)
    total = sum(duration for _, duration in samples)
    matrix = ints(65536,0,0,0,65536,0,0,0,1073741824)
    ftyp = box(b'ftyp', b'isom'+ints(0)+b'isom')
    mvhd = box(b'mvhd', ints(0,0,0,scale,total,65536,16777216,0,0)+matrix+bytes(24)+ints(2))
    tkhd = box(b'tkhd', ints(3,0,0,1,0,total,0,0,0,0)+matrix+ints(720<<16,480<<16))
    mdhd = box(b'mdhd', ints(0,0,0,scale,total,0x55c40000))
    hdlr = box(b'hdlr', ints(0,0)+b'text'+bytes(12)+b'Timed text\x00')
    stsd = box(b'stsd', ints(0,1)+box(b'tx3g', bytes(6)+b'\x00\x01'+description))
    stts = ints(0, len(samples)) + b"".join(ints(1, duration) for _, duration in samples)
    stsz = ints(0, 0, len(samples)) + ints(*(len(data) for data, _ in samples))
    stbl = box(b"stbl", stsd + box(b"stts", stts) + box(b"stsc", ints(0,1,1,len(samples),1))
               + box(b"stsz", stsz) + box(b"stco", ints(0,1,len(ftyp)+8)))
    minf = box(b'minf',box(b'nmhd',ints(0))+box(b'dinf',box(b'dref',ints(0,1)+box(b'url ',ints(1))))+stbl)
    return ftyp+box(b'mdat',payload)+box(b'moov',mvhd+box(b'trak',tkhd+box(b'mdia',mdhd+hdlr+minf)))

def mkv(codec, data, private=None, addition=None, attachments=(), pts=0, duration=1000, tags=b""):
    header = e(0x1A45DFA3, u(0x4286,1)+u(0x42F7,1)+u(0x42F2,4)+u(0x42F3,8)+e(0x4282,b'matroska')+u(0x4287,4)+u(0x4285,2))
    info = e(0x1549A966,u(0x2AD7B1,1000000)+e(0x4D80,b'fixture')+e(0x5741,b'fixture'))
    tracks = e(0x1654AE6B,e(0xAE,u(0xD7,1)+u(0x73C5,1)+u(0x83,17)+u(0x9C,0)+e(0x86,codec)
                        +(u(0x55ee,addition[0]) if addition is not None else b"")
                        +(e(0x63A2,private) if private is not None else b"")))
    block = e(0xA1,b'\x81'+struct.pack('>hB',min(pts,0),0)+data)
    if duration is not None:
        block += u(0x9B,duration)
    if addition is not None:
        ident, extra = addition
        block += e(0x75A1,e(0xA6,u(0xEE,ident)+e(0xA5,extra)))
    attached = b''.join(e(0x61A7,e(0x466E,name)+e(0x4660,mime)+e(0x465C,value)+u(0x46AE,i)) for i,(name,mime,value) in enumerate(attachments,1))
    return header+e(0x18538067,info+tracks+(e(0x1941A469,attached) if attached else b'')+tags+e(0x1F43B675,u(0xE7,max(pts,0))+e(0xA0,block)))

TARGETS = ["aarch64-unknown-linux-gnu", "thumbv7neon-unknown-linux-gnueabihf"]
PENDING = "paired runtime, heap and total RSS collections are pending on isolated gate hosts"
FEATURES = {
    "text-extraction": ("text-extraction", "text-extract-", None, PENDING,
        "Demux the selected subtitle track and project each sample as source text. Compare kind 1 rows with raw PTS and duration in the stream time base, UTF-8 text or opaque ASS bytes, and the WebVTT comment byte count. Kind 3 rows carry the identifier and settings. Kind 8 marks a refused sample. Matroska type-1 BlockAdditional is split as settings, identifier, comments when typed side data is absent."),
    "text-extraction-oracle": ("text-extraction", "text-extract-", "text-extraction",
        "the independent source-fact oracle has no comparable runtime or memory workload",
        "Compare the 40 source packets before container clock conversion. Fold raw and text ranges, encoding, exact times, raw source clock, decoded bytes, identifier, settings, full comments and tx3g modifier kinds and ranges."),
    "tx3g-configuration": ("tx3g-config", "tx3g-config-", None,
        "the reference reads the sample description while opening its decoder",
        "Compare Default ASS style projected from the sample description after a mov_text decoder open: size, inverted alpha with BGR primary, inverted alpha with BGR background, three face flags, justification mapped to an ASS alignment and the selected font name. The candidate uses the declared font ID and retains a missing name as empty."),
    "tx3g-config-oracle": ("tx3g-config", "tx3g-config-", "tx3g-config",
        "the reference does not publish the complete sample description",
        "Compare all fixed fields, font identifiers and raw font-name ranges and bytes, and the kinds, ranges and bytes of all child boxes, under ETSI TS 126 245 §5.16. Test the defaults record, sample entry body, and Unicode font mode."),
    "renderer-context": ("renderer-context", "renderer-context-", None, PENDING,
        "Open Matroska, select the first ASS or SSA track and collect the header and font attachments in container order. Compare kind 2 header bytes and per font filename and data by MIME metadata. Track tags and non-font attachments reach the builder."),
    "dvb-source": ("dvb-source", "dvb-source-", None, PENDING,
        "Open TS or Matroska and compare the first DVB track: the reference time base, each service's composition and ancillary page and type, the language list and each demuxed packet's raw PTS, DTS, duration and bytes."),
    "pgs-segments": ("pgs-segments", "pgs-segments-", "pgs-segments",
        "the reference publishes decoded subtitles; subtitle-pgs compares its consumption of the segment fields",
        "Compare each packet or SUP envelope and every typed composition, palette, object fragment and window field. The independent oracle encodes the public HDMV presentation graphics description at https://www.blu-ray.org.uk/2025/11/presentation-graphics.html and uses the pinned formats ffprobe only for container packet extraction. Includes raw clocks, complete segment bytes, payload lengths, unknown kinds and truncations."),
    "vobsub-index": ("vobsub-index", "vobsub-lines-", None,
        "the reference reads the index by opening the idx/sub pair, including its program stream",
        "Project setting lines before the first id as LF-terminated extradata, each track as id, language and default disposition, and timestamps with delay as millisecond PTS and sub file offsets. Compare the public VobSub demuxer output. Blank and comment counts are read independently from the raw index."),
    "charset-streams": ("charset-streams", "charset-", None,
        "the reference converts inside an ASS decoder call, packet by packet",
        "Compare complete UTF-8 conversions for every single byte, every byte pair in the multibyte profiles, and seeded streams. The ASS decoder exposes the visible string. Public glibc iconv supplies complete bytes after NUL and the first refused sequence's byte offset; the worker checks the visible prefix against ASS when present."),
    "charset-packets": ("charset-packets", "charset-packets-", None, PENDING,
        "Compare each labelled ASS packet through AssPacketJob and avcodec_decode_subtitle2 with sub_charenc before decoding. Compare the visible ASS packet up to its first NUL, or refusal. The Linux/glibc profile uses the expanded EUC-KR repertoire."),
    "carriage-trace": ("carriage-trace", "carriage-", None,
        "the carriage snapshot is a comparison-only projection",
        "Compare FFmpeg format-open and terminal return codes, stream codec, time base and private data, then packet ordinal, position, PTS, DTS, duration, flags, payload and ordered side data. The candidate uses document snapshots or demuxed sample snapshots from the carriage session."),
    "tx3g-semantics": ("tx3g-semantics", "tx3g-semantics-", "tx3g-semantics",
        "no pinned reference resolves 3GPP timed text semantics",
        "Resolve typed tx3g modifier boxes with the ETSI TS 126 245 §5.17 rules and the documented semantics policy. Compare each requested character's independently resolved effects and styles."),
}


def source_manifest(feature):
    return json.loads(Path(f"harness/features/{feature}/corpus.json").read_text())["cases"]


def capsule(data):
    if data[:8] != b"TRSTXT01":
        raise ValueError("text capsule marker")
    codec, label_size = struct.unpack_from(">BH", data, 8)
    label = data[11:11 + label_size]
    at = 11 + label_size
    count, = struct.unpack_from(">I", data, at)
    at += 4
    blocks = []
    for _ in range(count):
        kind = data[at]
        at += 1
        if kind == ord("O"):
            num, den, width, height, private, size = struct.unpack_from(">IIIIBI", data, at)
            at += 21
            blocks.append(("open", (num, den, width, height, data[at:at + size] if private else None)))
            at += size
        elif kind == ord("P"):
            known, pts, has_duration, duration, size = struct.unpack_from(">BqBqI", data, at)
            at += 22
            blocks.append(("packet", (pts if known else None, duration if has_duration else None, data[at:at + size])))
            at += size
        elif kind == ord("F"):
            blocks.append(("flush", None))
        else:
            raise ValueError(f"unknown text capsule operation {kind}")
    if at != len(data):
        raise ValueError("text capsule extent")
    return codec, label, blocks


def write_corpora(out):
    cases = defaultdict(list)

    def file(feature, name, data):
        relative = Path("harness/features") / feature / "inputs" / name
        dest = out / relative
        dest.parent.mkdir(parents=True, exist_ok=True)
        dest.write_bytes(data)
        return relative

    def add(feature, name, path, arguments, *, profile=None, source=None, companions=()):
        path = Path(path)
        data_path = out / path
        if not data_path.exists():
            data_path = path
        digest = hashlib.sha256(data_path.read_bytes()).hexdigest()
        if source and source["sha256"] != digest:
            raise ValueError(f"{path} does not match its source manifest")
        row = dict(id=name, profile=profile or name, file=path.as_posix(),
                   sha256=digest,
                   rights=source["rights"] if source else "authored-synthetic",
                   arguments=arguments)
        if source and "source" in source:
            row["source"] = source["source"]
        if companions:
            row["companions"] = list(companions)
        cases[feature].append(row)

    default = tx3g_description()
    packet_formats = {"Tx3g":"tx3g", "Utf8Packet":"utf8", "MatroskaWebVtt":"vtt", "OpaqueAss":"ass"}
    ids = {"utf8":b"S_TEXT/UTF8", "vtt":b"S_TEXT/WEBVTT", "ass":b"S_TEXT/ASS"}
    def value(time):
        return "-" if time is None else f"{time[0]}/{time[1]}"

    for item in json.loads((root/"manifest.json").read_text())["cases"]:
        name, fmt = item["id"], packet_formats[item["format"]]
        path = root/item["input"]
        packet = path.read_bytes()
        clock = item["clock"]
        raw_clock = "-" if clock is None else ":".join(
            "-" if clock[key] is None else str(clock[key])
            for key in ("numerator", "denominator", "pts_ticks", "dts_ticks", "duration_ticks"))
        extra = item["addition"]
        extra = None if extra is None else (extra["id"], (root/extra["path"]).read_bytes())
        extra_arg = "-" if extra is None else f"{extra[0]}:{extra[1].hex()}"
        add("text-extraction-oracle", name, path,
            ["raw", fmt, value(item["start"]), value(item["duration"]), raw_clock,
             str(item["input_limit"] or "-"), extra_arg], profile=fmt)
        if fmt == "tx3g":
            wrapped = file("text-extraction", f"{name}.mp4", mov([(packet, 1001)], default))
            args = ["mov", fmt]
        else:
            private = item.get("codec_private")
            codec = item.get("codec_id")
            def ticks(v):
                return None if v is None else v[0] * 1000 // v[1]
            wrapped = file("text-extraction", f"{name}.mkv", mkv(
                (root/codec).read_bytes() if codec else ids[fmt], packet,
                (root/private).read_bytes() if private else None, extra,
                pts=ticks(item["start"]) or 0, duration=ticks(item["duration"])))
            args = ["mkv", fmt]
        add("text-extraction", name, wrapped, args, profile=fmt)
        add("carriage-trace", name, wrapped, [args[0], "header"], profile=args[0])
        if fmt == "ass":
            add("renderer-context", name, wrapped, [], profile="text-attachments")

    for item in json.loads((root/"manifest.json").read_text())["documents"]:
        mode = {"SubRip":"srt", "WebVtt":"vtt"}[item["format"]]
        for kind in ("header", "streaminfo"):
            add("carriage-trace", f"{item['id']}-{kind}", root/item["input"], [mode, kind], profile=mode)

    text_cases = source_manifest("subtitle-text")
    for item in text_cases:
        if item.get("group") not in ("mov-text", "mov-text-tags", "tx3g", "charset"):
            continue
        name = item["id"]
        _, label, blocks = capsule(Path(item["file"]).read_bytes())
        if item["group"] == "charset":
            packets = [data[2] for kind, data in blocks if kind == "packet"]
            if len(packets) != 1:
                raise ValueError("one packet per labelled case")
            path = file("charset-packets", name + ".assp", packets[0])
            add("charset-packets", name, path, ["glibc", label.decode("ascii"), "packet"], profile="receiver")
            continue
        samples, config, rate = [], None, 1000
        for kind, data in blocks:
            if kind == "open":
                num, den, _, _, description = data
                if num and den:
                    rate = max(1, den // num)
                if description is not None:
                    config = description
            if kind == "packet":
                _, duration, sample = data
                samples.append((sample, max(0, min(0xffffffff, duration if duration is not None else rate))))
        wrapped = file("text-extraction", f"capsule-{name}.mp4",
                       mov(samples, config if config is not None else default, rate))
        add("text-extraction", "capsule-" + name, wrapped, ["mov", "tx3g"], profile="tx3g")
        add("carriage-trace", "capsule-" + name, wrapped, ["mov", "header"], profile="mov")

    # Sessions with no description still exercise the explicit absence case.
    native = Path("crates/subtitles/tests/native-mov-text-fixtures")
    empty = file("tx3g-configuration", "absent.bin", b"")
    for directory in sorted(path for path in native.iterdir() if path.is_dir()):
        name = directory.name
        path = directory/"extradata.bin"
        if not path.exists():
            path = empty
        add("tx3g-configuration", name, path, ["ass-header"], profile="recorded")
        add("tx3g-config-oracle", name, path, ["defaults"], profile="recorded")
    variants = {"base":default, "empty-fonts":tx3g_description(fonts=()),
                "duplicate-font-id":tx3g_description(fonts=((1,b"One"), (1,b"Two"))),
                "utf16-font":tx3g_description(fonts=((1,b"\xfe\xff\x00A"),)),
                "face":tx3g_description(face=7), "alpha":tx3g_description(rgb=(17,33,55),alpha=0),
                "sample-entry":bytes(6)+b"\xff\xff"+default,
                "box-to-end":default[:30]+bytes(4)+default[34:],
                "extended-box":default[:30]+struct.pack(">I4sQ",1,b"ftab",len(default)-22)+default[38:],
                "opaque-box":default+box(b"free",b"data"),
                "uuid-box":default+box(b"uuid",bytes(16)+b"data"),
                "duplicate-table":default+default[30:]}
    for key, offset, byte in [
            ("flag-high",0,255), ("h-left",4,0), ("h-right",4,255), ("h-center",4,1),
            ("v-top",5,0), ("v-center",5,1), ("v-bottom",5,255),
            ("background-red",6,255), ("background-green",7,255), ("background-blue",8,255),
            ("background-alpha",9,0), ("top-min",10,128), ("left-min",12,128),
            ("bottom-max",14,127), ("right-max",16,127), ("default-range",19,1),
            ("missing-font",23,2), ("face-high",24,255), ("font-size-zero",25,0),
            ("font-size-high",25,255), ("bad-justification",4,2)]:
        value = bytearray(default)
        value[offset] = byte
        variants[key] = value
    for cut in range(len(default)):
        variants[f"truncated-{cut:02d}"] = default[:cut]
    for name, data in variants.items():
        path = file("tx3g-configuration", name+".bin", data)
        mode = {"sample-entry":"sample-entry", "utf16-font":"unicode"}.get(name,"defaults")
        add("tx3g-config-oracle", "authored-"+name, path, [mode], profile="authored")
        if mode == "defaults":
            add("tx3g-configuration", "authored-"+name, path, ["ass-header"], profile="authored")

    font_source = Path("crates/subtitles/tests/context-fixtures/input")
    header = (font_source/"authored-ass-header.bin").read_bytes()
    payload = (font_source/"packet.bin").read_bytes()
    font = (font_source/"opaque-font-probe.bin").read_bytes()
    other = (font_source/"opaque-unknown-attachment.bin").read_bytes()
    mimes = [b"application/x-truetype-font", b"font/ttf", b"font/otf", b"application/vnd.ms-opentype",
             b"application/x-font-ttf", b"application/x-font-opentype", b"font/collection", b"application/octet-stream"]
    attachments = [(f"attachment-{index}.ttf".encode(), mime, other if index == 7 else font)
                   for index, mime in enumerate(mimes)]
    tags = e(0x1254C367, e(0x7373, e(0x63C0,u(0x63C5,1)) +
             e(0x67C8,e(0x45A3,b"TITLE") + e(0x4487,b"Authored subtitle track"))))
    path = file("renderer-context", "attachments.mkv",
                mkv(b"S_TEXT/ASS", payload, header, attachments=attachments, tags=tags))
    add("renderer-context", "attachments", path, [], profile="attachments")
    chunk_paths = set(Path("harness/fixtures/ass-render").glob("*.chunks"))
    chunk_paths.update(Path("harness/fixtures/ass").rglob("*.chunks"))
    for path in sorted(chunk_paths):
        text = path.read_bytes()
        if b"@private\n" not in text:
            continue
        head = text.split(b"@private\n",1)[1].split(b"\n@end",1)[0]
        name = path.parent.name + "-" + path.stem
        wrapped = file("renderer-context", name+".mkv", mkv(b"S_TEXT/ASS", payload, head))
        add("renderer-context", name, wrapped, [], profile="streams")

    base_index = Path(".research/subtitle-corpus")
    for item in source_manifest("subtitle-dvd"):
        if not item["file"].endswith(".idx"):
            continue
        path = base_index/item["file"]
        companions = [{**companion, "file":(base_index/companion["file"]).as_posix()}
                      for companion in item.get("companions", [])]
        companion = next((Path(row["file"]) for row in companions if row["file"].endswith(".sub")),
                         path.with_suffix(".sub"))
        add("vobsub-index", item["id"], path, [str(path), str(companion)],
            profile="existing", source=item, companions=companions)
    index = (base_index/"dvd/basic.idx").read_bytes()
    head = index.split(b"id: en", 1)[0].replace(b"langidx: 0\n", b"")
    stamp = b"timestamp: 00:00:01:000, filepos: 000000000\n"
    block = b"id: en, index: 0\n" + stamp + b"id: de, index: 1\n" + stamp
    authored = {
        "multi":head+b"langidx: 1\n"+block,
        "default-first":head+b"langidx: 0\n"+block,
        "default-out-of-range":head+b"langidx: 9\n"+block,
        "delay-across-track":head+b"id: en, index: 0\ndelay: -00:00:00:500\n"+stamp+b"id: de, index: 1\n"+stamp,
        "missing-index":head+b"id: en\n"+stamp,
        "malformed-id":head+b"id: invalid\n"+stamp,
        "large-index":head+b"id: en, index: 256\n"+stamp,
        "crlf":index.replace(b"\n",b"\r\n"),
        "long-timestamp":head+b"id: en, index: 0\ntimestamp: 00000000000000000:00:01:000, filepos: 0\n",
    }
    payload = base_index/"dvd/basic.sub"
    for name, data in authored.items():
        path = file("vobsub-index",name+".idx",data)
        add("vobsub-index", name, path, [str(path), str(payload)], profile="authored",
            companions=[dict(file=str(payload), sha256=hashlib.sha256(payload.read_bytes()).hexdigest())])

    for item in source_manifest("subtitle-dvb"):
        path = base_index/item["file"]
        kind = "mkv" if path.suffix == ".mkv" else "ts"
        add("dvb-source", item["id"], path, [kind], profile=kind, source=item)
    sets = [(dvb.T0 + i*90000,dvb.pes_data([dvb.pcs(page=1),dvb.eds(page=1)])) for i in range(4)]
    services = [('eng',0x10,1,2),('deu',0x20,3,4)]
    variants = {
        "combined-services":dvb.single(sets,services),
        "separate-descriptors":dvb.transport(
            [(dvb.SUB_PID,6,b"".join(dvb.subtitling_descriptor([item]) for item in services))],
            [(dvb.SUB_PID,dvb.pes(data,pts)) for pts,data in sets]),
    }
    for name, data in variants.items():
        path = file("dvb-source",name+".ts", data)
        add("dvb-source",name,path,["ts"],profile="descriptors")

    for item in source_manifest("subtitle-pgs"):
        path = base_index/item["file"]
        kind = {".sup":"sup", ".mkv":"mkv", ".ts":"ts", ".m2ts":"ts"}[path.suffix]
        ordinal = next((value[7:] for value in item["arguments"] if value.startswith("stream=")),"0")
        add("pgs-segments",item["id"],path,[kind] if kind == "sup" else [kind,ordinal],profile=kind,source=item)
    for item in source_manifest("subtitle-pgs-capsules"):
        add("pgs-segments",item["id"],item["file"],["capsule"],profile="capsule",source=item)
    from harness.subtitles import pgs_streams as pgs
    segments = [
        pgs.pcs(1920,1080,5,0x80,((1,0,40,50,True,(6,7,8,9)),)),
        pgs.pds(3, 5, [(1,2,3,4,5),(1,6,7,8,9)]),
        pgs.ods_raw(1,4,0xc0,b"\x01\x02",length=6,width=2,height=1),
        pgs.wds([(1,20,30,500,100)]), pgs.segment(0x80,b""), pgs.segment(0x7f,b"ab"),
    ]
    for segment in segments:
        for kind in ("packet","sup"):
            body = segment if kind == "packet" else b"PG"+ints(0xffffffff,42)+segment
            frames = range(len(body)+1)
            for cut in frames:
                name = f"authored-{kind}-{segment[0]:02x}-{cut:02d}"
                path = file("pgs-segments",name+".bin",body[:cut])
                add("pgs-segments",name,path,[kind],profile="truncations")
            # Rewrite the wire length as a second family to reach the typed readers.
            for cut in range(len(segment)-3):
                clipped = segment[:1] + struct.pack(">H",cut) + segment[3:3+cut]
                data = clipped if kind == "packet" else body[:10]+clipped
                name = f"typed-{kind}-{segment[0]:02x}-{cut:02d}"
                path = file("pgs-segments",name+".bin",data)
                add("pgs-segments",name,path,[kind],profile="truncations")

    streams = {
        "utf-8":("UTF-8","utf-8"), "utf-16":("UTF-16","utf-16"),
        "utf-16be":("UTF-16BE","utf-16-be"), "utf-16le":("UTF-16LE","utf-16-le"),
        "ascii":("US-ASCII","ascii"), "iso-8859-1":("ISO-8859-1","latin1"),
        "cp1251":("CP1251","cp1251"), "cp1252":("CP1252","cp1252"),
        "gbk":("GBK","gbk"), "big5":("BIG5","big5"), "shift_jis":("SHIFT_JIS","shift_jis"),
        "euc-kr":("EUC-KR","euc_kr"), "cp932":("CP932","cp932"),
        "cp949":("CP949","cp949"), "cp950":("CP950","cp950"),
        "big5-receiver":("BIG5","big5"), "kr-c1":("EUC-KR","euc_kr"),
        "kr-receiver":("EUC-KR","euc_kr"), "cp932-receiver":("CP932","cp932"),
    }
    seed = file("charset-streams","matrix.bin",b"")
    randomizer = random.Random(0xa55)
    for index, (name, (label, codec)) in enumerate(streams.items()):
        add("charset-streams", name+"-bytes", seed,[name,label,"bytes"],profile=name)
        if index >= 8:
            add("charset-streams", name+"-pairs", seed,[name,label,"pairs"],profile=name)
        alphabet = [char for char in "English text, Cyrillic Русский, 한글 한국, 日本語, 繁體中文, café."
                    if char.encode(codec, errors="ignore") not in (b"",b"\xff\xfe")]
        for length in (1024, 8192, 65536):
            text = "".join(randomizer.choice(alphabet) for _ in range(length//4))
            data = text.encode(codec)
            # Repeating complete characters keeps multibyte rows valid at the nominal bound.
            if codec == "utf-16":
                data = b"\xff\xfe" + (text*4).encode("utf-16-le")[:length-2]
            elif codec.startswith("utf-16"):
                data = (text*4).encode(codec)[:length]
            else:
                data += b"a" * (length - len(data))
            path = file("charset-streams",f"{name}-{length}.bin",data)
            add("charset-streams",f"{name}-{length}",path,[name,label,"stream"],profile=name)
    for name, label, data in [
            ("utf-8","UTF-8",b"head\xf0\x90\x80"), ("utf-16","UTF-16",b"\xfe\xff\x00A\xd8\x00"),
            ("gbk","GBK",b"head\x81"), ("big5","BIG5",b"head\xa1\x00"),
            ("shift_jis","SHIFT_JIS",b"head\x81"), ("euc-kr","EUC-KR",b"head\xa1")]:
        path = file("charset-streams",f"{name}-invalid.bin",data)
        add("charset-streams",f"{name}-invalid",path,[name,label,"stream"],profile=name)

    def semantic_case(name, sample, description=None, scale="1000", profile="recorded"):
        private = ints(0xffffffff) if description is None else ints(len(description))+description
        path = file("tx3g-semantics",name+".bin",private+sample)
        add("tx3g-semantics",name,path,[scale],profile=profile)

    for directory in sorted(path for path in native.iterdir() if path.is_dir()):
        configuration = directory/"extradata.bin"
        private = configuration.read_bytes() if configuration.exists() else None
        for sample in sorted(directory.glob("packet-*.bin")):
            semantic_case(directory.name+"-"+sample.stem, sample.read_bytes(), private)

    for item in text_cases:
        if item.get("group") != "tx3g":
            continue
        _, _, blocks = capsule(Path(item["file"]).read_bytes())
        description, scale = None, "-"
        for index, (kind, data) in enumerate(blocks):
            if kind == "open":
                numerator, denominator, _, _, description = data
                scale = str(denominator // numerator) if numerator and denominator else "-"
            elif kind == "packet":
                semantic_case(f"{item['id']}-{index}", data[2], description, scale, profile="subtitle-text")

    from harness.subtitle_corpus import tx3g_style, tx3g_styl, tx3g_hlit, tx3g_hclr
    def span(kind, start, end):
        return box(kind, struct.pack(">HH",start,end))
    def href(start,end):
        url, alt = b"https://example.com", "説明".encode()
        return box(b"href",struct.pack(">HHB",start,end,len(url))+url+bytes([len(alt)])+alt)
    def karaoke(*entries, start=0):
        return box(b"krok",struct.pack(">IH",start,len(entries))+
                   b"".join(struct.pack(">IHH",end_tick,first,last) for end_tick,first,last in entries))
    consistent = tx3g_sample("A😀♫éZ",tx3g_styl(tx3g_style(1,4,face=0x81)),
                             tx3g_hlit(2,4),tx3g_hclr(10,20,30,40),
                             href(0,2),span(b"blnk",1,5),
                             box(b"tbox",struct.pack(">4h",-10,2,40,55)),
                             box(b"dlay",ints(50)),box(b"twrp",b"\x01"),
                             box(b"disp",struct.pack(">h",-31)))
    continuous = bytearray(default)
    continuous[:4] = ints(0x000a09e0)
    semantic_case("authored-overlapping-style-and-effects",consistent,default,profile="authored")
    semantic_case("authored-dynamic-continuous",
                  tx3g_sample("ABCDE",karaoke((2,1,2),(5,3,6)),span(b"blnk",2,4)),
                  continuous,profile="authored")
    semantic_case("authored-dynamic-unknown-display",
                  tx3g_sample("ABCDE",karaoke((2,1,2),(5,3,6)),tx3g_hlit(0,1)),profile="authored")
    semantic_case("authored-dynamic-invalid-intersection",
                  tx3g_sample("ABCDE",karaoke((5,2,4)),tx3g_hlit(3,4)),default,profile="authored")
    semantic_case("authored-overlapping-same-type",
                  tx3g_sample("ABCDE",tx3g_hlit(1,4),tx3g_hlit(3,5)),default,profile="authored")
    semantic_case("authored-zero-length-karaoke",
                  tx3g_sample("ABCDE",karaoke((0,0,1),(5,3,6))),default,profile="authored")
    semantic_case("authored-description-disparity",tx3g_sample("CD"),
                  default+box(b"disp",struct.pack(">h",47)),scale="-",profile="authored")

    for feature, rows in cases.items():
        operation, prefix, oracle, cause, description = FEATURES[feature]
        contract = dict(schema=1,id=feature,gate="required",operation=operation,
                        candidate="subtitle-views-worker",worker_operation=prefix)
        if oracle:
            contract["oracle"] = oracle
        else:
            contract.update(reference="ffmpeg-subtitle-views-native",runner="cargo xtask report")
        contract.update(contract=[description], required_profiles=sorted({row["profile"] for row in rows}),
                        required_targets=TARGETS, iterations=50 if cause == PENDING else 1, minimum_pairs=30, confidence=0.95,
                        parity_timeout_seconds=120)
        contract["open_unmeasured"] = [dict(bound=bound, cases=[row["id"] for row in rows], cause=cause)
                                       for bound in ("runtime","heap","total_rss")]
        directory = out/"harness/features"/feature
        directory.mkdir(parents=True,exist_ok=True)
        for name, value in [("contract",contract),("corpus",dict(schema=1,directory=".",cases=rows))]:
            (directory/f"{name}.json").write_text(json.dumps(value,indent=2)+"\n")
        print(f"{feature}: {len(rows)} cases")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description="Author the subtitle view fixtures and their case lists.")
    parser.add_argument("--output", required=True, type=Path)
    write_corpora(parser.parse_args().output)
