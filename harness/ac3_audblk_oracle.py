"""A/52:2018 audio-block trace for high-exponent, minimum-SNR AC-3 controls.

The independent reader follows §5.3.2 and §5.3.3 in bitstream order. It
evaluates the zero-mantissa-width domain of §7.2.2 and rejects inputs outside
that domain. A/52 Annex E and the general bit-allocation domain need additional
oracles.
"""
from dataclasses import dataclass

# A/52 Tables 5.8 and 5.18.
CHANNELS = (2, 1, 2, 3, 3, 4, 4, 5)
RATES_KBPS = (32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256,
              320, 384, 448, 512, 576, 640)


class Bits:
    def __init__(self, data):
        self.bits = "".join(f"{byte:08b}" for byte in data)
        self.at = 0

    def take(self, length):
        end = self.at + length
        if end > len(self.bits):
            raise ValueError("incomplete AC-3 audio block")
        result = int(self.bits[self.at:end], 2) if length else 0
        self.at = end
        return result

    def optional(self, width):
        present = self.take(1)
        if present:
            self.take(width)
        return bool(present)


@dataclass
class Coupling:
    participants: tuple
    first: int
    last: int
    bands: int
    phase_present: bool


def header(bits):
    if bits.take(16) != 0x0B77:
        raise ValueError("AC-3 syncword")
    bits.take(16 + 8)  # CRC1 and the rate/size codes
    if bits.take(5) != 8:
        raise ValueError("oracle requires main-body AC-3")
    bits.take(3)
    mode = bits.take(3)
    bits.take(2 * ((mode > 1 and mode % 2 != 0) + (mode >= 4) + (mode == 2)))
    lfe = bool(bits.take(1))
    for _ in range(2 if mode == 0 else 1):
        bits.take(5)
        for width in (8, 8, 7):
            bits.optional(width)
    bits.take(2)
    bits.optional(14)
    bits.optional(14)
    if bits.take(1):
        length = bits.take(6) + 1
        bits.take(8 * length)
    return mode, lfe


def coupling_strategy(bits, mode, previous):
    if not bits.take(1):
        return previous
    if not bits.take(1):
        return None
    participants = tuple(bool(bits.take(1)) for _ in range(CHANNELS[mode]))
    phase_present = mode == 2 and bool(bits.take(1))
    start, end = bits.take(4), bits.take(4) + 3
    if end <= start or sum(participants) < 2:
        raise ValueError("AC-3 coupling extent or participants")
    joined = sum(bits.take(1) for _ in range(end - start - 1))
    return Coupling(participants, 37 + start * 12, 37 + end * 12,
                    end - start - joined, phase_present)


def read_exponents(bits, strategy, extent, coupled=False):
    repeat = 1 << (strategy - 1)
    absolute = bits.take(4) * (2 if coupled else 1)
    if not 15 <= absolute <= 24:
        raise ValueError("oracle requires high exponents")
    groups = ((extent if coupled else extent - 1) + 3 * repeat - 1) // (3 * repeat)
    for _ in range(groups):
        word = bits.take(7)
        if word >= 125:
            raise ValueError("reserved differential exponent triplet")
        for digit in (word // 25, word // 5 % 5, word % 5):
            absolute += digit - 2
            if not 15 <= absolute <= 24:
                raise ValueError("oracle requires high exponents")


def rows(frame, offset):
    bits = Bits(frame[:-2])
    mode, lfe = header(bits)
    channels = CHANNELS[mode]
    state = [False] * 16
    coupling = None
    for block in range(6):
        state[3] |= bool(bits.take(channels))
        state[4] |= bool(bits.take(channels))
        state[6] |= bits.optional(8)
        if mode == 0:
            state[6] |= bits.optional(8)
        coupling = coupling_strategy(bits, mode, coupling)
        if coupling is not None:
            coordinates = [bits.optional(2 + 8 * coupling.bands)
                           for present in coupling.participants if present]
            if coupling.phase_present and any(coordinates):
                bits.take(coupling.bands)
                state[1] = True
            state[0] = True

        if mode == 2 and bits.take(1):
            boundary = coupling.first if coupling else 253
            state[2] |= bool(bits.take(sum(edge <= boundary for edge in (13, 25, 49, 73))))

        cpl_strategy = bits.take(2) if coupling else 0
        strategies = [bits.take(2) for _ in range(channels)]
        lfe_strategy = bits.take(1) if lfe else 0
        if block == 0 and (0 in strategies or (lfe and not lfe_strategy)
                           or (coupling and not cpl_strategy)):
            raise ValueError("first AC-3 block reuses exponents")
        extents = []
        for index, strategy in enumerate(strategies):
            if strategy:
                extent = (coupling.first if coupling and coupling.participants[index]
                          else 73 + bits.take(6) * 3)
                extents.append((strategy, extent))
        if cpl_strategy:
            read_exponents(bits, cpl_strategy, coupling.last - coupling.first, coupled=True)
        for strategy, extent in extents:
            read_exponents(bits, strategy, extent)
            bits.take(2)
        if lfe_strategy:
            read_exponents(bits, 1, 7)

        bits.optional(11)  # allocation parameters
        if bits.take(1):
            coarse = bits.take(6)
            fine = [bits.take(4 + 3) >> 3
                    for _ in range(channels + int(bool(coupling)) + int(lfe))]
            if coarse or any(fine):
                raise ValueError("oracle requires minimum SNR offsets")
        elif block == 0:
            raise ValueError("first AC-3 block reuses SNR offsets")
        if coupling:
            bits.optional(6)  # initial coupling leaks
        if bits.take(1):
            state[5] = True
            modes = [bits.take(2) for _ in range(channels + int(bool(coupling)))]
            if 3 in modes or (block == 0 and 0 in modes):
                raise ValueError("invalid AC-3 delta allocation mode")
            for strategy in modes:
                if strategy == 1:
                    segments = bits.take(3) + 1
                    bits.take(12 * segments)
        if bits.take(1):
            state[13] = True
            bits.take(bits.take(9) * 8)
        # §7.2.2.2 gives PSD <= 1152 at exponent >= 15. The minimum
        # hearing threshold is 752 (Table 7.15), delta can subtract at
        # most 512, the SNR adjustment adds 960, and rounding the mask
        # down costs at most 31. This leaves every address at zero.
        # No grouped or ungrouped mantissa bits follow in this domain.
    return "4 " + " ".join(str(x) for x in (offset, *(int(flag) for flag in state)))


def trace(path):
    data = path.read_bytes()
    result = []
    offset = 0
    while offset < len(data):
        if len(data) - offset < 8:
            raise ValueError("incomplete AC-3 header")
        fscod, code = data[offset + 4] >> 6, data[offset + 4] & 63
        if fscod == 3 or code > 37:
            raise ValueError("invalid AC-3 header extent")
        rate = RATES_KBPS[code // 2]
        words = (2 * rate, 320 * rate // 147 + code % 2, 3 * rate)[fscod]
        end = offset + 2 * words
        if end > len(data):
            raise ValueError("incomplete AC-3 syncframe")
        result.append(rows(data[offset:end], offset))
        offset = end
    return result
