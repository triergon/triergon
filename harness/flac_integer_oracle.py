"""Read the encoder's native PCM checksum and dimensions from FLAC STREAMINFO."""

from hashlib import md5


def trace(path, mode=None):
    if mode not in (None, "bare", "crc"):
        raise ValueError("expected a configured, bare or CRC FLAC comparison")
    with path.open("rb") as source:
        metadata = source.read(42)
    if len(metadata) != 42 or metadata[:4] != b"fLaC":
        raise ValueError("expected a native FLAC header")
    if metadata[4] & 0x7F or int.from_bytes(metadata[5:8], "big") != 34:
        raise ValueError("expected STREAMINFO as the first metadata block")
    fields = int.from_bytes(metadata[18:26], "big")
    rate = fields >> 44
    channels = (fields >> 41 & 7) + 1
    depth = (fields >> 36 & 31) + 1
    samples = fields & ((1 << 36) - 1)
    digest = metadata[26:42]
    if not rate or not samples or not any(digest):
        raise ValueError("STREAMINFO must record PCM dimensions and checksum")
    rows = [f"3 {rate} {channels} {depth} {samples} {digest.hex()}"]
    # The authored frame is mono at 32 kHz, with 16 constant samples.
    pcm = (-32767).to_bytes(2, "little", signed=True) * 16
    if mode == "bare":
        rows.append(f"3 32000 1 16 16 {md5(pcm).hexdigest()}")
    if mode == "crc":
        # RFC 9639 section 9.3 defines a CRC over the header and subframes.
        # A single flipped footer bit changes the check without changing the samples.
        for policy in range(3):
            for frame in range(3):
                refused = int(policy == 2 and frame == 1)
                samples = 0 if refused else 16
                digest = md5(b"" if refused else pcm).hexdigest()
                rows.append(f"4 {policy} {frame} {refused} {samples} {digest}")
    return rows
