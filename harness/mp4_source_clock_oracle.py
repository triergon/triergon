"""ISO/IEC 14496-12 sample clocks, before the movie presentation edit list.

Decode time accumulates the sample deltas from stts, or starts at a fragment's
tfdt and accumulates trun durations. Composition time adds the ctts or trun
offset, unsigned in version zero and signed in version one. The edit list maps
media composition time to movie time, leaving all these media-time rows intact.
"""

from dataclasses import dataclass
from itertools import accumulate
from pathlib import Path


class InvalidMovie(ValueError):
    """A malformed box, timing run or sample count."""


MAX_SAMPLES = 1_000_000


def integer(data: memoryview, offset: int, width: int = 4, signed: bool = False) -> int:
    word = data[offset : offset + width]
    if len(word) != width:
        raise InvalidMovie("short integer")
    return int.from_bytes(word, "big", signed=signed)


@dataclass(frozen=True)
class Box:
    kind: bytes
    payload: memoryview

    @property
    def version(self) -> int:
        return integer(self.payload, 0, 1)

    @property
    def flags(self) -> int:
        return integer(self.payload, 1, 3)

    def children(self) -> list["Box"]:
        return list(boxes(self.payload))


def boxes(data: memoryview):
    at = 0
    while at < len(data):
        size = integer(data, at)
        kind = bytes(data[at + 4 : at + 8])
        header = 8
        if size == 1:
            size = integer(data, at + 8, 8)
            header += 8
        elif size == 0:
            size = len(data) - at
        if kind == b"uuid":
            header += 16
        if size < header or at + size > len(data):
            raise InvalidMovie(f"invalid box extent for {kind!r}")
        yield Box(kind, data[at + header : at + size])
        at += size


def find(items: list[Box], kind: bytes, required: bool = True) -> Box | None:
    found = [item for item in items if item.kind == kind]
    if len(found) > 1 or required and not found:
        raise InvalidMovie(f"expected one {kind!r} box")
    return found[0] if found else None


def version(box: Box) -> int:
    value = box.version
    if value not in (0, 1):
        raise InvalidMovie(f"unsupported {box.kind!r} version")
    return value


def check_edits(track: list[Box]) -> None:
    edts = find(track, b"edts", required=False)
    if edts is None:
        return
    elst = find(edts.children(), b"elst", required=False)
    if elst is None:
        return
    width = 8 if version(elst) else 4
    entries = integer(elst.payload, 4)
    stride = width * 2 + 4
    if len(elst.payload) != 8 + entries * stride:
        raise InvalidMovie("edit list has a short or surplus entry")
    for offset in range(8, len(elst.payload), stride):
        media_time = integer(elst.payload, offset + width, width, signed=True)
        if media_time < -1:
            raise InvalidMovie("edit media time is less than the empty-edit marker")


def run_values(box: Box, count: int, bounded: bool = False) -> list[int]:
    signed = box.kind == b"ctts" and version(box) == 1
    entries = integer(box.payload, 4)
    if len(box.payload) != 8 + entries * 8:
        raise InvalidMovie("timing table size does not match its entry count")
    result = []
    for index in range(entries):
        at = 8 + 8 * index
        repeat = integer(box.payload, at)
        value = integer(box.payload, at + 4, signed=signed)
        if repeat == 0:
            raise InvalidMovie("zero-length timing run")
        remaining = count - len(result)
        if repeat > remaining and not (
            bounded and count > 0 and (box.kind == b"ctts" or index + 1 == entries)
        ):
            raise InvalidMovie("timing run extends past the sample count")
        result.extend([value] * min(remaining, repeat))
    if len(result) != count:
        raise InvalidMovie("timing runs do not cover the declared samples")
    return result


@dataclass
class TrackClock:
    scale: int
    end: int
    rows: list[tuple[int, int, int]]

    def append(self, start: int, durations: list[int], offsets: list[int]) -> None:
        if len(self.rows) + len(durations) > MAX_SAMPLES:
            raise InvalidMovie("sample count exceeds the oracle bound")
        for dts, delta, offset in zip(accumulate(durations, initial=start), durations, offsets):
            self.rows.append((dts + offset, dts, delta))
        self.end = start + sum(durations)


