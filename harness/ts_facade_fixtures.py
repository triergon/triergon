"""Author selected-program TS boundaries used by the facade wire oracle."""

import argparse
from pathlib import Path
import struct

from .subtitles.dvb_streams import pes
from .subtitles.teletext_streams import Mux, crc32_mpeg, section

PID = 0x100
PMT = 0x1000


def adts(payload):
    size = 7 + len(payload)
    return bytes((0xff, 0xf1, 0x4c, 0x80 | (size >> 11),
                  (size >> 3) & 0xff, ((size & 7) << 5) | 0x1f, 0xfc)) + payload


def packet(stream_id, payload, time):
    original = pes(payload, time)
    return original[:3] + bytes([stream_id]) + original[4:]


def interrupted(clock):
    mux = Mux([(PID, 0x0f, b"")], pcr_pid=PID)
    mux.pes(PID, packet(0xc0, adts(b"old")[:4], 90_000))
    if not clock:
        mux.cc[PID] = 3
    offset = len(mux.out)
    mux.pes(PID, packet(0xc0, adts(b"whole"), 180_000))
    if clock:
        # PCR and discontinuity in the same physical carrier as the fresh PUSI.
        mux.out[offset + 5:offset + 12] = b"\x90\x00\x00\x00\x00\x7e\x00"
    return mux.bytes()


def rebind():
    mux = Mux([(PID, 0x0f, b"")], pcr_pid=PID)
    original = packet(0xc0, adts(bytes(range(255)) * 2), 90_000)
    mux.packet(PID, original[:184], True)
    body = struct.pack(">HHBHH", 0xe000 | (PID + 1), 0xf000, 0x0f, 0xe000 | PID, 0xf000)
    updated = bytearray(section(2, 1, body))
    updated[5] = 0xc3
    mux.psi(PMT, updated[:-4] + struct.pack(">I", crc32_mpeg(updated[:-4])))
    for at in range(184, len(original), 184):
        mux.packet(PID, original[at:at + 184], False)
    mux.pes(PID, packet(0xc0, adts(b"after"), 180_000))
    return mux.bytes()


def single(stream_type, stream_id, payload):
    mux = Mux([(PID, stream_type, b"")], pcr_pid=PID)
    mux.pes(PID, packet(stream_id, payload, 90_000))
    return mux.bytes()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("directory", type=Path)
    args = parser.parse_args()
    args.directory.mkdir(parents=True, exist_ok=True)
    cases = {
        "loss-interrupted-adts": interrupted(False),
        "clock-interrupted-adts": interrupted(True),
        "pcr-rebind-crossing-pes": rebind(),
        "hevc-headers-only": single(0x24, 0xe0, b"\x00\x00\x00\x01\x46\x01\x50"),
        "opaque-transport-stream-type": single(0x8e, 0xbd, b"opaque carriage"),
    }
    for name, data in cases.items():
        (args.directory / f"{name}.ts").write_bytes(data)


if __name__ == "__main__":
    main()
