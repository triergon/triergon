"""Write an M2TS192 copy of a TS188 input for the ts-program contract.

Each 188-byte transport packet is prefixed with a four-byte TP_extra_header:
copy_permission_indicator 0 and a 30-bit arrival time stamp that advances by
2,700 ticks of the 27 MHz clock (100 microseconds) per packet from zero. The
transport packets are unchanged.

    python3 make_m2ts.py INPUT.ts OUTPUT.m2ts
"""

import sys

PACKET = 188
STEP = 2700


def convert(data: bytes) -> bytes:
    if not data or len(data) % PACKET:
        raise ValueError("input is not whole TS188 packets")
    out = bytearray()
    for index in range(len(data) // PACKET):
        packet = data[index * PACKET:(index + 1) * PACKET]
        if packet[0] != 0x47:
            raise ValueError(f"packet {index} has no sync byte")
        out += ((index * STEP) & 0x3FFFFFFF).to_bytes(4, "big")
        out += packet
    return bytes(out)


def main() -> None:
    source, target = sys.argv[1:]
    with open(source, "rb") as handle:
        data = handle.read()
    with open(target, "wb") as handle:
        handle.write(convert(data))


if __name__ == "__main__":
    main()
