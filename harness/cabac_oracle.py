"""Serial arithmetic decoder and interval encoder for scripted CABAC bins.

Tables from ITU-T H.264 (08/2024), 9-44 and 9-45. The equivalent
decoding processes appear in ITU-T H.265 clause 9.3.4.3.
"""
from copy import copy
import struct


LPS_RANGE = (
    (128, 176, 208, 240),
    (128, 167, 197, 227),
    (128, 158, 187, 216),
    (123, 150, 178, 205),
    (116, 142, 169, 195),
    (111, 135, 160, 185),
    (105, 128, 152, 175),
    (100, 122, 144, 166),
    (95, 116, 137, 158),
    (90, 110, 130, 150),
    (85, 104, 123, 142),
    (81, 99, 117, 135),
    (77, 94, 111, 128),
    (73, 89, 105, 122),
    (69, 85, 100, 116),
    (66, 80, 95, 110),
    (62, 76, 90, 104),
    (59, 72, 86, 99),
    (56, 69, 81, 94),
    (53, 65, 77, 89),
    (51, 62, 73, 85),
    (48, 59, 69, 80),
    (46, 56, 66, 76),
    (43, 53, 63, 72),
    (41, 50, 59, 69),
    (39, 48, 56, 65),
    (37, 45, 54, 62),
    (35, 43, 51, 59),
    (33, 41, 48, 56),
    (32, 39, 46, 53),
    (30, 37, 43, 50),
    (29, 35, 41, 48),
    (27, 33, 39, 45),
    (26, 31, 37, 43),
    (24, 30, 35, 41),
    (23, 28, 33, 39),
    (22, 27, 32, 37),
    (21, 26, 30, 35),
    (20, 24, 29, 33),
    (19, 23, 27, 31),
    (18, 22, 26, 30),
    (17, 21, 25, 28),
    (16, 20, 23, 27),
    (15, 19, 22, 25),
    (14, 18, 21, 24),
    (14, 17, 20, 23),
    (13, 16, 19, 22),
    (12, 15, 18, 21),
    (12, 14, 17, 20),
    (11, 14, 16, 19),
    (11, 13, 15, 18),
    (10, 12, 15, 17),
    (10, 12, 14, 16),
    (9, 11, 13, 15),
    (9, 11, 12, 14),
    (8, 10, 12, 14),
    (8, 9, 11, 13),
    (7, 9, 11, 12),
    (7, 9, 10, 12),
    (7, 8, 10, 11),
    (6, 8, 9, 11),
    (6, 7, 9, 10),
    (6, 7, 8, 9),
    (2, 2, 2, 2),
)
LPS_STATE = (
    0, 0, 1, 2, 2, 4, 4, 5, 6, 7, 8, 9, 9, 11, 11, 12,
    13, 13, 15, 15, 16, 16, 18, 18, 19, 19, 21, 21, 22, 22, 23, 24,
    24, 25, 26, 26, 27, 27, 28, 29, 29, 30, 30, 30, 31, 32, 32, 33,
    33, 33, 34, 34, 35, 35, 35, 36, 36, 36, 37, 37, 37, 38, 38, 63,
)


