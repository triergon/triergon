"""Deterministic random teletext streams and their reference comparison.

Each kind builds one stream and decoder arguments per seed; `run` writes the
streams under `.research/subtitle-corpus/teletext-fuzz/`, decodes each in the
text and ASS formats with the reference and the candidate worker (`sub-trace`)
and keeps the streams whose outputs differ.

    python3 harness/subtitles/teletext_streams.py --fuzz KIND START COUNT

Kinds: normal, subpages, edge (Level 1 pages, X/26 characters), level25
(X/26 attributes, X/28 and M/29 format 1 packets, flags and restarts) and
objects (X/26 object definitions and invocations).
"""
import os
import random
import subprocess

from teletext_streams import (CLOCK, Stream, enhancement_packet, header_packet, mrag, row_packet,
                              triplet, x28_format1)

PAGES = [(8, 0x88), (8, 0x89), (1, 0x00), (1, 0x01), (8, 0x01)]
TEXT = b"abcdefghij ABC xyz  .,"


def options(rng, duration=True):
    opts = []
    if rng.random() < 0.2:
        opts.append("opt:txt_chop_top=0")
    if rng.random() < 0.2:
        opts.append("opt:txt_chop_spaces=0")
    if rng.random() < 0.15:
        opts.append("opt:txt_page=" + rng.choice(["888", "subtitle", "100,889", "*"]))
    if rng.random() < 0.15:
        opts.append("opt:txt_default_region=%d" % rng.randint(0, 87))
    if duration and rng.random() < 0.1:
        opts.append("opt:txt_duration=%d" % rng.randint(0, 5000))
    return opts


def stream(pes):
    s = Stream()
    for i, packets in enumerate(pes):
        s.send(CLOCK * (1 + i), packets)
    return s.bytes()


# Level 1 pages with X/26 characters: the normal, subpages and edge kinds.

def level1_row(rng, boxed, edge):
    n = rng.randint(0, 40)
    out = []
    if boxed and rng.random() < 0.8:
        out = [0x20] * rng.randint(0, 12)
        if rng.random() < 0.3:
            out.append(rng.choice([0x0D, 0x0F, 0x0E]))
        if rng.random() < 0.4:
            out.append(rng.randint(1, 7))
        if rng.random() < 0.2:
            out.append(0x1D)
        out += [0x0B, 0x0B]
    while len(out) < n:
        r = rng.random()
        if r < 0.65:
            out.append(rng.randint(0x20, 0x7F) if rng.random() < 0.3 else rng.choice(TEXT))
        elif r < 0.75:
            out.append(rng.randint(0x01, 0x07))
        elif r < 0.8:
            out.append(rng.choice([0x0A, 0x0B]))
        elif r < 0.85:
            out.append(rng.randint(0x10, 0x17))
        else:
            out.append(rng.randint(0x00, 0x1F))
    out = out[:36]
    if not edge:
        out = out + [0x20] * (36 - len(out)) + [0x0C, 0x07, 0x1C, 0x20]
    return bytes(out[:40])


def level1_x26(rng):
    trips = []
    for _ in range(rng.randint(1, 12)):
        if rng.random() < 0.3:
            trips.append(triplet(rng.choice(range(40, 64)), 0x04, rng.randint(0, 39)))
        else:
            trips.append(triplet(rng.randint(0, 39), rng.choice([0x0F] + list(range(0x10, 0x20))),
                                 rng.choice(b"aeiouAEOUcnszCZ")))
    return enhancement_packet(8 if rng.random() < 0.7 else 1, 0, trips)


def level1_case(seed, subpages=False, edge=False):
    rng = random.Random(seed)
    pes = []
    serial = rng.random() < 0.2
    for _ in range(rng.randint(3, 15)):
        packets = []
        for _ in range(rng.randint(1, 3)):
            magazine, page = rng.choice(PAGES)
            flags = dict(erase=rng.random() < 0.8, subtitle=rng.random() < 0.6, suppress=rng.random() < 0.6,
                         newsflash=rng.random() < 0.1, inhibit=rng.random() < 0.05,
                         national=rng.choice([0, 0, 0, 1, 2, 4, rng.randint(0, 7)]), serial=serial,
                         subcode=rng.choice([0, 0, 1, 0x12]) if subpages else 0)
            packets.append(header_packet(magazine, page, "HDR %d" % rng.randint(0, 9), **flags))
            boxed = flags["subtitle"] or flags["newsflash"]
            for _ in range(rng.randint(0, 5)):
                packets.append(row_packet(magazine, rng.randint(1, 24), level1_row(rng, boxed, edge)))
            if rng.random() < 0.2:
                packets.append(level1_x26(rng))
            if rng.random() < 0.5:
                packets.append(header_packet(magazine, 0xFF, ""))
        pes.append(packets)
    pes.append([header_packet(8, 0xFF, ""), header_packet(1, 0xFF, "")])
    return pes, options(rng)