def movie_tracks(movie: Box, bounded: bool) -> tuple[dict[int, TrackClock], dict[int, int]]:
    children = movie.children()
    tracks = {}
    defaults = {}
    for trak in (child for child in children if child.kind == b"trak"):
        track = trak.children()
        tkhd = find(track, b"tkhd")
        track_id = integer(tkhd.payload, 20 if version(tkhd) else 12)
        media = find(track, b"mdia").children()
        mdhd = find(media, b"mdhd")
        scale = integer(mdhd.payload, 20 if version(mdhd) else 12)
        if not track_id or track_id in tracks or not scale:
            raise InvalidMovie("duplicate track, zero track ID or zero timescale")
        check_edits(track)
        table = find(find(media, b"minf").children(), b"stbl").children()
        sizes = find(table, b"stsz", required=False) or find(table, b"stz2")
        count = integer(sizes.payload, 8)
        if count > MAX_SAMPLES:
            raise InvalidMovie("sample count exceeds the oracle bound")
        delta = run_values(find(table, b"stts"), count, bounded)
        composition = find(table, b"ctts", required=False)
        offsets = run_values(composition, count, bounded) if composition else [0] * count
        tracks[track_id] = TrackClock(scale, 0, [])
        tracks[track_id].append(0, delta, offsets)
    mvex = find(children, b"mvex", required=False)
    if mvex:
        for trex in (child for child in mvex.children() if child.kind == b"trex"):
            defaults[integer(trex.payload, 4)] = integer(trex.payload, 12)
    return tracks, defaults


def fragment_run(run: Box, default_duration: int | None) -> tuple[list[int], list[int]]:
    flags = run.flags
    count = integer(run.payload, 4)
    if count > MAX_SAMPLES:
        raise InvalidMovie("fragment sample count exceeds the oracle bound")
    signed = version(run) == 1
    at = 8 + 4 * bool(flags & 0x001) + 4 * bool(flags & 0x004)
    duration = []
    offset = []
    for _ in range(count):
        if flags & 0x100:
            delta = integer(run.payload, at)
            at += 4
        elif default_duration is None:
            raise InvalidMovie("sample duration has no fragment default")
        else:
            delta = default_duration
        at += 4 * bool(flags & 0x200) + 4 * bool(flags & 0x400)
        composition = integer(run.payload, at, signed=signed) if flags & 0x800 else 0
        at += 4 * bool(flags & 0x800)
        duration.append(delta)
        offset.append(composition)
    if at != len(run.payload):
        raise InvalidMovie("fragment sample rows do not fill the run")
    return duration, offset


def add_fragment(moof: Box, tracks: dict[int, TrackClock], defaults: dict[int, int]) -> None:
    for traf in (child for child in moof.children() if child.kind == b"traf"):
        children = traf.children()
        tfhd = find(children, b"tfhd")
        track_id = integer(tfhd.payload, 4)
        if track_id not in tracks:
            raise InvalidMovie("fragment track is missing its initialization")
        track = tracks[track_id]
        at = 8 + 8 * bool(tfhd.flags & 0x001) + 4 * bool(tfhd.flags & 0x002)
        default = integer(tfhd.payload, at) if tfhd.flags & 0x008 else defaults.get(track_id)
        tfdt = find(children, b"tfdt", required=False)
        start = integer(tfdt.payload, 4, 8 if version(tfdt) else 4) if tfdt else track.end
        for trun in (child for child in children if child.kind == b"trun"):
            duration, offsets = fragment_run(trun, default)
            track.append(start, duration, offsets)
            start = track.end


def trace(path: Path, *arguments: str) -> list[str]:
    """Canonical source-clock rows by track ID, preserving per-track decode order."""
    if len(arguments) != 2 or arguments[1] != "clock" or arguments[0] not in (
        "mp4", "mp4-progressive", "mp4-timing-policy",
    ):
        raise ValueError("expected an MP4 opener followed by clock")
    try:
        roots = list(boxes(memoryview(path.read_bytes())))
        tracks, defaults = movie_tracks(find(roots, b"moov"), arguments[0] == "mp4-timing-policy")
        for moof in (root for root in roots if root.kind == b"moof"):
            add_fragment(moof, tracks, defaults)
    except InvalidMovie:
        return ["8"]
    return [
        f"3 {track_id} 1 {track.scale} {pts} {dts} {duration}"
        for track_id, track in sorted(tracks.items())
        for pts, dts, duration in track.rows
    ] + ["7"]
