"""Independent zlib streams for the inflate checks, written by Python's zlib.

Inputs and bit flips come from seeded `random.Random` generators. Every expected
output is Python's own zlib result, never candidate output.
"""
import random
import zlib

LEVELS = [(0, zlib.Z_DEFAULT_STRATEGY, 'default'), (6, zlib.Z_FIXED, 'fixed'),
          (6, zlib.Z_DEFAULT_STRATEGY, 'default'), (9, zlib.Z_HUFFMAN_ONLY, 'huffman-only')]


def inflate_cases():
    """Every input at three window sizes and four level/strategy pairs, each
    stream with a non-final sync-flushed block that keeps cross-block history;
    one stream to truncate and corrupt; and 160 single-bit flips of another, each
    with Python's decoded output, or null where Python's zlib rejects it."""
    rng = random.Random(1951)
    inputs = [b'', b'a', bytes(range(256)) * 256, b'abcde' * 6000, bytes(rng.randrange(256) for _ in range(20000))]
    inputs += [bytes(rng.choices(range(256), weights=[1 / (1 + i) for i in range(256)], k=n)) for n in [8, 129, 4096, 32769]]
    streams = []
    for window in [9, 12, 15]:
        for level, strategy, name in LEVELS:
            for index, original in enumerate(inputs):
                encoder = zlib.compressobj(level, zlib.DEFLATED, window, 8, strategy)
                half = len(original) // 2
                encoded = (encoder.compress(original[:half]) + encoder.flush(zlib.Z_SYNC_FLUSH)
                           + encoder.compress(original[half:]) + encoder.flush())
                streams.append({'window': window, 'level': level, 'strategy': name, 'input': index,
                                'encoded': encoded.hex()})
    rng = random.Random(1950); valid = zlib.compress(bytes(range(256)) * 4)
    mutations = []
    for _ in range(160):
        data = bytearray(valid); at = rng.randrange(len(data)); data[at] ^= 1 << rng.randrange(8)
        try:
            decoded = zlib.decompress(data).hex()
        except zlib.error:
            decoded = None
        mutations.append({'data': data.hex(), 'decoded': decoded})
    return {'inputs': [original.hex() for original in inputs], 'streams': streams,
            'truncation': zlib.compress(b'abcde' * 3000).hex(), 'mutations': mutations}
