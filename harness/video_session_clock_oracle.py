"""Use native decoder picture order and the public session lifecycle."""

import json
import subprocess

FFPROBE = ".research/reference-decode/install/bin/ffprobe"
START = -4_000_000
PERIOD = 3_000_000
RESTART = 100_000_000


def positions(path):
    result = subprocess.run(
        [FFPROBE, "-v", "error", "-select_streams", "v:0", "-show_frames",
         "-show_entries", "frame=pkt_pos", "-of", "json", str(path)],
        text=True, stdout=subprocess.PIPE, check=True,
    )
    return [int(frame["pkt_pos"]) for frame in json.loads(result.stdout)["frames"]]


def trace(path, codec, width, height, observed_positions):
    if codec not in ("avc", "hevc"):
        raise ValueError("the video-lane control needs AVC or HEVC")
    decode = positions(path)
    if not decode or decode != list(map(int, observed_positions.split(","))):
        raise ValueError("picture mapping disagrees with the native output order")
    result = [
        f"1 {index + 1} 1 {(START + index * PERIOD) % (1 << 64)} {width} {height}"
        for index in range(len(decode))
    ]
    return result + [f"2 {len(decode) + 1} 2 {START + RESTART}", "3 3", "4 1", "5 1"]
