#!/usr/bin/env python3
"""Untimed peak-RSS pass for an evaluate.py output directory (research tooling).

Per case and configuration, three `load` runs (input resident, no device, no
work) and three `phases` runs, each a fresh process; ru_maxrss comes from the
kernel via wait4 (bytes on macOS, KiB on Linux). Attributable RSS is the
phases peak minus the load-only peak of the same worker, so the resident
input frames are excluded for every configuration alike. Results are added to
<out>/<case>.json under "rss".

  rss.py <same arguments as evaluate.py>
"""
import json
import os
import platform
import statistics
import subprocess
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import evaluate  # noqa: E402


def maxrss(cmd, env):
    p = subprocess.Popen(cmd, env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    _, status, usage = os.wait4(p.pid, 0)
    return status, usage.ru_maxrss * (1 if platform.system() == "Darwin" else 1024)


def main():
    ap = evaluate.parser()
    a = ap.parse_args()
    out = Path(a.out)
    cases = {c["id"]: c for c in json.loads((evaluate.ROOT / "research/capture/cases.json").read_text())["cases"]}
    base_env = dict(os.environ, **json.loads(a.extra_env))
    configs = evaluate.configs_for(a)
    for cid in a.cases:
        c = cases[cid]
        inp = f"{a.inputs}/{c['content']}-{c['width']}x{c['height']}-{c['frames']}.rgba"
        geo = [str(c["width"]), str(c["height"]), str(c["flip"]), str(c["frames"]), str(c["bitrate"]), inp]
        path = out / f"{cid}.json"
        rec = json.loads(path.read_text())
        rec["rss"] = {}
        for name, (cmd, env) in configs.items():
            e = dict(base_env, **env)
            load = [maxrss(cmd + ["load", *geo], e) for _ in range(3)]
            work = [maxrss(cmd + ["phases", *geo, "1"], e) for _ in range(3)]
            ok = [r for s, r in work if s == 0]
            base = statistics.median(r for _, r in load)
            rec["rss"][name] = {"load_maxrss": [r for _, r in load], "phases_maxrss": [r for _, r in work],
                                "phases_exit": [s for s, _ in work],
                                "attributable_bytes": [r - base for r in ok]}
        path.write_text(json.dumps(rec, indent=1))
        print(cid, {k: [round(x / 1e6, 1) for x in v["attributable_bytes"]] for k, v in rec["rss"].items()}, flush=True)


if __name__ == "__main__":
    main()
