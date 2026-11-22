"""Carrier and selected-program control records from ITU-T H.222.0 (08/2023)."""

from .ts_descriptor_oracle import valid_crc
from .ts_packet_oracle import carrier, row


def u16(data):
    return int.from_bytes(data[:2], "big")


def section(payload):
    start = 1 + payload[0]
    header = payload[start:start + 3]
    length = u16(header[1:]) & 0x0FFF
    body = payload[start:start + 3 + length]
    if len(body) < 12 or len(body) != 3 + length or not valid_crc(body):
        raise ValueError("the control needs a complete valid PSI section")
    return body


def program_map(data):
    pcr = u16(data[8:]) & 0x1FFF
    position = 12 + (u16(data[10:]) & 0x0FFF)
    entries = []
    end = len(data) - 4
    while position < end:
        entries.append((data[position], u16(data[position + 1:]) & 0x1FFF))
        position += 5 + (u16(data[position + 3:]) & 0x0FFF)
    if position != end:
        raise ValueError("the control needs a complete PMT stream loop")
    return pcr, entries


def trace(path, width):
    width = int(width)
    data = path.read_bytes()
    if width not in (188, 192) or len(data) % width:
        raise ValueError("expected complete 188- or 192-byte carriers")
    rows, counts, pes, elementary = [], {}, {}, {}
    pmt, pcr, pids = None, None, set()
    epoch = 0
    for offset in range(0, len(data), width):
        wire = data[offset:offset + width]
        fields = carrier(wire, offset)[0].split()
        _, _, _, _, _, start, _, pid, _, control, cc, payload_start, length, *_ = map(int, fields)
        rows.append(row(1, 1, offset, pid, cc, start, payload_start, wire.hex()))
        if pid not in {0, pmt, pcr, *pids}:
            continue
        previous = counts.get(pid)
        if previous is not None and cc != (previous + int(bool(control & 1))) % 16:
            raise ValueError("the control needs uninterrupted continuity")
        rows.append(row(2, pid, int(previous is not None)))
        counts[pid] = cc
        payload = data[payload_start:payload_start + length]
        if pid in (0, pmt):
            if not start:
                raise ValueError("the control needs one complete table per PSI carrier")
            table = section(payload)
            rows.append(row(3, pid, 0, table.hex()))
            if pid == 0:
                entries = [
                    (u16(table[i:]), u16(table[i + 2:]) & 0x1FFF)
                    for i in range(8, len(table) - 4, 4)
                ]
                [selected] = [pid for program, pid in entries if program != 0]
                epoch += int(pmt != selected)
                pmt = selected
                rows.append(row(4, u16(table[3:]), *(n for entry in entries for n in entry)))
            else:
                pcr, entries = program_map(table)
                pids = {pid for _, pid in entries}
                epoch += 1
                rows.append(row(5, u16(table[3:]), pcr, 1, epoch,
                                *(n for entry in entries for n in entry), table.hex()))
        elif pid in pids and payload:
            if start:
                if pid in pes or payload[:3] != b"\x00\x00\x01":
                    raise ValueError("the control needs a complete start of each PES")
                pes[pid] = bytearray()
            buffered = pes[pid]
            buffered.extend(payload)
            if len(buffered) >= 6:
                end = 6 + u16(buffered[4:])
                if end == 6 or len(buffered) > end:
                    raise ValueError("the control needs a finite PES extent")
                if len(buffered) == end:
                    header = 9 + buffered[8]
                    rows.append(row(6, pid, buffered[:header].hex()))
                    position = elementary.get(pid, 0)
                    rows.append(row(9, pid, position, buffered[header:].hex()))
                    elementary[pid] = position + end - header
                    del pes[pid]
    if pes:
        raise ValueError("the control ended inside a PES packet")
    return rows + ["7"]
