"""Independent byte positions, ranges and ADTS lengths for the reader protocol."""


def trace(path, mode):
    source = path.read_bytes()
    if len(source) < 9:
        raise ValueError("a control needs at least nine source bytes")
    if mode == "cursor":
        return [
            f"1 2 3 6 8 {source[:2].hex()}",
            f"2 1 3 {source[1:3].hex()}",
            f"3 3 6 {source[3:6].hex()}",
            f"4 1 {len(source)} 2 0 {source[-1:].hex()}",
        ]
    if mode == "feed":
        rows = []
        for maximum in (1, 8, 64, 8192):
            rows += [
                f"5 {maximum} 1 {len(source)} 1 {source.hex()}",
                f"6 {maximum} 2 {source[2:2 + maximum].hex()}",
            ]
        return rows
    if mode != "blocking":
        raise ValueError("unknown reader protocol")
    # ISO/IEC 14496-3 §1.A.2.2: ADTS frame length includes the header.
    packets = []
    position = samples = 0
    while position < len(source):
        header = source[position:position + 7]
        if len(header) != 7 or header[0] != 0xff or header[1] & 0xf6 != 0xf0:
            raise ValueError("the control needs consecutive complete ADTS frames")
        length = (header[3] & 3) << 11 | header[4] << 3 | header[5] >> 5
        if length <= 7 or position + length > len(source):
            raise ValueError("the control needs a full ADTS payload")
        duration = 1024 * (1 + (header[6] & 3))
        packets.append((samples, duration, source[position:position + length].hex()))
        samples += duration
        position += length
    rows = []
    for random_access in (0, 1):
        rows.extend(f"7 {random_access} 0 {start} {duration} {data}" for start, duration, data in packets)
        rows.append(f"8 {random_access} 1 {len(source)}")
        if random_access:
            rows.append(f"10 0 0 {packets[0][2]}")
    return rows + ["9 0"]