# Level 2.5: the level25 and objects kinds.

def level25_row(rng, boxed):
    n = rng.randint(0, 40)
    out = []
    if boxed and rng.random() < 0.7:
        out = [0x20] * rng.randint(0, 12)
        if rng.random() < 0.3:
            out.append(rng.choice([0x0D, 0x0F, 0x0E]))
        if rng.random() < 0.4:
            out.append(rng.randint(0, 7))
        if rng.random() < 0.2:
            out.append(0x1D)
        out += [0x0B, 0x0B]
    while len(out) < n:
        r = rng.random()
        if r < 0.6:
            out.append(rng.randint(0x20, 0x7F) if rng.random() < 0.3 else rng.choice(TEXT))
        elif r < 0.7:
            out.append(rng.randint(0x00, 0x07))
        elif r < 0.75:
            out.append(rng.choice([0x0A, 0x0B]))
        elif r < 0.8:
            out.append(rng.randint(0x10, 0x17))
        elif r < 0.85:
            out.append(rng.choice([0x1C, 0x1D]))
        else:
            out.append(rng.randint(0x00, 0x1F))
    out = out[:36]
    if rng.random() < 0.7:
        out = out + [0x20] * (36 - len(out)) + [0x0C, 0x07, 0x1C, 0x20]
    return bytes(out[:40])


COLUMN_MODES = [0x00, 0x00, 0x03, 0x03, 0x0C, 0x0C, 0x01, 0x02, 0x0B, 0x09, 0x0F, 0x07, 0x0E, 0x0D, 0x08,
                0x05] + list(range(0x10, 0x20))
OBJECT_MODES = range(0x10, 0x18)


def column_triplet(rng):
    mode = rng.choice(COLUMN_MODES)
    if mode in (0x00, 0x03):
        data = rng.randint(0, 31) if rng.random() < 0.9 else rng.randint(0, 127)
    elif mode == 0x0C:
        data = rng.randint(0, 127) if rng.random() < 0.3 else rng.choice(
            [0x00, 0x02, 0x10, 0x12, 0x01, 0x40, 0x41, 0x04, 0x08, 0x20, 0x16, 0x26])
    else:
        data = rng.randint(0x20, 0x7F) if rng.random() < 0.9 else rng.randint(0, 0x7F)
    return triplet(rng.randint(0, 39), mode, data)


def row_triplet(rng, objects):
    address = rng.randint(40, 63)
    modes = [0x04, 0x04, 0x04, 0x01, 0x07, 0x00, 0x1F if rng.random() < 0.2 else 0x04]
    modes.append(rng.choice([m for m in range(32) if objects or m not in OBJECT_MODES]))
    mode = rng.choice(modes)
    if mode == 0x07 and rng.random() < 0.7:
        address = 63
    return triplet(address, mode, rng.randint(0, 39) if rng.random() < 0.8 else rng.randint(0, 127))


def object_triplets(rng):
    """An object definition or invocation and the triplets around it."""
    kind = rng.choice([1, 2, 3])                # active, adaptive, passive
    out = []
    if rng.random() < 0.6:
        # Invocation of a local object: address bits select its packet and
        # triplet, the data its kind, mode 0x11-0x13.
        designation, number = rng.randint(0, 3), rng.randint(0, 12)
        address = 40 + (number & 0xF) + (designation & 1) * 16 if rng.random() < 0.85 else rng.randint(40, 63)
        out.append(triplet(address, 0x10 + kind, (designation >> 1) | rng.choice([0, 0, 0x08, 0x10, 0x18])
                           | (rng.randint(0, 0x7F) & 0x60 if rng.random() < 0.2 else 0)))
    if rng.random() < 0.7:
        out.append(triplet(rng.randint(40, 63), 0x14 + kind, rng.randint(0, 0x7F) if rng.random() < 0.3
                           else rng.choice([0x00, 0x08, 0x10, 0x18])))
    if rng.random() < 0.3:
        out.append(triplet(rng.randint(40, 63), 0x10, rng.randint(0, 0x7F)))        # origin modifier
    return out


def level25_x26(rng, magazine, objects):
    out = []
    designations = [0] if rng.random() < 0.6 else sorted(rng.sample(range(4), rng.randint(1, 3)))
    if rng.random() < 0.1:
        designations = [rng.randint(0, 15) for _ in range(2)]
    for designation in designations:
        trips = []
        for _ in range(rng.randint(1, 13)):
            r = rng.random()
            if objects and r < 0.15:
                trips += object_triplets(rng)
            elif r < 0.25:
                trips.append(row_triplet(rng, objects))
            else:
                trips.append(column_triplet(rng))
        out.append(enhancement_packet(magazine, designation, trips[:13]))
    return out


