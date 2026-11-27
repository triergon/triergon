"""PES timestamp fields and bounded clock lifts (ITU-T H.222.0 §2.4.3.7)."""

from fractions import Fraction


PERIOD = 2**33


def timestamp(field):
    encoded = int.from_bytes(field, "big")
    if len(field) != 5 or encoded & 0x100010001 != 0x100010001:
        raise ValueError("invalid PES timestamp markers")
    return ((encoded >> 33) & 7) << 30 | ((encoded >> 17) & 32767) << 15 | (encoded >> 1) & 32767


def pes_clocks(data, pid):
    """Authored TS188 controls put the full PES header in its start carrier."""
    if not data or len(data) % 188:
        raise ValueError("incomplete TS188 control")
    for offset in range(0, len(data), 188):
        carrier = data[offset:offset + 188]
        if carrier[0] != 0x47:
            raise ValueError("missing sync byte")
        if int.from_bytes(carrier[1:3], "big") & 8191 != pid or not carrier[1] & 64:
            continue
        payload = carrier[5 + carrier[4]:] if carrier[3] & 32 else carrier[4:]
        if not payload.startswith(b"\x00\x00\x01") or len(payload) < 9:
            raise ValueError("control does not start with a PES header")
        flags = payload[7] >> 6
        if flags == 1:
            raise ValueError("forbidden PTS/DTS flags")
        yield (
            timestamp(payload[9:14]) if flags >= 2 else None,
            timestamp(payload[14:19]) if flags == 3 else None,
        )


def lift(raw, anchor90, distance27):
    """Bracket the anchor with two congruent PTS values and choose the nearer."""
    if raw is None:
        return "3", "absent", "-", "-", "-"
    lower = raw + ((anchor90 - raw) // PERIOD) * PERIOD
    upper = lower + PERIOD
    before, after = anchor90 - lower, upper - anchor90
    if before == after:
        return "8", "ambiguous", raw, "-", "-"
    ticks = lower if before < after else upper
    if min(before, after) * 300 > distance27:
        return "8", "outside", raw, "-", "-"
    exact = Fraction(ticks, 90000)
    return "3", "anchored", raw, ticks, f"{exact.numerator}/{exact.denominator}"


def trace(path, mode, pid, tsid, program, pcr_pid, epoch, unit, sample, distance):
    if mode not in ("au", "pes") or unit not in ("pts90", "pcr27"):
        raise ValueError("unsupported clock control")
    # These controls all use the declared TSID, program and PCR PID and start
    # in clock epoch zero, with no PMT change or discontinuity.
    identity = tuple(map(int, (tsid, program, pcr_pid, epoch)))
    expected = (1, 1, 256, 0) if mode == "pes" else (1, 1, 257, 0)
    anchor = Fraction(int(sample), 300 if unit == "pcr27" else 1)
    rows = []
    for pair in pes_clocks(path.read_bytes(), int(pid)):
        for field, raw in enumerate(pair):
            if raw is not None and identity != expected:
                kind, status, raw, ticks, exact = "8", "identity", raw, "-", "-"
            else:
                kind, status, raw, ticks, exact = lift(raw, anchor, int(distance))
            rows.append(f"{kind} {pid} {field} {status} {raw} {ticks} {exact}")
    if not rows:
        raise ValueError("clock control has no selected PES")
    return [*rows, "7"]
