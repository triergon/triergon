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


def trace(path, width, mode=None):
    if mode == "boundary":
        return boundary_trace(path, int(width))
    if mode is not None:
        raise ValueError("unknown transport control")
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


def payload_carriers(data, width):
    """Return the physical carriers, independent of assembly and reader policy."""
    if width != 188 or len(data) % width:
        raise ValueError("expected complete TS188 boundary carriers")
    for offset in range(0, len(data), width):
        wire = data[offset:offset + width]
        fields = carrier(wire, offset)[0].split()
        _, _, _, _, _, start, _, pid, _, control, cc, first, size, *_ = map(int, fields)
        flags = wire[5] if control & 2 and wire[4] else 0
        yield offset, pid, start, cc, flags, first, data[first:first + size]


def split_pes(packet):
    if packet[:3] != b"\x00\x00\x01" or 6 + u16(packet[4:]) != len(packet):
        raise ValueError("expected one complete finite PES")
    start = 9 + packet[8]
    return packet[:start], packet[start:]


def boundary_trace(path, width):
    """Compare source interruptions, resulting packets and typed refusal in order."""
    pat, pmt, first, *rest = payload_carriers(path.read_bytes(), width)
    table = section(pat[-1])
    program, pmt_pid = u16(table[8:]), u16(table[10:]) & 0x1fff
    if pmt[1] != pmt_pid or not first[2]:
        raise ValueError("missing first PMT or PES start")
    pcr, [(stream_type, pid)] = program_map(section(pmt[-1]))
    if first[1] != pid or pcr != pid:
        raise ValueError("the boundary needs one elementary PID carrying PCR")
    if stream_type == 0x24:
        _, data = split_pes(first[-1])
        # H.265 Table 7-1: NAL unit type 35 is an access unit delimiter.
        if rest or len(data) != 7 or data[:4] != b"\x00\x00\x00\x01" or data[4] >> 1 != 35:
            raise ValueError("expected just one HEVC AUD")
        return [row(16, pid, 0, len(data), 0, data.hex()), "7"]
    if stream_type == 0x8e:
        header, payload = split_pes(first[-1])
        if rest or header[3] != 0xbd or not payload:
            raise ValueError("expected one opaque private PES")
        return [row(17, pid, stream_type), row(19, pid, 1, first[-2], header.hex()),
                row(20, payload.hex()), "7"]
    if stream_type != 0x0f:
        raise ValueError("expected the ADTS stream for interruption")
    if len(rest) == 1:
        _, interrupted = split_pes(first[-1])
        offset, resumed_pid, pusi, actual, flags, _, continuation = rest[0]
        _, resumed = split_pes(continuation)
        if len(interrupted) >= 7 or not pusi or resumed_pid != pid:
            raise ValueError("expected an interrupted ADTS header and a clean start")
        expected = (first[3] + 1) % 16
        if flags & 0x90 == 0x90 and actual == expected:
            controls = [row(13, pid, 0, len(interrupted), program, 1), row(10, program, 1)]
            clock_epoch = 1
        elif actual != expected and not flags & 0x80:
            controls = [
                row(2, pid, 5, expected, actual),
                row(12, pid, 0, len(interrupted), pid, 1, offset),
                row(11, pid, 1, offset),
            ]
            clock_epoch = 0
        else:
            raise ValueError("expected a PCR transition or unexplained continuity loss")
        # Loss and PCR epoch changes restart the elementary run. The old
        # incomplete range above is relative to the discarded run.
        return controls + [row(18, pid, 0, len(resumed), clock_epoch, resumed.hex()), "7"]

    updated, *continuation, resumed = rest
    rebound = section(updated[-1])
    pcr, streams = program_map(rebound)
    version = (rebound[5] >> 1) & 31
    if (updated[1] != pmt_pid or pcr == pid or streams != [(stream_type, pid)]
            or not rebound[5] & 1 or not continuation or any(part[2] for part in continuation)
            or any(part[1] != pid for part in [*continuation, resumed]) or not resumed[2]):
        raise ValueError("expected a PCR PID rebind across a single PES")
    header, interrupted = split_pes(first[-1] + b"".join(part[-1] for part in continuation))
    _, data = split_pes(resumed[-1])
    return [row(14, pid, 1, first[-2], 0, 2, 0, 3, pcr, version, header.hex()),
            row(15, interrupted.hex()), row(18, pid, 0, len(data), 0, data.hex()), "7"]
