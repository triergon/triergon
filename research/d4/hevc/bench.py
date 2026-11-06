#!/usr/bin/env python3
"""Rough development timing (NOT evidence): best-of-N wall time of
`ffmpeg -threads 1 -i X -f null -` vs `examples/decode X /dev/null --quiet` on this machine."""
import os, subprocess, sys, time
ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), '..', '..', '..'))
VEC = os.path.join(ROOT, '.research/m1d4/fate-suite/hevc-conformance')
DEC = os.path.join(ROOT, 'target/agent-hevc/release/examples/decode')
FF = '/opt/homebrew/bin/ffmpeg'
DEFAULT = ['AMP_A_Samsung_4.bit', 'DELTAQP_A_BRCM_4.bit', 'ENTP_C_LG_3.bit', 'TILES_A_Cisco_2.bit',
           'AMP_D_Hisilicon.bit', 'DSLICE_A_HHI_5.bit', 'WPP_A_ericsson_MAIN10_2.bit', 'SAO_C_Samsung_5.bit',
           'IPRED_A_docomo_2.bit', 'PICSIZE_B_Bossen_1.bit']

def best(cmd, n):
    t = []
    for _ in range(n):
        t0 = time.perf_counter(); subprocess.run(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL); t.append(time.perf_counter() - t0)
    return min(t)

def main():
    names = [a for a in sys.argv[1:] if not a.startswith('-')] or DEFAULT
    n = 3
    tf = to = 0.0
    for name in names:
        p = os.path.join(VEC, name) if not os.path.isabs(name) else name
        f = best([FF, '-v', 'error', '-threads', '1', '-i', p, '-f', 'null', '-'], n)
        o = best([DEC, p, '/dev/null', '--quiet'], n)
        tf += f; to += o
        print(f'{os.path.basename(p):40s} ffmpeg {f:7.3f}s  ours {o:7.3f}s  ratio {o/f:5.2f}', flush=True)
    print(f'{"TOTAL":40s} ffmpeg {tf:7.3f}s  ours {to:7.3f}s  ratio {to/tf:5.2f}')

if __name__ == '__main__':
    main()
