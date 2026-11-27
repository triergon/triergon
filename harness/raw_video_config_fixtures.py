"""Derive raw configuration controls by changing only SPS syntax fields."""

import argparse
from pathlib import Path

from .raw_video_config_oracle import START, decode_sections

AVC = Path("crates/playback/tests/fixtures/raw-video/avc-short.h264")
AVC_OTHER = Path("crates/playback/tests/fixtures/raw-video/avc-80x48.h264")
HEVC = Path("crates/framing/fixtures/raw-annex-b/hevc-aud0.hevc")


def without_prevention(data):
    raw, zeros = bytearray(), 0
    for byte in data:
        if byte == 3 and zeros == 2:
            zeros = 0
            continue
        raw.append(byte)
        zeros = zeros + 1 if byte == 0 else 0
    return raw


def with_prevention(raw):
    data, zeros = bytearray(), 0
    for byte in raw:
        if byte <= 3 and zeros == 2:
            data.append(3)
            zeros = 0
        data.append(byte)
        zeros = zeros + 1 if byte == 0 else 0
    return bytes(data)


def altered_sps(source, codec, changes):
    fields = next(values for name, values in decode_sections(source, codec)
                  if name == "Sequence Parameter Set")
    header = 2 if codec == "hevc" else 1
    data = source.read_bytes()
    starts = list(START.finditer(data))
    result = bytearray(data[:starts[0].start()])
    for start, following in zip(starts, [*starts[1:], None]):
        end = len(data) if following is None else following.start()
        nal = data[start.end():end]
        result.extend(start[0])
        kind = (nal[0] >> 1) & 63 if header == 2 else nal[0] & 31
        if kind != (33 if header == 2 else 7):
            result.extend(nal)
            continue

        # Bit positions reported by the public syntax trace include the NAL
        # header and omit prevention bytes. Keep the existing RBSP stop bit.
        rbsp = without_prevention(nal[header:].rstrip(b"\x00"))
        bits = "".join(f"{byte:08b}" for byte in rbsp).rstrip("0")
        for name, width, encoded in sorted(changes, key=lambda item: fields[item[0]][0], reverse=True):
            pos, original, _ = fields[name]
            pos -= header * 8
            assert bits[pos:pos + len(original)] == original, name
            bits = bits[:pos] + encoded + bits[pos + width:]
        bits += "0" * (-len(bits) % 8)
        result.extend(nal[:header])
        result.extend(with_prevention(bytes(int(bits[i:i + 8], 2) for i in range(0, len(bits), 8))))
    return bytes(result)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("directory", type=Path)
    args = parser.parse_args()
    args.directory.mkdir(parents=True, exist_ok=True)
    cases = {
        "avc-fixed.h264": altered_sps(AVC, "h264", [
            ("fixed_frame_rate_flag", 1, "1"),
        ]),
        "avc-no-timing.h264": altered_sps(AVC, "h264", [
            ("timing_info_present_flag", 66, "0"),
        ]),
        "avc-reconfigure.h264": AVC.read_bytes() + AVC_OTHER.read_bytes(),
        "hevc-proportional.hevc": altered_sps(HEVC, "hevc", [
            ("vui_time_scale", 32, f"{24:032b}"),
            ("vui_poc_proportional_to_timing_flag", 1, "1010"),
        ]),
        "hevc-no-timing.hevc": altered_sps(HEVC, "hevc", [
            ("vui_timing_info_present_flag", 67, "0"),
        ]),
    }
    for name, data in cases.items():
        (args.directory / name).write_bytes(data)


if __name__ == "__main__":
    main()
