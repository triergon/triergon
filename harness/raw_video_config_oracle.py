"""Raw SPS and active parameter-set observations through the pinned decoder interface."""

from pathlib import Path
import re
import subprocess

from .ts_packet_oracle import row

REFERENCE = ".research/reference-decode/install/bin/ffmpeg"
START = re.compile(rb"\x00\x00+\x01")
TRACE = re.compile(r"^\[trace_headers @ [^\]]+\] (.*)$")
FIELD = re.compile(r"^(\d+)\s+(\S+)\s+([01]+)\s+=\s+(-?\d+)$")
PARAMETER_SETS = {
    "Video Parameter Set", "Sequence Parameter Set", "Picture Parameter Set",
}
SLICE = {"Slice Header", "Slice Segment Header"}


def nal_units(data):
    positions = list(START.finditer(data))
    return [
        data[start.end():end.start() if end else len(data)].rstrip(b"\x00")
        for start, end in zip(positions, [*positions[1:], None])
    ]


def decode_sections(path, codec):
    result = subprocess.run(
        [REFERENCE, "-hide_banner", "-loglevel", "info", "-f", codec, "-i", str(path),
         "-c:v", "copy", "-bsf:v", "trace_headers", "-f", "null", "-"],
        stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True, check=True,
    )
    in_packet = False
    section, fields = None, {}
    for line in result.stderr.splitlines():
        match = TRACE.match(line)
        if not match:
            continue
        data = match[1]
        if data.startswith("Packet:"):
            in_packet = True
        if not in_packet:
            continue
        field = FIELD.match(data)
        if field:
            if section:
                fields[field[2]] = (int(field[1]), field[3], int(field[4]))
            continue
        if section:
            yield section, fields
        section = data if data in PARAMETER_SETS | SLICE else None
        fields = {}
    if section:
        yield section, fields


def records(path, codec):
    hevc = codec == "hevc"
    types = (32, 33, 34) if hevc else (None, 7, 8)
    names = ("Video Parameter Set", "Sequence Parameter Set", "Picture Parameter Set")
    units = {name: iter([nal for nal in nal_units(path.read_bytes()) if nal and (
        (nal[0] >> 1) & 63 if hevc else nal[0] & 31
    ) == code]) for name, code in zip(names, types)}
    for name, fields in decode_sections(path, codec):
        values = {k: v[2] for k, v in fields.items()}
        if name in units:
            yield name, values, next(units[name])
        else:
            yield name, values, None


def geometry(sps, hevc):
    chroma = sps.get("chroma_format_idc", 1)
    if sps.get("separate_colour_plane_flag", 0):
        chroma = 0
    sub_x, sub_y = [(1, 1), (2, 2), (2, 1), (1, 1)][chroma]
    if hevc:
        width, height = sps["pic_width_in_luma_samples"], sps["pic_height_in_luma_samples"]
        crop = "conf_win_"
    else:
        frames = 2 - sps["frame_mbs_only_flag"]
        width = (1 + sps["pic_width_in_mbs_minus1"]) * 16
        height = (1 + sps["pic_height_in_map_units_minus1"]) * 16 * frames
        sub_y *= frames
        crop = "frame_crop_"
    width -= sub_x * (sps.get(f"{crop}left_offset", 0) + sps.get(f"{crop}right_offset", 0))
    height -= sub_y * (sps.get(f"{crop}top_offset", 0) + sps.get(f"{crop}bottom_offset", 0))
    return width, height


def timing(sps, hevc):
    if hevc and sps.get("vui_timing_info_present_flag", 0):
        flag = sps["vui_poc_proportional_to_timing_flag"]
        return (1, sps["vui_num_units_in_tick"], sps["vui_time_scale"], flag,
                1 + sps["vui_num_ticks_poc_diff_one_minus1"] if flag else 0)
    if not hevc and sps.get("timing_info_present_flag", 0):
        return 1, sps["num_units_in_tick"], sps["time_scale"], sps["fixed_frame_rate_flag"], 0
    return (0,) * 5


def trace(path, opener, mode):
    codec = {"raw-avc": "h264", "raw-hevc": "hevc"}[opener]
    if mode != "configuration":
        raise ValueError("expected raw configuration projection")
    hevc = codec == "hevc"
    if hevc:
        vps_id, sps_id, pps_id = ("vps_video_parameter_set_id", "sps_seq_parameter_set_id",
                                  "pps_pic_parameter_set_id")
        sps_vps, pps_sps, slice_pps = ("sps_video_parameter_set_id", "pps_seq_parameter_set_id",
                                       "slice_pic_parameter_set_id")
    else:
        sps_id, pps_id = "seq_parameter_set_id", "pic_parameter_set_id"
        pps_sps, slice_pps = sps_id, pps_id
    vps, sps, pps = {}, {}, {}
    rows, previous, revision = [], None, 0
    for kind, fields, nal in records(path, codec):
        if kind == "Video Parameter Set":
            vps[fields[vps_id]] = nal
        elif kind == "Sequence Parameter Set":
            sps[fields[sps_id]] = fields, nal
        elif kind == "Picture Parameter Set":
            pps[fields[pps_id]] = fields[pps_sps], nal
        elif fields.get("first_slice_segment_in_pic_flag" if hevc else "first_mb_in_slice") == int(hevc):
            selected = fields[slice_pps]
            seq, picture = pps[selected]
            facts, sequence = sps[seq]
            sets = ((vps[facts[sps_vps]],) if hevc else ()) + (sequence, picture)
            if sets == previous:
                continue
            revision += 1
            previous = sets
            rows.append(row(4, int(hevc), revision, *geometry(facts, hevc), *timing(facts, hevc)))
            for i, body in enumerate(sets):
                rows += [row(5, i), body.hex()]
            if hevc:
                suffix = facts["sps_max_sub_layers_minus1"]
                rows.append(row(6, selected, facts[f"sps_max_num_reorder_pics[{suffix}]"],
                                int(16 <= fields["nal_unit_type"] <= 21)))
    if not rows:
        raise ValueError("the reference exposed no active configuration")
    return rows + ["7"]
