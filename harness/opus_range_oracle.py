"""Project the normative per-packet ranges recorded alongside RFC 8251 inputs."""
import struct


def ranges(path):
    data = path.read_bytes()
    rows = []
    offset = 0
    while offset < len(data):
        length, final_range = struct.unpack_from(">II", data, offset)
        offset += 8 + length
        if offset > len(data):
            raise ValueError("truncated Opus test packet")
        rows.append(f"3 {len(rows)} {final_range}")
    return rows
