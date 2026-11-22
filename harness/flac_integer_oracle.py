"""Read the encoder's native PCM checksum and dimensions from FLAC STREAMINFO."""

from hashlib import md5


def trace(path, mode=None):
    if mode not in (None, "bare"):
        raise ValueError("expected a configured or bare FLAC decoder")
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
    if mode == "bare":
        # The authored frame changes to mono at 32 kHz, with 16 constant samples.
        pcm = (-32767).to_bytes(2, "little", signed=True) * 16
        rows.append(f"3 32000 1 16 16 {md5(pcm).hexdigest()}")
    return rows
