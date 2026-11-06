#!/usr/bin/env python3
"""Render the per-vector result table for STATUS.md from a compare.py run log.

usage: python3 -I status_table.py <compare-output.txt> <profiles.txt>
profiles.txt: "<name> <general_profile_idc>" lines.
"""
import sys

OUT = {
    'ADJUST_IPRED_ANGLE_A_RExt_Mitsubishi_1.bit': 'RExt 4:2:2',
    'IPCM_A_RExt_NEC.bit': 'RExt 4:2:2',
    'IPCM_B_RExt_NEC.bit': 'RExt 4:4:4',
    'Main_422_10_A_RExt_Sony_1.bin': 'RExt 4:2:2 10-bit',
    'Main_422_10_B_RExt_Sony_1.bin': 'RExt 4:2:2 10-bit',
    'PERSIST_RPARAM_A_RExt_Sony_1.bit': 'RExt 4:4:4, persistent Rice adaptation',
    'PERSIST_RPARAM_A_RExt_Sony_3.bit': 'RExt 4:4:4, persistent Rice adaptation',
    'QMATRIX_A_RExt_Sony_1.bit': 'RExt 4:4:4 scaling lists',
    'SAO_A_RExt_MediaTek_1.bit': 'RExt 4:4:4 SAO offset scaling',
    'WPP_HIGH_TP_444_8BIT_RExt_Apple_2.bit': 'RExt High Throughput 4:4:4 tools',
}
FF_REFUSES = {
    'EXT_A_ericsson_3.bit': 'SPS uses pre-v2 extension syntax; FFmpeg: "Overread SPS", no output',
    'TSUNEQBD_A_MAIN10_Technicolor_2.bit': 'luma 10 / chroma 9 bit; FFmpeg: "different luma and chroma bit depth unsupported", no output',
}
PROF = {'0': '0 (pre-final)', '1': 'Main', '2': 'Main 10', '3': 'MSP', '4': 'RExt', '5': 'HT 4:4:4'}

def main():
    res = {}
    for line in open(sys.argv[1]):
        if ':' not in line:
            continue
        n, rest = line.split(':', 1)
        res[n.strip()] = rest.strip()
    prof = dict(l.split() for l in open(sys.argv[2]) if l.strip())
    print('| vector | profile_idc | class | result |')
    print('|---|---|---|---|')
    for n in sorted(res):
        r = res[n]
        p = PROF.get(prof.get(n, '?'), prof.get(n, '?'))
        if n in OUT:
            cls = 'out-of-set: ' + OUT[n]
            refused = 'unsupported' in r
            result = 'refused with typed Unsupported (FFmpeg decodes)' if refused else 'UNEXPECTED: ' + r
        elif n in FF_REFUSES:
            cls = 'FFmpeg-refused'
            result = 'no output, matches FFmpeg (' + FF_REFUSES[n] + ')'
        else:
            cls = 'selected'
            result = r.replace('|', '/')
        print(f'| {n} | {p} | {cls} | {result} |')

if __name__ == '__main__':
    main()
