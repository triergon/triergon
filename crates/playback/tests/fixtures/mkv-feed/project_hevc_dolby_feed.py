#!/usr/bin/env python3
"""Project ffprobe packet bytes into the direct-feed wire contract."""
import argparse
import collections
import json
import struct
from fractions import Fraction
from pathlib import Path

START = b"\x00\x00\x00\x01"


def decode_hex_dump(dump):
    return bytes.fromhex(
        " ".join(line.partition(": ")[2].split("  ", 1)[0]
                 for line in dump.splitlines() if line)
    )


def length_prefixed(data, width, count=None):
    nals = []
    while data and (count is None or len(nals) < count):
        size = int.from_bytes(data[:width], "big")
        assert size >= 2 and width + size <= len(data)
        nals.append(data[width:width + size])
        data = data[width + size:]
    assert count is None or len(nals) == count
    return nals, data


def parameters(record):
    width = (record[21] & 3) + 1
    data = record[23:]
    sets = []
    for _ in range(record[22]):
        nal_type = data[0] & 63
        count = int.from_bytes(data[1:3], "big")
        nals, data = length_prefixed(data[3:], 2, count)
        if nal_type in (32, 33, 34):
            sets.extend(nals)
    assert not data
    return width, sets


def main():
    cli = argparse.ArgumentParser(description=__doc__)
    cli.add_argument("reference", type=Path)
    cli.add_argument("gold", type=Path)
    args = cli.parse_args()
    reference = json.loads(args.reference.read_bytes())
    streams = {stream["index"]: stream for stream in reference["streams"]}
    hevc = next(stream for stream in streams.values() if stream["codec_name"] == "hevc")
    width, sets = parameters(decode_hex_dump(hevc["extradata"]))
    counts = collections.Counter()
    random_access = 0
    with args.gold.open("wb") as output:
        for packet in reference["packets"]:
            index = packet["stream_index"]
            codec = streams[index]["codec_name"]
            payload = decode_hex_dump(packet["data"])
            assert len(payload) == int(packet["size"])
            irap = False
            if codec == "hevc":
                nals, trailing = length_prefixed(payload, width)
                assert not trailing
                irap = any(16 <= (nal[0] >> 1) & 63 <= 23 for nal in nals)
                payload = b"".join(START + nal for nal in ((sets if irap else []) + nals))
            else:
                assert codec in ("ac3", "eac3")
            pts = int(packet["pts"]) * Fraction(streams[index]["time_base"]) * 10**9
            assert pts.denominator == 1
            output.write(struct.pack(">BqBBI", index, int(pts), "K" in packet["flags"], irap, len(payload)))
            output.write(payload)
            counts[codec] += 1
            random_access += irap
    print(dict(counts), "HEVC random-access packets", random_access)


if __name__ == "__main__":
    main()