def random_clut(rng):
    return None if rng.random() < 0.3 else [rng.randint(0, 0xFFF) for _ in range(16)]


def format1(rng, magazine, designation, m29):
    packet = bytearray(x28_format1(
        magazine, designation,
        page_function=0 if rng.random() < 0.85 else rng.randint(1, 15), coding=rng.choice([0, 0, 1, 3]),
        default_g0g2=rng.choice([0, 0x08, 0x10, 0x18, 0x20, 0x28, 0x30, 0x40, 0x48, 0x50, rng.randint(0, 127)]),
        second_g0=rng.choice([0, 0x08, 0x20, rng.randint(0, 127)]), clut=random_clut(rng),
        default_screen=rng.randint(0, 31), default_row=rng.randint(0, 31), black_bg_sub=rng.random() < 0.3,
        colour_table=rng.randint(0, 7) if rng.random() < 0.7 else 0))
    if rng.random() < 0.08:
        packet[2 + 3 * rng.randint(0, 12)] ^= 0x03
    if m29:
        packet[:2] = mrag(magazine, 29)
    return bytes(packet)


def level25_case(seed, objects=False):
    rng = random.Random(seed)
    pes = []
    serial = rng.random() < 0.15
    for _ in range(rng.randint(3, 14)):
        packets = []
        for _ in range(rng.randint(1, 3)):
            magazine, page = rng.choice(PAGES)
            if rng.random() < 0.2:
                packets.append(format1(rng, magazine, rng.choice([0, 0, 4, rng.randint(0, 7)]), True))
            subtitle, newsflash = rng.random() < 0.6, rng.random() < 0.2
            flags = dict(erase=rng.random() < 0.6, subtitle=subtitle, suppress=rng.random() < 0.6,
                         newsflash=newsflash, inhibit=rng.random() < 0.03,
                         national=rng.choice([0, 0, 0, 1, 2, 4, rng.randint(0, 7)]), serial=serial,
                         subcode=rng.choice([0, 0, 0, 1, 0x3F7F]))
            for _ in range(2 if rng.random() < 0.2 else 1):
                packets.append(header_packet(magazine, page, "HDR %d" % rng.randint(0, 9), **flags))
                body = [row_packet(magazine, rng.randint(1, 24), level25_row(rng, subtitle or newsflash))
                        for _ in range(rng.randint(0, 4))]
                if rng.random() < (0.9 if objects else 0.6):
                    body += level25_x26(rng, magazine, objects)
                for designation in ([0] if rng.random() < 0.4 else []) + ([4] if rng.random() < 0.2 else []):
                    body.append(format1(rng, magazine, designation, False))
                if rng.random() < 0.1:
                    body.append(format1(rng, magazine, rng.randint(0, 3), True))
                rng.shuffle(body)
                packets += body
                if rng.random() < 0.3:
                    flags["erase"] = not flags["erase"]
            if rng.random() < 0.5:
                packets.append(header_packet(magazine, 0xFF, ""))
        pes.append(packets)
    pes.append([header_packet(8, 0xFF, ""), header_packet(1, 0xFF, "")])
    return pes, options(rng, duration=False)


KINDS = {
    "normal": lambda seed: level1_case(seed),
    "subpages": lambda seed: level1_case(seed, subpages=True),
    "edge": lambda seed: level1_case(seed, edge=True),
    "level25": lambda seed: level25_case(seed),
    "objects": lambda seed: level25_case(seed, objects=True),
}


def build(kind, seed):
    """The stream and the decoder arguments of one seed."""
    pes, opts = KINDS[kind](seed)
    return stream(pes), opts


def trace(worker, path, arguments):
    out = subprocess.run([worker, "sub-trace", path, "teletext"] + arguments, capture_output=True, text=True)
    return out.stdout


def run(kind, start, count, reference, candidate, directory):
    """Compares `count` seeds from `start`; returns the failing (seed, format)
    pairs and keeps only their streams."""
    os.makedirs(directory, exist_ok=True)
    failures = []
    for seed in range(start, start + count):
        data, opts = build(kind, seed)
        path = os.path.join(directory, "%s-%d.ts" % (kind, seed))
        with open(path, "wb") as f:
            f.write(data)
        failed = False
        for fmt in ("text", "ass"):
            arguments = ["opt:txt_format=" + fmt] + opts
            if trace(reference, path, arguments) != trace(candidate, path, arguments):
                failures.append((seed, fmt))
                failed = True
        if not failed:
            os.remove(path)
    return failures
