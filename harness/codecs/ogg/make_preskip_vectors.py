"""Crop two pinned Ogg Opus streams without re-encoding their 2.5 ms packets.

Usage: make_preskip_vectors.py SOURCE_DIR OUTPUT_DIR
"""

import argparse
from pathlib import Path


def checksum(page):
    """Compute the Ogg page CRC with its checksum field cleared."""
    crc = 0
    for byte in page:
        crc ^= byte << 24
        for _ in range(8):
            polynomial = 0x04C11DB7 if crc & 0x80000000 else 0
            crc = ((crc << 1) ^ polynomial) & 0xFFFFFFFF
    return crc


def preskip(data, samples):
    """Change the pre-skip in the single OpusHead packet on the first page."""
    if len(data) < 47 or data[:6] != b"OggS\x00\x02" or data[26:28] != b"\x01\x13":
        raise ValueError("expected a beginning-of-stream page with one 19-byte packet")
    if data[28:36] != b"OpusHead":
        raise ValueError("expected an Opus identification header")
    page = bytearray(data[:47])
    page[38:40] = samples.to_bytes(2, "little")
    page[22:26] = bytes(4)
    page[22:26] = checksum(page).to_bytes(4, "little")
    return page + data[47:]


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("source", type=Path)
    parser.add_argument("output", type=Path)
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    # End at a packet boundary in mono and 17 samples into the next packet in stereo.
    for name, samples in (("celt_mo_2.5.opus", 3840), ("celt_st_2.5.opus", 3857)):
        data = (args.source / name).read_bytes()
        (args.output / f"preskip-{samples}-{name}").write_bytes(preskip(data, samples))


if __name__ == "__main__":
    main()
