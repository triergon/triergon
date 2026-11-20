"""MPEG-2 PSI and EBML wire primitives for the subtitle fixture authors."""


def ebml_id(value):
    return value.to_bytes((value.bit_length() + 7) // 8, "big")


def ebml_size(n):
    for length in range(1, 9):
        if n < (1 << (7 * length)) - 1:
            return ((1 << (7 * length)) | n).to_bytes(length, "big")
    raise ValueError(n)


def _crc32_mpeg_entry(byte):
    crc = byte << 24
    for _ in range(8):
        crc = ((crc << 1) ^ (0x04C11DB7 if crc & 0x80000000 else 0)) & 0xFFFFFFFF
    return crc


_CRC32_MPEG = tuple(_crc32_mpeg_entry(byte) for byte in range(256))


def crc32_mpeg(data):
    """CRC-32/MPEG-2 over a PSI section, most significant bit first."""
    crc = 0xFFFFFFFF
    for byte in data:
        crc ^= byte << 24
        crc = ((crc << 8) & 0xFFFFFFFF) ^ _CRC32_MPEG[(crc >> 24) & 0xFF]
    return crc
