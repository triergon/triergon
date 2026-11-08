"""Write damaged copies of an Ogg file into the current directory.

usage: damage.py SRC

crc_bad.opus has one payload byte of the third page flipped, lost_page.opus lacks the
fourth page and truncated.opus ends in the middle of the last page.
"""
import sys


def pages(data):
    """(start, payload start, end) of every page."""
    out, at = [], 0
    while at < len(data):
        assert data[at:at + 4] == b"OggS", f"no page at {at}"
        count = data[at + 26]
        payload = at + 27 + count
        end = payload + sum(data[at + 27:payload])
        out.append((at, payload, end))
        at = end
    return out


def main(path):
    data = open(path, "rb").read()
    found = pages(data)
    _, payload, _ = found[2]
    bad = bytearray(data)
    bad[payload + 5] ^= 0x40
    open("crc_bad.opus", "wb").write(bad)
    start, _, end = found[3]
    open("lost_page.opus", "wb").write(data[:start] + data[end:])
    _, payload, end = found[-1]
    open("truncated.opus", "wb").write(data[:payload + (end - payload) // 2])


if __name__ == "__main__":
    main(sys.argv[1])
