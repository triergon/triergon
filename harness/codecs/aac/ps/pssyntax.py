"""ps_data() payload builders, in the field order and widths the length probes establish
(probes/e07: header flags and 3-bit modes, frame_class, 2-bit envelope index, 5-bit
borders, per-envelope dt flag and one codeword per parameter band, 4-bit extension size
with an 8-bit escape at 15)."""

NBANDS = {0: 10, 1: 20, 2: 34, 3: 10, 4: 20, 5: 34}


def header(iid=None, icc=None, ext=False):
    s = '1'
    s += '0' if iid is None else '1' + format(iid, '03b')
    s += '0' if icc is None else '1' + format(icc, '03b')
    s += '1' if ext else '0'
    return s


def grid(frame_class=0, idx=1, borders=()):
    s = str(frame_class) + format(idx, '02b')
    for b in borders:
        s += format(b, '05b')
    return s
