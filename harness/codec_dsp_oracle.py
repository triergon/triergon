"""Independent arithmetic and power-complementary window expectations."""
from functools import lru_cache
import math
import struct


def pairs(path):
    signed_min = -(1 << 31)
    signed_max = (1 << 31) - 1
    for line in path.read_text(encoding="ascii").splitlines():
        fields = line.split()
        if fields[0] != "range":
            yield tuple(map(int, fields))
            continue
        first, last = map(int, fields[1:])
        for d in range(first, last + 1):
            end = signed_max // d
            yield from ((a, n, d) for a, n in zip(
                (signed_min, 1 - d, -d, d, signed_max, signed_min + 1, signed_max),
                (0, d - 1, d, d + 1, end - 1, end, (1 << 32) - 1),
            ))

def divide(path):
    rows = []
    for signed, unsigned, divisor in pairs(path):
        magnitude = abs(signed) // divisor
        truncated = (-magnitude if signed < 0 else magnitude) % (1 << 32)
        quotient, remainder = divmod(unsigned, divisor)
        suffix = f" {divisor} {quotient} {remainder}" if unsigned * divisor < 1 << 31 else ""
        rows.append(f"3 {truncated} {quotient}{suffix}")
    return rows


@lru_cache(maxsize=20)
def kaiser_coefficients(alpha):
    squared = (math.pi * alpha / 4) ** 2
    # I0(πα·sqrt(t)/2) = Σ (πα/4)^(2k) t^k / (k!)².
    # Evaluate the fixed polynomial with Horner's method.
    terms = [squared ** k / math.factorial(k) ** 2 for k in range(64)]
    # The discarded absolute tail is below float64 rounding even at t = 1.
    cutoff = max(k for k, term in enumerate(terms) if term >= 2.0**-60)
    return tuple(reversed(terms[:cutoff + 1]))


def kbd(length, twice_alpha):
    coefficients = kaiser_coefficients(float(twice_alpha))
    half = []
    for i in range(length // 2 + 1):
        t = 4.0 * i * (length - i) / length**2
        value = 0.0
        for coefficient in coefficients:
            value = value * t + coefficient
        half.append(value)
    kernel = half + half[-2 if length % 2 == 0 else -1::-1]
    total = math.fsum(kernel)
    cumulative = 0.0
    for value in kernel[:-1]:
        cumulative += value
        yield math.sqrt(cumulative / total)


def vorbis(length):
    for i in range(length):
        angle = math.pi * (2 * i + 1) / (4 * length)
        yield math.sin(math.pi / 2 * math.sin(angle)**2)


def windows(path):
    rows = []
    for line in path.read_text(encoding="ascii").splitlines():
        length, alpha = map(int, line.split())
        values = kbd(length, alpha) if alpha else vorbis(length)
        packed = b"".join(struct.pack(">f", sample) for sample in values)
        rows.append(f"3 {length} {alpha} {packed.hex()}")
    return rows


def cosine(case):
    """Direct float64 sums for both transform definitions; no FFT."""
    n, scale = struct.unpack_from("<Id", case)
    amplitudes = struct.unpack_from(f"<{n // 2}f", case, 12)
    terms = [(i + .5, value) for i, value in enumerate(amplitudes) if value]

    def basis(position):
        step = math.tau / n * position
        return scale * math.fsum(value * math.cos(step * coefficient)
                                 for coefficient, value in terms)

    # The first and last quarter are determined by the middle two quarters.
    center = [basis(i + .5 + n / 2) for i in range(n // 2)]
    first, second = center[:n // 4], center[n // 4:]
    imdct = [-v for v in reversed(first)] + center + second[::-1]
    dct4 = [basis(i + .5) for i in range(n // 2)]
    return imdct + dct4


if __name__ == "__main__":
    from pathlib import Path
    import sys

    for name in sys.argv[1:]:
        path = Path(name)
        values = cosine(path.read_bytes())
        path.with_suffix(".f64").write_bytes(struct.pack(f"<{len(values)}d", *values))