def initialized(m, n, qp):
    pre = min(126, max(1, m * min(51, max(0, qp)) // 16 + n))
    return (63 - pre, 0) if pre < 64 else (pre - 64, 1)


def partition(span, context):
    probability, most_likely = context
    unlikely_span = LPS_RANGE[probability][span // 64 - 4]
    return span - unlikely_span, unlikely_span, most_likely


def transition(context, bin_value):
    probability, most_likely = context
    if bin_value == most_likely:
        return (min(probability + 1, 62) if probability < 63 else 63, most_likely)
    return LPS_STATE[probability], most_likely ^ (probability == 0)


def packed(context):
    probability, most_likely = context
    return 2 * probability + most_likely


class Decoder:
    def __init__(self, data, start):
        self.data = data
        self.position = start * 8
        self.span = 510
        self.offset = 0
        for _ in range(9):
            self.offset = 2 * self.offset + self.bit()

    def bit(self):
        byte, within = divmod(self.position, 8)
        self.position += 1
        return (self.data[byte] >> (7 - within)) & 1 if byte < len(self.data) else 0

    def renormalize(self):
        while self.span < 256:
            self.span *= 2
            self.offset = 2 * self.offset + self.bit()

    def decision(self, context):
        likely_span, unlikely_span, bin_value = partition(self.span, context)
        if self.offset < likely_span:
            self.span = likely_span
        else:
            self.offset -= likely_span
            self.span = unlikely_span
            bin_value ^= 1
        self.renormalize()
        return bin_value

    def bypass(self):
        self.offset = 2 * self.offset + self.bit()
        if self.offset < self.span:
            return 0
        self.offset -= self.span
        return 1

    def bins(self, count):
        return sum(self.bypass() << shift for shift in reversed(range(count)))

    def terminate(self):
        self.span -= 2
        if self.offset >= self.span:
            return 1
        self.renormalize()
        return 0


class Encoder:
    """Select an endpoint of the final interval on a dyadic grid."""

    def __init__(self):
        self.low, self.span, self.width = 0, 510, 9

    def renormalize(self):
        while self.span < 256:
            self.low *= 2
            self.span *= 2
            self.width += 1

    def decision(self, context, bin_value):
        likely_span, unlikely_span, most_likely = partition(self.span, context)
        if bin_value == most_likely:
            self.span = likely_span
        else:
            self.low += likely_span
            self.span = unlikely_span
        self.renormalize()
        return transition(context, bin_value)

    def bypass(self, bins):
        for bin_value in bins:
            self.low = 2 * self.low + int(bin_value) * self.span
            self.width += 1

    def terminate(self, bin_value):
        self.span -= 2
        if bin_value:
            # An odd point supplies rbsp_stop_one_bit at the logical cursor.
            self.low = (self.low + self.span) | 1
        else:
            self.renormalize()

    def bytes(self):
        padding = -self.width % 8
        return (self.low << padding).to_bytes((self.width + padding) // 8, "big")


class Lookahead:
    """Track physical reads for next_byte independently of arithmetic."""

    def __init__(self, width, headroom):
        self.capacity = 23 - headroom
        self.block = 32 if width == 64 else None
        self.initial = (56 if headroom else 32) if self.block else width - headroom

    def restart(self, start):
        self.loaded = start * 8 + self.initial

    def refill(self, position):
        if self.loaded - position < 16:
            self.loaded = self.loaded + self.block if self.block else position + self.capacity


def trace(path, layout, width=None):
    lines = path.read_text().splitlines()
    data = bytes.fromhex(lines[0])
    lookahead = Lookahead(width or struct.calcsize("P") * 8, layout == "headroom")
    contexts = [(0, 0)] * 128
    decoder = None
    result = []
    for index, instruction in enumerate(lines[1:]):
        operation, *fields = instruction.split()
        operands = list(map(int, fields))
        kind, values = 3, [index]
        payload = ""
        if operation in ("new", "restart"):
            start, = operands
            decoder = Decoder(data, start)
            lookahead.restart(start)
            if decoder.offset >= 510:
                kind = 8
                if operation == "new":
                    decoder = None
        elif operation == "state":
            slot, state = operands
            contexts[slot] = divmod(state, 2)
        elif operation == "init":
            slot, *parameters = operands
            contexts[slot] = initialized(*parameters)
            values.append(packed(contexts[slot]))
        elif operation in ("decision", "unfilled"):
            slot, = operands
            bin_value = decoder.decision(contexts[slot])
            contexts[slot] = transition(contexts[slot], bin_value)
            values.extend((bin_value, packed(contexts[slot])))
            if operation == "decision":
                lookahead.refill(decoder.position)
        elif operation == "refill":
            lookahead.refill(decoder.position)
        elif operation == "bypass":
            values.append(decoder.bypass())
            lookahead.refill(decoder.position)
        elif operation == "bits":
            count, = operands
            value = 0
            for chunk in ((count - 16, 16) if count > 16 else (count,)):
                value = (value << chunk) + decoder.bins(chunk)
                if chunk:
                    lookahead.refill(decoder.position)
            values.append(value)
        elif operation == "peek":
            values.append(copy(decoder).bins(*operands))
        elif operation == "consume":
            decoder.bins(*operands)
        elif operation == "unary":
            limit, = operands
            ones = 0
            while ones < limit:
                if decoder.bypass() == 0:
                    break
                ones += 1
                if ones % 8 == 0:
                    lookahead.refill(decoder.position)
            lookahead.refill(decoder.position)
            values.append(ones)
        elif operation == "terminate":
            before = decoder.position
            values.append(decoder.terminate())
            if decoder.position != before:
                lookahead.refill(decoder.position)
        elif operation == "data":
            values.append(len(data))
            if data:
                payload = " " + data.hex()
        else:
            raise ValueError(f"unknown CABAC script operation: {operation}")
        if decoder is not None:
            position = decoder.position
            values.extend((position, (position + 7) // 8, (lookahead.loaded + 7) // 8))
        result.append(" ".join(map(str, (kind, *values))) + payload)
    return result
