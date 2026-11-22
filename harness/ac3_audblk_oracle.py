"""A/52:2018 main-body AC-3 audio-block trace.

The reader follows §5.3.2 and §5.3.3 in bitstream order. It uses §7.2.2 to
locate the following audio block without reconstructing PCM. Annex E uses
different block and mantissa syntax.
"""
from bisect import bisect_right
from collections import Counter
from dataclasses import dataclass
from itertools import accumulate

# A/52 Tables 5.8 and 5.18.
CHANNELS = (2, 1, 2, 3, 3, 4, 4, 5)
RATES_KBPS = (32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256,
              320, 384, 448, 512, 576, 640)

# A/52 Tables 7.6–7.12, 7.14–7.17. Table 7.13 follows from the band widths.
SLOW_DECAY = (15, 17, 19, 21)
FAST_DECAY = (63, 83, 103, 123)
SLOW_GAIN = (1344, 1240, 1144, 1040)
KNEE = (0, 1792, 2304, 2816)
FLOOR = (752, 688, 624, 560, 496, 368, 240, -2048)
BAND_ENDS = tuple(accumulate([1] * 28 + [3] * 7 + [6] * 6 + [12] * 4 + [24] * 5))
LOG_ADD = (
    64, 63, 62, 61, 60, 59, 58, 57, 56, 55,
    54, 53, 52, 52, 51, 50, 49, 48, 47, 47,
    46, 45, 44, 44, 43, 42, 41, 41, 40, 39,
    38, 38, 37, 36, 36, 35, 35, 34, 33, 33,
    32, 32, 31, 30, 30, 29, 29, 28, 28, 27,
    27, 26, 26, 25, 25, 24, 24, 23, 23, 22,
    22, 21, 21, 21, 20, 20, 19, 19, 19, 18,
    18, 18, 17, 17, 17, 16, 16, 16, 15, 15,
    15, 14, 14, 14, 13, 13, 13, 13, 12, 12,
    12, 12, 11, 11, 11, 11, 10, 10, 10, 10,
    10, 9, 9, 9, 9, 9, 8, 8, 8, 8,
    8, 8, 7, 7, 7, 7, 7, 7, 6, 6,
    6, 6, 6, 6, 6, 6, 5, 5, 5, 5,
    5, 5, 5, 5, 4, 4, 4, 4, 4, 4,
    4, 4, 4, 4, 4, 3, 3, 3, 3, 3,
) + (3,) * 9 + (2,) * 19 + (1,) * 32 + (0,) * 46
HEARING = tuple(tuple(int(value, 16) << 4 for value in line.split()) for line in (
    "4d 4f 58", "4d 4f 58", "44 46 4b", "40 41 45", "3e 3e 42",
    "3c 3d 3f", "3b 3c 3e", "3b 3b 3d", "3a 3b 3c", "3a 3a 3b",
    "3a 3a 3b", "3a 3a 3b", "3a 3a 3a", "39 3a 3a", "39 39 3a",
    "39 39 3a", "38 39 3a", "38 38 3a", "37 38 3a", "37 38 3a",
    "36 37 39", "36 37 39", "35 36 39", "35 36 39", "34 35 38",
    "34 35 38", "33 34 38", "32 34 37", "31 32 36", "30 31 35",
    "2f 30 34", "2f 2f 33", "2f 2f 32", "2f 2f 31", "30 2f 30",
    "31 30 2f", "34 32 2f", "39 35 2f", "3e 39 30", "42 3e 31",
    "46 42 33", "49 45 35", "4a 4a 3c", "46 49 41", "44 46 47",
    "44 44 4a", "52 48 46", "80 63 44", "84 84 45", "84 84 4e",
))
BAP_BREAKS = (1, 6, 8, 11, 13, 15, 19, 23, 27, 31, 35, 39, 43, 47, 55)
MANTISSA_BITS = (0, 0, 0, 3, 0, 4, 5, 6, 7, 8, 9, 10, 11, 12, 14, 16)


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
    bits.take(16)  # CRC1
    rate = bits.take(2)
    bits.take(6)
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
    return mode, lfe, rate


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
    values = [] if coupled else [absolute]
    groups = ((extent if coupled else extent - 1) + 3 * repeat - 1) // (3 * repeat)
    for _ in range(groups):
        word = bits.take(7)
        if word >= 125:
            raise ValueError("reserved differential exponent triplet")
        for digit in (word // 25, word // 5 % 5, word % 5):
            absolute += digit - 2
            if not 0 <= absolute <= 24:
                raise ValueError("invalid AC-3 exponent")
            values.extend([absolute] * repeat)
    return values[:extent]


def low_compensation(previous, current, following, band):
    if band >= 20:
        return max(0, previous - 128)
    if current + 256 == following:
        return 384 if band < 7 else 320
    return max(0, previous - 64) if current > following else previous


def excitation(bands, first, slow_decay, fast_decay, slow_gain, fast_gain, leaks):
    result = []
    low = 0
    grounded = first != 0
    fast, slow = ((768 + (code << 8) for code in leaks) if grounded else (0, 0))
    for index, psd in enumerate(bands):
        band = first + index
        has_next = index + 1 < len(bands)
        if first == 0 and band < 22 and has_next:
            low = low_compensation(low, psd, bands[index + 1], band)
        if not grounded and band < 7:
            fast, slow = psd - fast_gain, psd - slow_gain
            result.append(fast - low)
            grounded = band >= 2 and has_next and psd <= bands[index + 1]
        else:
            fast = max(fast - fast_decay, psd - fast_gain)
            slow = max(slow - slow_decay, psd - slow_gain)
            result.append(max(fast - (low if first == 0 and band < 22 else 0), slow))
        if band == 6:
            grounded = True
    return result


def pointers(exponents, start, rate, codes, coarse, fine, gain, leaks, segments):
    slow_decay, fast_decay, slow_gain, knee, floor = (
        table[code] for table, code in zip((SLOW_DECAY, FAST_DECAY, SLOW_GAIN, KNEE, FLOOR), codes)
    )
    psds = [3072 - (128 * value) for value in exponents]
    start_band = bisect_right(BAND_ENDS, start)
    last_band = bisect_right(BAND_ENDS, start + len(psds) - 1)
    bins = [psds[max(0, (BAND_ENDS[band - 1] if band else 0) - start):BAND_ENDS[band] - start]
            for band in range(start_band, last_band + 1)]
    powers = []
    for values in bins:
        integrated = values[0]
        for psd in values[1:]:
            index = min(abs(integrated - psd) // 2, 255)
            integrated = max(integrated, psd) + LOG_ADD[index]
        powers.append(integrated)
    excite = excitation(powers, start_band, slow_decay, fast_decay, slow_gain,
                        128 * (gain + 1), leaks)
    delta = [0] * len(BAND_ENDS)
    band = start_band
    for distance, length, code in segments:
        band += distance
        if band + length > len(delta):
            raise ValueError("delta allocation segment outside the critical bands")
        value = 128 * (code - (3 if code >= 4 else 4))
        delta[band:band + length] = [value] * length
        band += length
    snr = ((coarse - 15) * 16 + fine) * 4
    for index, psd_band in enumerate(bins):
        band = start_band + index
        masked = max(excite[index] + max(0, knee - powers[index]) // 4, HEARING[band][rate])
        masked += delta[band]
        threshold = (max(0, masked - snr - floor) & 0x1FE0) + floor
        yield from (bisect_right(BAP_BREAKS, min(63, max(0, (value - threshold) // 32)))
                    for value in psd_band)


def mantissa_extent(counts):
    return (sum(counts[bap] * width for bap, width in enumerate(MANTISSA_BITS))
            + ((counts[1] + 2) // 3) * 5 + ((counts[2] + 2) // 3) * 7 + ((counts[4] + 1) // 2) * 7)


def rows(frame, offset):
    bits = Bits(frame[:-2])
    mode, lfe, rate = header(bits)
    channels = CHANNELS[mode]
    state = [False] * 16
    coupling = None
    cpl, lfe_channel = channels, channels + 1
    exponents = {}
    codes = coarse = leaks = None
    snr = {}
    deltas = {}
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
        extents = {}
        for index, strategy in enumerate(strategies):
            if strategy:
                extent = (coupling.first if coupling and coupling.participants[index]
                          else 73 + bits.take(6) * 3)
                if extent > 253:
                    raise ValueError("reserved AC-3 channel bandwidth")
                extents[index] = (strategy, extent)
        if cpl_strategy:
            exponents[cpl] = read_exponents(bits, cpl_strategy, coupling.last - coupling.first,
                                            coupled=True)
        for index, (strategy, extent) in extents.items():
            exponents[index] = read_exponents(bits, strategy, extent)
            bits.take(2)
        if lfe_strategy:
            exponents[lfe_channel] = read_exponents(bits, 1, 7)

        ordered = ([cpl] if coupling else []) + list(range(channels))
        allocation_channels = ordered + ([lfe_channel] if lfe else [])
        if bits.take(1):
            codes = tuple(bits.take(width) for width in (2, 2, 2, 2, 3))
        if bits.take(1):
            coarse = bits.take(6)
            for channel in allocation_channels:
                snr[channel] = bits.take(4), bits.take(3)
        if coupling and bits.take(1):
            leaks = bits.take(3), bits.take(3)
        if bits.take(1):
            state[5] = True
            modes = {channel: bits.take(2) for channel in ordered}
            for channel, strategy in modes.items():
                if strategy == 1:
                    deltas[channel] = [(bits.take(5), bits.take(4), bits.take(3))
                                       for _ in range(bits.take(3) + 1)]
                elif strategy == 2:
                    deltas.pop(channel, None)
                elif strategy == 3 or block == 0:
                    raise ValueError("invalid AC-3 delta allocation mode")
        if bits.take(1):
            state[13] = True
            bits.take(bits.take(9) * 8)
        if codes is None or coarse is None or any(channel not in snr for channel in allocation_channels):
            raise ValueError("AC-3 block has no bit allocation parameters")
        # §7.2.2.1.1 defines all-zero SNR fields as zero allocation.
        if coarse or any(snr[channel][0] for channel in allocation_channels):
            counts = Counter()
            for channel in allocation_channels:
                start = coupling.first if channel == cpl else 0
                fine, gain = snr[channel]
                counts.update(pointers(exponents[channel], start, rate, codes, coarse, fine,
                                       gain, leaks, deltas.get(channel, ())))
            bits.take(mantissa_extent(counts))
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
